use serde::{Deserialize, Serialize};
use serde_dhall::StaticType;

/// A single operation a migration can perform. Every migration format
/// (Dhall today, possibly a line format later) evaluates to a `Vec<Op>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, StaticType)]
pub enum Op {
    CreateStream {
        name: String,
    },
    DeleteStream {
        name: String,
    },
    CreateTopic {
        stream: String,
        name: String,
        partitions: u64,
        message_expiry: Option<String>,
        max_size: Option<String>,
    },
    DeleteTopic {
        stream: String,
        name: String,
    },
    CreateConsumerGroup {
        stream: String,
        topic: String,
        name: String,
    },
    DeleteConsumerGroup {
        stream: String,
        topic: String,
        name: String,
    },
}
