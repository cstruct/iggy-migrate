//! Configuration, loaded with ordna from, in increasing precedence:
//!
//! 1. `iggy-migrate.styx` (or `--config <file>`), optional
//! 2. `iggy-migrate.secrets.styx` next to it (or `--secrets <file>`), optional,
//!    and the only source of secrets: passwords and tokens
//! 3. environment variables, `IGGY_MIGRATE__<PATH>` such as
//!    `IGGY_MIGRATE__LEDGER__URL`
//! 4. command line flags
//!
//! Unknown settings, secrets set from a public source and malformed values are
//! errors, reported with where each value came from.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use ordna_core::{
    ConfigBuilder, ConfigLoadError, ConfigModel, ConfigPath, DiagnosticValue, SourceFieldPath, SourceMeta,
    ValuesSource, config,
};
use ordna_source_env::{EnvSource, EnvVarDetail};
use ordna_source_styx::{StyxSource, styx_file};

pub const ENV_PREFIX: &str = "IGGY_MIGRATE";
pub const DEFAULT_FILE: &str = "iggy-migrate.styx";

#[config]
#[derive(Debug)]
pub struct Config {
    /// Directory containing migration files.
    #[facet(default = "migrations".to_string())]
    pub dir: String,
    /// Environment being migrated, exposed to migrations as `env`.
    #[facet(default = "dev".to_string())]
    pub env: String,
    /// Every known environment, comma separated, so `per_env` can require
    /// all of them. (A string, since ordna cannot read lists from env vars.)
    #[facet(default = "dev,staging,prod".to_string())]
    pub environments: String,
    /// Recorded in the ledger as who applied each migration; defaults to `$USER`.
    pub runner: Option<String>,
    /// Let `up` apply migrations that delete resources.
    #[facet(default)]
    pub allow_deletes: bool,
    #[facet(default)]
    pub iggy: Iggy,
    #[facet(default)]
    pub ledger: Ledger,
    #[facet(default)]
    pub rehearsal: Rehearsal,
}

#[config]
#[derive(Debug)]
#[facet(default)]
pub struct Iggy {
    /// Connection string without the password, such as
    /// `iggy://user@host:8090` or `iggy+quic://user@host:8080`.
    #[facet(default = DEFAULT_IGGY_URL.to_string())]
    pub url: String,
    /// Password for the user in `url`.
    #[config(secret)]
    pub password: Option<String>,
    /// Personal access token, instead of a user and password.
    #[config(secret)]
    pub token: Option<String>,
    /// How long to try reaching the server before giving up, such as `10s`.
    #[facet(default = DEFAULT_CONNECT_TIMEOUT.to_string())]
    pub connect_timeout: String,
}

const DEFAULT_IGGY_URL: &str = "iggy://iggy@localhost:8090";
const DEFAULT_CONNECT_TIMEOUT: &str = "10s";

impl Default for Iggy {
    fn default() -> Self {
        Iggy {
            url: DEFAULT_IGGY_URL.into(),
            password: None,
            token: None,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT.into(),
        }
    }
}

/// Where applied migrations are recorded.
#[config]
#[derive(Debug, Default)]
#[repr(u8)]
#[facet(tag = "type", rename_all = "snake_case")]
pub enum Ledger {
    /// The `migration/migration` topic on the Iggy server itself.
    #[default]
    Iggy,
    /// A Postgres table, which also lets `up` take a lock so concurrent
    /// runners wait for each other.
    Postgres {
        /// Connection string without the password, such as
        /// `postgres://migrate@db.internal/ops?sslmode=require`.
        url: String,
        #[config(secret)]
        password: Option<String>,
        /// Separates the ledgers of different Iggy servers in one table;
        /// defaults to `env`.
        namespace: Option<String>,
        #[facet(default = "iggy_migrate_events".to_string())]
        table: String,
        /// How long `up` waits for another runner to finish, e.g. `10m`.
        #[facet(default = "10m".to_string())]
        lock_timeout: String,
    },
}

#[config]
#[derive(Debug, Default)]
#[facet(default)]
pub struct Rehearsal {
    /// Docker image for rehearsal servers. Defaults to the target server's
    /// version for `up`, and to the version this tool is built for otherwise.
    pub image: Option<String>,
}

