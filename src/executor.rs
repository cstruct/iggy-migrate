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
        Op::CreateTopic { stream, name, .. } | Op::DeleteTopic { stream, name } => {
            format!("{stream}/{name}")
        }
        Op::CreateConsumerGroup { stream, topic, name }
        | Op::DeleteConsumerGroup { stream, topic, name } => format!("{stream}/{topic}/{name}"),
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
        Some(exists) => planned_action(op, exists),
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

fn planned_action(op: &Op, exists: bool) -> Action {
    match (creates(op), exists) {
        (true, false) | (false, true) => Action::Apply,
        (true, true) => Action::Skip("created earlier in this run"),
        (false, false) => Action::Skip("deleted earlier in this run"),
    }
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
        Op::CreateTopic { stream, name, partitions, message_expiry, max_size } => {
            let Some(existing) = get_topic(client, stream, name).await? else {
                return Ok(Action::Apply);
            };
            let mut diffs = Vec::new();
            if u64::from(existing.partitions_count) != *partitions {
                diffs.push(format!("partitions {} (want {partitions})", existing.partitions_count));
            }
            if let Some(want) = message_expiry
                && existing.message_expiry != parse_expiry(want)?
            {
                diffs.push(format!("message_expiry {} (want {want})", existing.message_expiry));
            }
            if let Some(want) = max_size
                && existing.max_topic_size != parse_size(want)?
            {
                diffs.push(format!("max_size {} (want {want})", existing.max_topic_size));
            }
            if !diffs.is_empty() {
                bail!("topic {stream}/{name} already exists with different config: {}", diffs.join(", "));
            }
            Action::Skip("already exists")
        }
        Op::DeleteTopic { stream, name } => match get_topic(client, stream, name).await? {
            Some(_) => Action::Apply,
            None => Action::Skip("already absent"),
        },
        Op::CreateConsumerGroup { stream, topic, name } => {
            match get_group(client, stream, topic, name).await? {
                Some(_) => Action::Skip("already exists"),
                None => Action::Apply,
            }
        }
        Op::DeleteConsumerGroup { stream, topic, name } => {
            match get_group(client, stream, topic, name).await? {
                Some(_) => Action::Apply,
                None => Action::Skip("already absent"),
            }
        }
    })
}

async fn execute(client: &IggyClient, op: &Op) -> Result<()> {
    match op {
        Op::CreateStream { name } => {
            client.create_stream(name).await?;
        }
        Op::DeleteStream { name } => client.delete_stream(&id(name)?).await?,
        Op::CreateTopic { stream, name, partitions, message_expiry, max_size } => {
            let options = TopicCreateOptions {
                partitions_count: Some(u32::try_from(*partitions)?),
                message_expiry: message_expiry.as_deref().map(parse_expiry).transpose()?,
                max_topic_size: max_size.as_deref().map(parse_size).transpose()?,
                ..Default::default()
            };
            client.create_topic(&id(stream)?, name, &options).await?;
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

async fn get_topic(client: &IggyClient, stream: &str, name: &str) -> Result<Option<TopicDetails>> {
    if client.get_stream(&id(stream)?).await?.is_none() {
        return Ok(None);
    }
    Ok(client.get_topic(&id(stream)?, &id(name)?).await?)
}

async fn get_group(
    client: &IggyClient,
    stream: &str,
    topic: &str,
    name: &str,
) -> Result<Option<ConsumerGroupDetails>> {
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
    }
}
