//! Drift: differences between the state the applied migrations produce and
//! what the server actually contains, such as resources created, changed or
//! deleted by hand.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use iggy::prelude::*;
use serde::Serialize;

use crate::executor::{self, key};
use crate::ledger;
use crate::migrations::Migration;
use crate::squash;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Created by an applied migration, but not on the server.
    Missing,
    /// On the server with a config the migrations did not set.
    Changed,
    /// On the server, but not created by any applied migration.
    Unmanaged,
}

#[derive(Debug, Serialize)]
pub struct Item {
    pub kind: Kind,
    pub resource: &'static str,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Compares the server with the state `applied` produces. A missing or
/// unmanaged stream or topic is reported once, not again for its children.
pub async fn detect(client: &IggyClient, applied: &[&Migration]) -> Result<Vec<Item>> {
    let expected = squash::fold(applied.iter().flat_map(|m| &m.ops))
        .context("the applied migrations do not add up to a state to compare against")?;
    let mut actual = server_state(client).await?;
    let mut items = Vec::new();
    for op in &expected {
        let k = key(op);
        match actual.remove(&k) {
            None => items.push(item(Kind::Missing, k, None)),
            Some(Some(topic)) => {
                let diffs = executor::topic_diffs(op, &topic)?;
                if !diffs.is_empty() {
                    items.push(item(Kind::Changed, k, Some(diffs.join(", "))));
                }
            }
            Some(None) => {}
        }
    }
    items.extend(actual.into_keys().map(|k| item(Kind::Unmanaged, k, None)));
    let reported: Vec<(Kind, String)> = items.iter().map(|i| (i.kind, i.key.clone())).collect();
    items.retain(|i| !executor::ancestors(&i.key).any(|a| reported.iter().any(|(kind, k)| *kind == i.kind && *k == a)));
    items.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(items)
}

fn item(kind: Kind, key: String, details: Option<String>) -> Item {
    let resource = match key.matches('/').count() {
        0 => "stream",
        1 => "topic",
        _ => "consumer_group",
    };
    Item { kind, resource, key, details }
}

/// Every resource on the server except the ledger, by key, with details for
/// topics.
async fn server_state(client: &IggyClient) -> Result<BTreeMap<String, Option<TopicDetails>>> {
    let mut state = BTreeMap::new();
    for stream in client.get_streams().await? {
        if stream.name == ledger::STREAM {
            continue;
        }
        let stream_id = Identifier::named(&stream.name)?;
        for topic in client.get_topics(&stream_id).await? {
            let topic_id = Identifier::named(&topic.name)?;
            for group in client.get_consumer_groups(&stream_id, &topic_id).await? {
                state.insert(format!("{}/{}/{}", stream.name, topic.name, group.name), None);
            }
            let details = client.get_topic(&stream_id, &topic_id).await?;
            state.insert(format!("{}/{}", stream.name, topic.name), details);
        }
        state.insert(stream.name, None);
    }
    Ok(state)
}