impl Config {
    pub fn environments(&self) -> Vec<String> {
        self.environments.split(',').map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).collect()
    }

    pub fn dir(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }

    pub fn runner(&self) -> String {
        self.runner.clone().or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "unknown".into())
    }

    /// The Iggy connection string, with the password or token from the
    /// secrets file filled in.
    pub fn iggy_connection_string(&self) -> Result<String> {
        let url = &self.iggy.url;
        let (scheme, rest) =
            url.split_once("://").ok_or_else(|| anyhow!("iggy.url `{url}` must start with iggy://"))?;
        let (user, server) = match rest.split_once('@') {
            Some((user, server)) => (Some(user), server),
            None => (None, rest),
        };
        if user.is_some_and(|u| u.contains(':')) {
            bail!("iggy.url must not contain a password; set iggy.password in the secrets file instead");
        }
        let credentials = match (user, &self.iggy.password, &self.iggy.token) {
            (_, Some(_), Some(_)) => bail!("set iggy.password or iggy.token, not both"),
            (None, None, Some(token)) => token.clone(),
            (Some(_), None, Some(_)) => bail!("iggy.url names a user, but iggy.token replaces the user"),
            (Some(user), Some(password), None) => {
                if password.contains([':', '@']) {
                    bail!("iggy.password must not contain `:` or `@`, which Iggy connection strings cannot escape");
                }
                format!("{user}:{password}")
            }
            (Some(user), None, None) => {
                bail!("iggy.url names user `{user}` but no iggy.password is set; add it to the secrets file")
            }
            (None, Some(_), None) => bail!("iggy.password is set but iggy.url names no user (iggy://user@host)"),
            (None, None, None) => bail!("set iggy.password (with a user in iggy.url) or iggy.token"),
        };
        Ok(format!("{scheme}://{credentials}@{server}"))
    }
}

pub fn parse_duration(name: &str, value: &str) -> Result<Duration> {
    humantime::parse_duration(value).map_err(|e| anyhow!("{name} `{value}`: {e}"))
}

