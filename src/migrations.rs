//! Discovers, evaluates and validates migration files.
//!
//! Files are named `<YYYYMMDDHHMMSS>_<name>.dhall` and applied in timestamp
//! order. Every file is evaluated and validated before anything is applied,
//! so a mistake in any of them stops the run before the server is touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::op::Op;
use crate::{dhall, executor, ledger};

pub struct Migration {
    pub id: String,
    pub ops: Vec<Op>,
    pub checksum: String,
}

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
    let ops = dhall::eval(&source, env)?;
    for (i, op) in ops.iter().enumerate() {
        validate(op).with_context(|| format!("op {} ({op:?})", i + 1))?;
    }
    let checksum = dhall::checksum(&ops);
    Ok(Migration { id, ops, checksum })
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
        Op::CreateTopic { stream, name, partitions, message_expiry, max_size } => {
            if *partitions == 0 || *partitions > u64::from(u32::MAX) {
                bail!("partitions must be between 1 and {}", u32::MAX);
            }
            if let Some(e) = message_expiry {
                executor::parse_expiry(e)?;
            }
            if let Some(s) = max_size {
                executor::parse_size(s)?;
            }
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
        };
        assert!(validate(&topic(0, None)).is_err());
        assert!(validate(&topic(1, Some("whenever"))).is_err());
        assert!(validate(&topic(1, Some("7d"))).is_ok());
    }

    #[test]
    fn template_evaluates() {
        let env = dhall::Env { name: "dev".into(), all: vec!["dev".into()] };
        assert_eq!(dhall::eval(TEMPLATE, &env).unwrap(), vec![]);
    }
}
