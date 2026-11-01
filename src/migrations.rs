//! Discovers, evaluates and validates migration files.
//!
//! Files are named `<YYYYMMDDHHMMSS>_<name>.dhall` and applied in timestamp
//! order. Every file is evaluated and validated before anything is applied,
//! so a mistake in any of them stops the run before the server is touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_dhall::StaticType;

use crate::op::Op;
use crate::{dhall, executor, ledger};

pub struct Migration {
    pub id: String,
    pub ops: Vec<Op>,
    pub checksum: String,
    /// For a baseline, the migrations it replaces; empty otherwise.
    pub replaces: Vec<Replaced>,
}

impl Migration {
    pub fn is_baseline(&self) -> bool {
        !self.replaces.is_empty()
    }

    /// The migrations this baseline replaces directly.
    pub fn direct_replaces(&self) -> impl Iterator<Item = &Replaced> {
        self.replaces.iter().filter(|r| r.parent.is_none())
    }
}

/// A migration squashed into a baseline, with the checksum it had in the
/// environment being evaluated.
///
/// When a baseline is itself squashed, the migrations it replaced are carried
/// along with `parent` naming it, so servers that applied the originals but
/// never the intermediate baseline are still recognised. (Dhall has no
/// recursive types, hence a flat list rather than a tree.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, StaticType)]
pub struct Replaced {
    pub id: String,
    pub checksum: String,
    pub parent: Option<String>,
}

/// What a `*_baseline.dhall` file evaluates to.
#[derive(Debug, Deserialize, StaticType)]
pub struct Baseline {
    pub replaces: Vec<Replaced>,
    pub ops: Vec<Op>,
}

pub const BASELINE_SUFFIX: &str = "_baseline";

pub fn load_dir(dir: &Path, env: &dhall::Env) -> Result<Vec<Migration>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "dhall") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut migrations = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match load(&path, env) {
            Ok(m) => migrations.push(m),
            Err(e) => errors.push(format!("{}: {e:#}", path.display())),
        }
    }
    if !errors.is_empty() {
        bail!("{} migration(s) are invalid:\n\n{}", errors.len(), errors.join("\n\n"));
    }
    Ok(migrations)
}

fn load(path: &Path, env: &dhall::Env) -> Result<Migration> {
    let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    validate_id(&id)?;
    let source = fs::read_to_string(path)?;
    let (ops, replaces) = if id.ends_with(BASELINE_SUFFIX) {
        let baseline: Baseline = dhall::eval_as(&source, env)?;
        if baseline.replaces.is_empty() {
            bail!("a baseline must replace at least one migration");
        }
        (baseline.ops, baseline.replaces)
    } else {
        (dhall::eval(&source, env)?, Vec::new())
    };
    for (i, op) in ops.iter().enumerate() {
        validate(op).with_context(|| format!("op {} ({op:?})", i + 1))?;
    }
    validate_updates(&ops)?;
    let checksum = if replaces.is_empty() {
        dhall::checksum(&ops)
    } else {
        dhall::checksum(&(&replaces, &ops))
    };
    Ok(Migration { id, ops, checksum, replaces })
}

fn validate_id(id: &str) -> Result<()> {
    let valid = id.split_once('_').is_some_and(|(ts, name)| {
        ts.len() == 14
            && ts.bytes().all(|b| b.is_ascii_digit())
            && !name.is_empty()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    });
    if !valid {
        bail!("file name must be <YYYYMMDDHHMMSS>_<snake_case_name>.dhall");
    }
    Ok(())
}

fn validate(op: &Op) -> Result<()> {
    let (stream, names): (&str, Vec<&str>) = match op {
        Op::CreateStream { name } | Op::DeleteStream { name } => (name, vec![name]),
        Op::CreateTopic {
            stream,
            name,
            partitions,
            message_expiry,
            max_size,
            compression,
            segment_size,
            durability,
        } => {
            validate_topic(Some(*partitions), message_expiry, max_size, compression)?;
            if let Some(s) = segment_size {
                executor::parse_segment_size(s)?;
            }
            if let Some(d) = durability {
                executor::parse_durability(d)?;
            }
            (stream, vec![stream, name])
        }
        Op::UpdateTopic { stream, name, partitions, message_expiry, max_size, compression } => {
            if partitions.is_none()
                && message_expiry.is_none()
                && max_size.is_none()
                && compression.is_none()
            {
                bail!(
                    "an update must set at least one of partitions, message_expiry, max_size, \
                     compression"
                );
            }
            validate_topic(*partitions, message_expiry, max_size, compression)?;
            (stream, vec![stream, name])
        }
        Op::DeleteTopic { stream, name } => (stream, vec![stream, name]),
        Op::CreateConsumerGroup { stream, topic, name }
        | Op::DeleteConsumerGroup { stream, topic, name } => (stream, vec![stream, topic, name]),
    };
    if names.iter().any(|n| n.trim().is_empty()) {
        bail!("names must not be empty");
    }
    if stream == ledger::STREAM {
        bail!("migrations must not modify the `{}` ledger stream", ledger::STREAM);
    }
    Ok(())
}

