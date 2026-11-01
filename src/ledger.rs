//! The ledger: an append-only record of migration runs, stored in Iggy itself
//! as JSON messages on a single-partition topic that never expires.

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use iggy::prelude::*;
use serde::{Deserialize, Serialize};

use crate::op::Op;

pub const STREAM: &str = "migration";
pub const TOPIC: &str = "migration";
const PARTITION: u32 = 0;
const CONSUMER: &str = "iggy-migrate";
const BATCH: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Applied,
    Failed,
    /// A baseline marked applied without running it, because every migration
    /// it replaces had already been applied.
    Baselined,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub status: Status,
    pub checksum: String,
    pub env: String,
    /// The evaluated ops, so the ledger shows what actually ran.
    pub ops: Vec<Op>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// For a baseline, the ids of the migrations it replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
    pub applied_at: String,
    pub duration_ms: u64,
    pub runner: String,
    pub tool_version: String,
}

fn stream_id() -> Identifier {
    Identifier::named(STREAM).expect("valid stream name")
}

fn topic_id() -> Identifier {
    Identifier::named(TOPIC).expect("valid topic name")
}

/// Creates the ledger stream and topic if missing, and verifies that an
/// existing ledger topic is configured so it can never lose history.
pub async fn ensure(client: &IggyClient) -> Result<()> {
    if client.get_stream(&stream_id()).await?.is_none() {
        client.create_stream(STREAM).await.context("creating ledger stream")?;
    }
    match client.get_topic(&stream_id(), &topic_id()).await? {
        None => {
            let options = TopicCreateOptions {
                partitions_count: Some(1),
                message_expiry: Some(IggyExpiry::NeverExpire),
                max_topic_size: Some(MaxTopicSize::Unlimited),
                ..Default::default()
            };
            client
                .create_topic(&stream_id(), TOPIC, &options)
                .await
                .context("creating ledger topic")?;
        }
        Some(topic) => {
            if topic.partitions_count != 1
                || topic.message_expiry != IggyExpiry::NeverExpire
                || topic.max_topic_size != MaxTopicSize::Unlimited
            {
                bail!(
                    "ledger topic {STREAM}/{TOPIC} must have 1 partition, no message expiry and \
                     unlimited size, but has {} partitions, expiry {:?}, max size {:?}",
                    topic.partitions_count,
                    topic.message_expiry,
                    topic.max_topic_size,
                );
            }
        }
    }
    Ok(())
}

/// Reads every event in the ledger, oldest first. A missing ledger reads as
/// empty, so read-only commands never create it.
pub async fn read(client: &IggyClient) -> Result<Vec<Event>> {
    if client.get_topic(&stream_id(), &topic_id()).await.ok().flatten().is_none() {
        return Ok(Vec::new());
    }
    let consumer = Consumer::new(Identifier::named(CONSUMER)?);
    let mut events = Vec::new();
    let mut offset = 0;
    loop {
        let polled = client
            .poll_messages(
                &stream_id(),
                &topic_id(),
                Some(PARTITION),
                &consumer,
                &PollingStrategy::offset(offset),
                BATCH,
                false,
            )
            .await
            .context("reading ledger")?;
        let Some(last) = polled.messages.last() else {
            break;
        };
        offset = last.header.offset + 1;
        for message in &polled.messages {
            let event = serde_json::from_slice(&message.payload).with_context(|| {
                format!("ledger message at offset {} is not a valid event", message.header.offset)
            })?;
            events.push(event);
        }
    }
    Ok(events)
}

pub async fn append(client: &IggyClient, event: &Event) -> Result<()> {
    let payload = serde_json::to_string(event)?;
    let mut messages = [IggyMessage::from_str(&payload)?];
    client
        .send_messages(
            &stream_id(),
            &topic_id(),
            &Partitioning::partition_id(PARTITION),
            &mut messages,
        )
        .await
        .context("writing ledger event")?;
    Ok(())
}
