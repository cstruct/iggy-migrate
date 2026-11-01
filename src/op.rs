use serde::{Deserialize, Serialize};

/// A single operation a migration can perform. Every migration evaluates to
/// a `Vec<Op>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        // Settings added after the first release are left out of the JSON
        // when unset, so checksums of migrations written before them hold.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compression: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        segment_size: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        durability: Option<String>,
    },
    /// Sets the given fields of an existing topic; `None` leaves a field
    /// alone. Partitions can only grow, since removing one deletes its data.
    UpdateTopic {
        stream: String,
        name: String,
        partitions: Option<u64>,
        message_expiry: Option<String>,
        max_size: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compression: Option<String>,
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

impl Op {
    /// A `CreateTopic`'s optional settings by name, for comparing and
    /// rendering them uniformly. Empty for every other op.
    pub fn topic_settings(&self) -> Vec<(&'static str, Option<&String>)> {
        match self {
            Op::CreateTopic { message_expiry, max_size, compression, segment_size, durability, .. } => {
                vec![
                    ("message_expiry", message_expiry.as_ref()),
                    ("max_size", max_size.as_ref()),
                    ("compression", compression.as_ref()),
                    ("segment_size", segment_size.as_ref()),
                    ("durability", durability.as_ref()),
                ]
            }
            _ => Vec::new(),
        }
    }

    /// True for ops that remove a resource and the messages in it.
    pub fn deletes(&self) -> bool {
        matches!(self, Op::DeleteStream { .. } | Op::DeleteTopic { .. } | Op::DeleteConsumerGroup { .. })
    }
}
