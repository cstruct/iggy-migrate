//! Applies ops against a live server. Every op is checked against current
//! state first, so rerunning a migration that failed halfway is safe: work
//! that is already done is skipped, and existing resources whose config
//! differs from the op are a hard error rather than silently accepted.

use std::collections::HashSet;
use std::fmt;
use std::str::FromStr;

use anyhow::{Result, bail};
use iggy::prelude::*;

use crate::op::Op;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Apply,
    Skip(&'static str),
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Apply => f.write_str("apply"),
            Action::Skip(reason) => write!(f, "skip ({reason})"),
        }
    }
}

/// Tracks what earlier ops in a dry run would have done, so later ops are
/// planned against that state instead of the server's current state.
#[derive(Default)]
pub struct Planned {
    created: HashSet<String>,
    deleted: HashSet<String>,
}

impl Planned {
    /// `Some(exists)` when earlier planned ops decide whether `key` exists.
    fn exists(&self, key: &str) -> Option<bool> {
        let ancestors = || ancestors(key);
        if self.deleted.contains(key) || ancestors().any(|a| self.deleted.contains(&a)) {
            return Some(false);
        }
        if self.created.contains(key) {
            return Some(true);
        }
        if ancestors().any(|a| self.created.contains(&a)) {
            return Some(false);
        }
        None
    }

    fn record(&mut self, op: &Op) {
        let key = key(op);
        if matches!(op, Op::UpdateTopic { .. }) {
            return;
        }
        if creates(op) {
            self.deleted.retain(|k| k != &key && !ancestors(k).any(|a| a == key));
            self.created.insert(key);
        } else {
            self.created.retain(|k| k != &key && !ancestors(k).any(|a| a == key));
            self.deleted.insert(key);
        }
    }
}

pub enum Mode<'a> {
    Apply,
    DryRun(&'a mut Planned),
}

pub fn key(op: &Op) -> String {
    match op {
        Op::CreateStream { name } | Op::DeleteStream { name } => name.clone(),
        Op::CreateTopic { stream, name, .. }
        | Op::UpdateTopic { stream, name, .. }
        | Op::DeleteTopic { stream, name } => {
            format!("{stream}/{name}")
        }
        Op::CreateConsumerGroup { stream, topic, name } | Op::DeleteConsumerGroup { stream, topic, name } => {
            format!("{stream}/{topic}/{name}")
        }
    }
}

pub fn ancestors(key: &str) -> impl Iterator<Item = String> + '_ {
    key.match_indices('/').map(|(i, _)| key[..i].to_string())
}

fn id(name: &str) -> Result<Identifier> {
    Ok(Identifier::named(name)?)
}

pub async fn run(client: &IggyClient, op: &Op, mode: Mode<'_>) -> Result<Action> {
    let key = key(op);
    if let Some(parent) = ancestors(&key).last() {
        let planned = match &mode {
            Mode::DryRun(planned) => planned.exists(&parent),
            Mode::Apply => None,
        };
        let exists = match planned {
            Some(exists) => exists,
            None => resource_exists(client, &parent).await?,
        };
        if !exists && creates(op) {
            let kind = if parent.contains('/') { "topic" } else { "stream" };
            bail!("cannot create {key}: {kind} {parent} does not exist");
        }
    }
    let planned = match &mode {
        Mode::DryRun(planned) => planned.exists(&key),
        Mode::Apply => None,
    };
    let action = match planned {
        Some(exists) => planned_action(op, &key, exists)?,
        None => check(client, op).await?,
    };
    match mode {
        Mode::DryRun(planned) => planned.record(op),
        Mode::Apply if action == Action::Apply => execute(client, op).await?,
        Mode::Apply => {}
    }
    Ok(action)
}

pub fn creates(op: &Op) -> bool {
    matches!(op, Op::CreateStream { .. } | Op::CreateTopic { .. } | Op::CreateConsumerGroup { .. })
}

