//! The ledger: an append-only record of migration runs, in Iggy itself or in
//! Postgres.

pub mod iggy;
pub mod postgres;

use ::iggy::prelude::IggyClient;
use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::op::Op;

pub use self::iggy::STREAM;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Applied,
    Failed,
    /// A baseline marked applied without running it, because every migration
    /// it replaces had already been applied.
    Baselined,
    /// Recorded as applied by `ledger accept`, with the checksum the file had
    /// then, without running it. Unlike `Applied`, it replaces an earlier
    /// applied event for the same id.
    Accepted,
    /// Undoes the earlier applied events for the id, by `ledger forget`, so the
    /// migration is pending again or, if its file is gone, no longer missing.
    Forgotten,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Applied => "applied",
            Status::Failed => "failed",
            Status::Baselined => "baselined",
            Status::Accepted => "accepted",
            Status::Forgotten => "forgotten",
        }
    }
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
    /// Why a repair (`accept` or `forget`) was made, as given with `--reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For a baseline, the ids of the migrations it replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<String>,
    pub applied_at: String,
    pub duration_ms: u64,
    pub runner: String,
    pub tool_version: String,
}

pub enum Ledger<'a> {
    Iggy(&'a IggyClient),
    Postgres(Box<postgres::Ledger>),
}

impl Ledger<'_> {
    /// Every event, oldest first. A ledger that doesn't exist yet reads as
    /// empty, so read-only commands never create it.
    pub async fn read(&self) -> Result<Vec<Event>> {
        match self {
            Ledger::Iggy(client) => self::iggy::read(client).await,
            Ledger::Postgres(ledger) => ledger.read().await,
        }
    }

    /// Creates the ledger if missing.
    pub async fn ensure(&self) -> Result<()> {
        match self {
            Ledger::Iggy(client) => self::iggy::ensure(client).await,
            Ledger::Postgres(ledger) => ledger.ensure().await,
        }
    }

    pub async fn append(&self, event: &Event) -> Result<()> {
        match self {
            Ledger::Iggy(client) => self::iggy::append(client, event).await,
            Ledger::Postgres(ledger) => ledger.append(event).await,
        }
    }

    /// Waits until no other runner holds this ledger. Only Postgres can lock;
    /// with the Iggy ledger concurrent runners are not prevented.
    pub async fn lock(&self) -> Result<()> {
        match self {
            Ledger::Iggy(_) => Ok(()),
            Ledger::Postgres(ledger) => ledger.lock().await,
        }
    }
}