/// Settings given as command line flags, by config path.
#[derive(Default)]
pub struct Flags {
    pub config: Option<PathBuf>,
    pub secrets: Option<PathBuf>,
    pub values: Vec<(&'static str, String)>,
}

#[derive(Debug)]
struct Origin {
    path: ConfigPath,
}

fn origin<D>(path: ConfigPath, _: D) -> Origin {
    Origin { path }
}

impl SourceFieldPath for Origin {
    fn field_path(&self) -> &ConfigPath {
        &self.path
    }
}

pub fn load(flags: Flags) -> Result<Config> {
    reject_old_env_vars(std::env::vars().map(|(k, _)| k))?;
    let (config_file, required) = match flags.config {
        Some(path) => (path, true),
        None => (PathBuf::from(DEFAULT_FILE), false),
    };
    let secrets_file = flags.secrets.clone().unwrap_or_else(|| secrets_path(&config_file));
    let file = |path: &Path, required: bool| {
        let source = styx_file(path);
        if required { source } else { source.optional() }
    };
    let mut given = ValuesSource::new();
    for (path, value) in flags.values {
        given.insert(ConfigPath::parse(path).map_err(|e| anyhow!("{e:?}"))?, value);
    }
    let loaded = ConfigBuilder::<Config, Origin, String>::new()
        .source_named(
            config_file.display().to_string(),
            Config::register_source::<StyxSource>(|e| e.to_string(), origin),
            file(&config_file, required),
        )
        .source_named(
            secrets_file.display().to_string(),
            Config::register_source::<StyxSource>(|e| e.to_string(), origin),
            file(&secrets_file, flags.secrets.is_some()).secrets(),
        )
        .source_named(
            "the environment",
            Config::register_source::<EnvSource>(|e| e.to_string(), origin::<EnvVarDetail>),
            EnvSource::with_prefix(ENV_PREFIX),
        )
        .source_named("flags", Config::register_source::<ValuesSource>(|never| match never {}, origin), given)
        .load();
    let config = loaded.map_err(describe)?;
    if !config.environments().contains(&config.env) {
        bail!("env `{}` is not one of environments ({})", config.env, config.environments);
    }
    Ok(config)
}

/// `iggy-migrate.styx` → `iggy-migrate.secrets.styx`, in the same directory.
fn secrets_path(config: &Path) -> PathBuf {
    let stem = config.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    config.with_file_name(format!("{stem}.secrets.styx"))
}

/// Variables from before the config moved to ordna would otherwise be
/// ignored without a word.
fn reject_old_env_vars(names: impl Iterator<Item = String>) -> Result<()> {
    let old: Vec<String> = names
        .filter(|n| n.starts_with(&format!("{ENV_PREFIX}_")) && !n.starts_with(&format!("{ENV_PREFIX}__")))
        .collect();
    if !old.is_empty() {
        bail!(
            "{} no longer read: environment variables are now {ENV_PREFIX}__<PATH>, e.g. \
             IGGY_MIGRATE__IGGY__URL or IGGY_MIGRATE__ENV, and passwords go in the secrets file",
            old.join(", ") + if old.len() == 1 { " is" } else { " are" }
        );
    }
    Ok(())
}

fn describe(error: ConfigLoadError<String>) -> anyhow::Error {
    let violations = match error {
        ConfigLoadError::Source(e) => return anyhow!("reading {}: {}", source_name(&e.source), e.error),
        ConfigLoadError::Violations(v) => v,
    };
    let mut problems = Vec::new();
    for issue in violations.unknown_paths() {
        problems.push(format!("unknown setting `{}` in {}", issue.path, source_name(&issue.source)));
    }
    for issue in violations.layer_mismatches() {
        problems.push(format!(
            "`{}` is a secret, so it can only be set in the secrets file, not in {}",
            issue.path,
            source_name(&issue.source)
        ));
    }
    for issue in violations.conflicts() {
        let values: Vec<String> = issue.contenders.iter().map(value).collect();
        problems.push(format!(
            "`{}` is set more than once in {}: {}",
            issue.path,
            source_name(&issue.source),
            values.join(", ")
        ));
    }
    for issue in violations.decode_errors() {
        problems.push(issue.message.clone());
    }
    anyhow!("invalid configuration:\n  {}", problems.join("\n  "))
}

fn source_name(source: &SourceMeta) -> String {
    source.name.clone().unwrap_or_else(|| source.kind.to_string())
}

fn value(value: &DiagnosticValue) -> String {
    match value {
        DiagnosticValue::Text(text) => format!("`{text}`"),
        DiagnosticValue::PartiallyRedacted(text) => format!("`{text}`"),
        DiagnosticValue::FullyRedacted => "(secret)".into(),
        DiagnosticValue::Opaque { type_name } => format!("({type_name})"),
        DiagnosticValue::Unknown => "(unknown)".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(iggy: Iggy) -> Config {
        Config {
            dir: "migrations".into(),
            env: "dev".into(),
            environments: "dev".into(),
            runner: None,
            allow_deletes: false,
            iggy,
            ledger: Ledger::Iggy,
            rehearsal: Rehearsal::default(),
        }
    }

    fn iggy(url: &str, password: Option<&str>, token: Option<&str>) -> Result<String> {
        config(Iggy {
            url: url.into(),
            password: password.map(Into::into),
            token: token.map(Into::into),
            ..Iggy::default()
        })
        .iggy_connection_string()
    }

    #[test]
    fn credentials_are_filled_into_the_iggy_url() {
        assert_eq!(iggy("iggy://u@h:1", Some("p"), None).unwrap(), "iggy://u:p@h:1");
        assert_eq!(iggy("iggy+quic://h:1?x=y", None, Some("t")).unwrap(), "iggy+quic://t@h:1?x=y");
        assert!(iggy("iggy://u:p@h:1", Some("p"), None).is_err());
        assert!(iggy("iggy://u@h:1", None, None).is_err());
        assert!(iggy("iggy://u@h:1", Some("a@b"), None).is_err());
        assert!(iggy("iggy://h:1", Some("p"), Some("t")).is_err());
    }

    #[test]
    fn old_env_vars_are_errors() {
        assert!(reject_old_env_vars(["IGGY_MIGRATE_URL".to_string()].into_iter()).is_err());
        assert!(reject_old_env_vars(["IGGY_MIGRATE__IGGY__URL".to_string(), "HOME".into()].into_iter()).is_ok());
    }

    #[test]
    fn secrets_file_sits_next_to_the_config() {
        assert_eq!(secrets_path(Path::new("ops/prod.styx")), PathBuf::from("ops/prod.secrets.styx"));
        assert_eq!(secrets_path(Path::new(DEFAULT_FILE)), PathBuf::from("iggy-migrate.secrets.styx"));
    }
}