/// Whether the stream (`s`) or topic (`s/t`) named by `key` exists.
async fn resource_exists(client: &IggyClient, key: &str) -> Result<bool> {
    Ok(match key.split_once('/') {
        None => client.get_stream(&id(key)?).await?.is_some(),
        Some((stream, topic)) => get_topic(client, stream, topic).await?.is_some(),
    })
}

fn planned_action(op: &Op, key: &str, exists: bool) -> Result<Action> {
    Ok(match (op, creates(op), exists) {
        (Op::UpdateTopic { .. }, _, true) => Action::Apply,
        (Op::UpdateTopic { .. }, _, false) => {
            bail!("cannot update {key}: topic does not exist")
        }
        (_, true, false) | (_, false, true) => Action::Apply,
        (_, true, true) => Action::Skip("created earlier in this run"),
        (_, false, false) => Action::Skip("deleted earlier in this run"),
    })
}

async fn check(client: &IggyClient, op: &Op) -> Result<Action> {
    Ok(match op {
        Op::CreateStream { name } => match client.get_stream(&id(name)?).await? {
            Some(_) => Action::Skip("already exists"),
            None => Action::Apply,
        },
        Op::DeleteStream { name } => match client.get_stream(&id(name)?).await? {
            Some(_) => Action::Apply,
            None => Action::Skip("already absent"),
        },
        Op::CreateTopic { stream, name, .. } => {
            let Some(existing) = get_topic(client, stream, name).await? else {
                return Ok(Action::Apply);
            };
            let diffs = topic_diffs(op, &existing)?;
            if !diffs.is_empty() {
                bail!("topic {stream}/{name} already exists with different config: {}", diffs.join(", "));
            }
            Action::Skip("already exists")
        }
        Op::UpdateTopic { stream, name, .. } => {
            let Some(existing) = get_topic(client, stream, name).await? else {
                bail!("cannot update {stream}/{name}: topic does not exist");
            };
            if topic_update(op, &existing)?.is_empty() { Action::Skip("already up to date") } else { Action::Apply }
        }
        Op::DeleteTopic { stream, name } => match get_topic(client, stream, name).await? {
            Some(_) => Action::Apply,
            None => Action::Skip("already absent"),
        },
        Op::CreateConsumerGroup { stream, topic, name } => match get_group(client, stream, topic, name).await? {
            Some(_) => Action::Skip("already exists"),
            None => Action::Apply,
        },
        Op::DeleteConsumerGroup { stream, topic, name } => match get_group(client, stream, topic, name).await? {
            Some(_) => Action::Apply,
            None => Action::Skip("already absent"),
        },
    })
}

async fn execute(client: &IggyClient, op: &Op) -> Result<()> {
    match op {
        Op::CreateStream { name } => {
            client.create_stream(name).await?;
        }
        Op::DeleteStream { name } => client.delete_stream(&id(name)?).await?,
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
            let options = TopicCreateOptions {
                partitions_count: Some(u32::try_from(*partitions)?),
                message_expiry: message_expiry.as_deref().map(parse_expiry).transpose()?,
                max_topic_size: max_size.as_deref().map(parse_size).transpose()?,
                compression_algorithm: compression.as_deref().map(parse_compression).transpose()?,
                segment_size: segment_size.as_deref().map(parse_segment_size).transpose()?,
                durability: durability.as_deref().map(parse_durability).transpose()?.unwrap_or_default(),
                ..Default::default()
            };
            client.create_topic(&id(stream)?, name, &options).await.map_err(size_hint)?;
        }
        Op::UpdateTopic { stream, name, .. } => {
            let Some(existing) = get_topic(client, stream, name).await? else {
                bail!("cannot update {stream}/{name}: topic does not exist");
            };
            let mut options = TopicUpdateOptions::default();
            for change in topic_update(op, &existing)? {
                match change {
                    Change::Partitions(n) => client.create_partitions(&id(stream)?, &id(name)?, n).await?,
                    Change::Expiry(e) => options.message_expiry = Some(e),
                    Change::MaxSize(s) => options.max_topic_size = Some(s),
                    Change::Compression(c) => options.compression_algorithm = Some(c),
                }
            }
            if options != TopicUpdateOptions::default() {
                client.update_topic(&id(stream)?, &id(name)?, &existing.name, &options).await.map_err(size_hint)?;
            }
        }
        Op::DeleteTopic { stream, name } => client.delete_topic(&id(stream)?, &id(name)?).await?,
        Op::CreateConsumerGroup { stream, topic, name } => {
            client.create_consumer_group(&id(stream)?, &id(topic)?, name).await?;
        }
        Op::DeleteConsumerGroup { stream, topic, name } => {
            client.delete_consumer_group(&id(stream)?, &id(topic)?, &id(name)?).await?
        }
    }
    Ok(())
}