fn validate_topic(
    partitions: Option<u64>,
    message_expiry: &Option<String>,
    max_size: &Option<String>,
    compression: &Option<String>,
) -> Result<()> {
    if let Some(p) = partitions
        && (p == 0 || p > u64::from(u32::MAX))
    {
        bail!("partitions must be between 1 and {}", u32::MAX);
    }
    if let Some(e) = message_expiry {
        executor::parse_expiry(e)?;
    }
    if let Some(s) = max_size {
        executor::parse_size(s)?;
    }
    if let Some(c) = compression {
        executor::parse_compression(c)?;
    }
    Ok(())
}

/// Allows each topic one change per migration: a create or a single update.
/// Rerunning a migration that failed halfway would otherwise find the topic
/// already changed by a later op, so an earlier create or update would no
/// longer match it; write the final config in one op instead.
fn validate_updates(ops: &[Op]) -> Result<()> {
    let mut changed = Vec::new();
    for op in ops {
        if let Op::CreateTopic { .. } | Op::UpdateTopic { .. } = op {
            let key = executor::key(op);
            if changed.contains(&key) {
                bail!(
                    "topic {key} is created or updated more than once in this migration; \
                     combine the changes into one op"
                );
            }
            changed.push(key);
        }
    }
    Ok(())
}

pub fn create(dir: &Path, name: &str) -> Result<PathBuf> {
    let now = humantime::format_rfc3339_seconds(SystemTime::now()).to_string();
    let ts: String = now.chars().filter(char::is_ascii_digit).collect();
    let id = format!("{ts}_{name}");
    validate_id(&id).context("invalid migration name")?;
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{id}.dhall"));
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    fs::write(&path, TEMPLATE)?;
    Ok(path)
}

const TEMPLATE: &str = r#"-- Ops available: Op.CreateStream, Op.DeleteStream, Op.CreateTopic,
-- Op.DeleteTopic, Op.CreateConsumerGroup, Op.DeleteConsumerGroup.
-- Topic::{ stream, name } gives a topic with defaults; `env.kind` is the
-- environment being migrated, for use with `merge`.
--
-- [ Op.CreateStream { name = "orders" }
-- , Op.CreateTopic Topic::{ stream = "orders", name = "events", partitions = 3 }
-- ]

[] : List Op
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_must_be_timestamped_snake_case() {
        assert!(validate_id("20261101120000_create_orders").is_ok());
        assert!(validate_id("2026110112_create_orders").is_err());
        assert!(validate_id("20261101120000_Create-Orders").is_err());
        assert!(validate_id("20261101120000_").is_err());
        assert!(validate_id("create_orders").is_err());
    }

    #[test]
    fn ledger_stream_is_off_limits() {
        assert!(validate(&Op::DeleteStream { name: ledger::STREAM.into() }).is_err());
        assert!(validate(&Op::DeleteTopic { stream: ledger::STREAM.into(), name: "x".into() }).is_err());
    }

    #[test]
    fn invalid_topic_settings_are_rejected() {
        let topic = |partitions, expiry: Option<&str>| Op::CreateTopic {
            stream: "s".into(),
            name: "t".into(),
            partitions,
            message_expiry: expiry.map(Into::into),
            max_size: None,
            compression: None,
            segment_size: None,
            durability: None,
        };
        assert!(validate(&topic(0, None)).is_err());
        assert!(validate(&topic(1, Some("whenever"))).is_err());
        assert!(validate(&topic(1, Some("7d"))).is_ok());
    }

    #[test]
    fn invalid_updates_are_rejected() {
        let update = |partitions| Op::UpdateTopic {
            stream: "s".into(),
            name: "t".into(),
            partitions,
            message_expiry: None,
            max_size: None,
            compression: None,
        };
        assert!(validate(&update(None)).is_err());
        assert!(validate(&update(Some(0))).is_err());
        assert!(validate(&update(Some(4))).is_ok());
        let create = Op::CreateTopic {
            stream: "s".into(),
            name: "t".into(),
            partitions: 1,
            message_expiry: None,
            max_size: None,
            compression: None,
            segment_size: None,
            durability: None,
        };
        assert!(validate_updates(&[create, update(Some(4))]).is_err());
        assert!(validate_updates(&[update(Some(4)), update(Some(5))]).is_err());
        assert!(validate_updates(&[update(Some(4))]).is_ok());
    }

    #[test]
    fn template_evaluates() {
        let env = dhall::Env { name: "dev".into(), all: vec!["dev".into()] };
        assert_eq!(dhall::eval(TEMPLATE, &env).unwrap(), vec![]);
    }
}