/// The server's error for a max size below the segment size prints the wrong
/// numbers, so say what the limit actually is.
fn size_hint(e: IggyError) -> anyhow::Error {
    let hint = e
        .to_string()
        .contains("segment size")
        .then_some("max_size must be at least the topic's segment_size (1 GiB unless set; note 1 GB < 1 GiB)");
    match hint {
        Some(hint) => anyhow::Error::new(e).context(hint),
        None => e.into(),
    }
}

enum Change {
    /// Partitions to add.
    Partitions(u32),
    Expiry(IggyExpiry),
    MaxSize(MaxTopicSize),
    Compression(CompressionAlgorithm),
}

/// How an existing topic differs from a `CreateTopic`, mirroring how a
/// create is checked: settings the op leaves to the server default are not
/// compared.
pub fn topic_diffs(op: &Op, existing: &TopicDetails) -> Result<Vec<String>> {
    let Op::CreateTopic { partitions, message_expiry, max_size, compression, segment_size, durability, .. } = op else {
        unreachable!("only called for CreateTopic");
    };
    let runtime = iggy_common::TopicRuntimeOptions::from_resource_options(&existing.options);
    let mut diffs = Vec::new();
    let mut diff = |name: &str, differs: bool, have: String, want: &str| {
        if differs {
            diffs.push(format!("{name} {have} (want {want})"));
        }
    };
    diff(
        "partitions",
        u64::from(existing.partitions_count) != *partitions,
        existing.partitions_count.to_string(),
        &partitions.to_string(),
    );
    if let Some(want) = message_expiry {
        let have = existing.message_expiry;
        diff("message_expiry", have != parse_expiry(want)?, have.to_string(), want);
    }
    if let Some(want) = max_size {
        let have = existing.max_topic_size;
        diff("max_size", have != parse_size(want)?, have.to_string(), want);
    }
    if let Some(want) = compression {
        let have = existing.compression_algorithm;
        diff("compression", have != parse_compression(want)?, have.to_string(), want);
    }
    if let Some(want) = segment_size {
        let have = runtime.effective_segment_size();
        diff("segment_size", have != parse_segment_size(want)?, have.to_string(), want);
    }
    if let Some(want) = durability {
        let have = runtime.durability;
        diff("durability", have != parse_durability(want)?, have.to_string(), want);
    }
    Ok(diffs)
}

/// What an `UpdateTopic` would change on `existing`; empty when it is
/// already up to date. Shrinking a topic is an error, since Iggy removes
/// partitions by deleting them and their messages.
fn topic_update(op: &Op, existing: &TopicDetails) -> Result<Vec<Change>> {
    let Op::UpdateTopic { stream, name, partitions, message_expiry, max_size, compression } = op else {
        unreachable!("only called for UpdateTopic");
    };
    let mut changes = Vec::new();
    if let Some(want) = partitions {
        let have = u64::from(existing.partitions_count);
        if *want < have {
            bail!(
                "cannot reduce {stream}/{name} from {have} to {want} partitions: removing \
                 partitions deletes their messages"
            );
        }
        if *want > have {
            changes.push(Change::Partitions(u32::try_from(want - have)?));
        }
    }
    if let Some(want) = message_expiry {
        let want = parse_expiry(want)?;
        if existing.message_expiry != want {
            changes.push(Change::Expiry(want));
        }
    }
    if let Some(want) = max_size {
        let want = parse_size(want)?;
        if existing.max_topic_size != want {
            changes.push(Change::MaxSize(want));
        }
    }
    if let Some(want) = compression {
        let want = parse_compression(want)?;
        if existing.compression_algorithm != want {
            changes.push(Change::Compression(want));
        }
    }
    Ok(changes)
}

async fn get_topic(client: &IggyClient, stream: &str, name: &str) -> Result<Option<TopicDetails>> {
    if client.get_stream(&id(stream)?).await?.is_none() {
        return Ok(None);
    }
    Ok(client.get_topic(&id(stream)?, &id(name)?).await?)
}

async fn get_group(client: &IggyClient, stream: &str, topic: &str, name: &str) -> Result<Option<ConsumerGroupDetails>> {
    if get_topic(client, stream, topic).await?.is_none() {
        return Ok(None);
    }
    Ok(client.get_consumer_group(&id(stream)?, &id(topic)?, &id(name)?).await?)
}

pub fn parse_expiry(s: &str) -> Result<IggyExpiry> {
    IggyExpiry::from_str(s).map_err(|e| anyhow::anyhow!("invalid message_expiry `{s}`: {e}"))
}

pub fn parse_size(s: &str) -> Result<MaxTopicSize> {
    MaxTopicSize::from_str(s).map_err(|e| anyhow::anyhow!("invalid max_size `{s}`: {e}"))
}

pub fn parse_compression(s: &str) -> Result<CompressionAlgorithm> {
    CompressionAlgorithm::from_str(s).map_err(|e| anyhow::anyhow!("invalid compression `{s}` (use none or gzip): {e}"))
}

pub fn parse_segment_size(s: &str) -> Result<IggyByteSize> {
    IggyByteSize::from_str(s).map_err(|e| anyhow::anyhow!("invalid segment_size `{s}`: {e}"))
}

pub fn parse_durability(s: &str) -> Result<Durability> {
    Durability::from_str(s).map_err(|e| anyhow::anyhow!("invalid durability `{s}` (use replicated or persisted): {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(name: &str) -> Op {
        Op::CreateStream { name: name.into() }
    }

    fn topic(stream: &str, name: &str) -> Op {
        Op::CreateTopic {
            stream: stream.into(),
            name: name.into(),
            partitions: 1,
            message_expiry: None,
            max_size: None,
            compression: None,
            segment_size: None,
            durability: None,
        }
    }

    #[test]
    fn planned_children_of_created_stream_do_not_exist() {
        let mut p = Planned::default();
        p.record(&stream("orders"));
        assert_eq!(p.exists("orders"), Some(true));
        assert_eq!(p.exists("orders/events"), Some(false));
        assert_eq!(p.exists("other/events"), None);
    }

    #[test]
    fn planned_delete_removes_descendants() {
        let mut p = Planned::default();
        p.record(&stream("orders"));
        p.record(&topic("orders", "events"));
        p.record(&Op::DeleteStream { name: "orders".into() });
        assert_eq!(p.exists("orders"), Some(false));
        assert_eq!(p.exists("orders/events"), Some(false));
        p.record(&stream("orders"));
        assert_eq!(p.exists("orders"), Some(true));
        assert_eq!(p.exists("orders/events"), Some(false));
    }

    #[test]
    fn expiry_and_size_parse() {
        assert!(parse_expiry("7d").is_ok());
        assert_eq!(parse_expiry("none").unwrap(), IggyExpiry::NeverExpire);
        assert!(parse_expiry("soon").is_err());
        assert_eq!(parse_size("unlimited").unwrap(), MaxTopicSize::Unlimited);
        assert!(parse_size("10 GB").is_ok());
        assert!(parse_size("big").is_err());
        assert!(parse_compression("gzip").is_ok());
        assert!(parse_compression("zstd").is_err());
        assert!(parse_durability("persisted").is_ok());
        assert!(parse_durability("eventually").is_err());
        assert!(parse_segment_size("256 MiB").is_ok());
    }
}
