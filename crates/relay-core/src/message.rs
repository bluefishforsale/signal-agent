//! What a stored message and a conversation look like to every consumer: the
//! store persists these, tools return them, the backend produces them.

use serde::{Deserialize, Serialize};

use crate::{MessageId, Recipient, ServiceId};

/// A message as stored and returned to MCP clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub conversation: Recipient,
    pub sender: ServiceId,
    pub from_me: bool,
    pub received_at_ms: u64,
    pub body: Option<String>,
    pub attachments: Vec<AttachmentRef>,
    pub quote: Option<MessageId>,
    pub edited: bool,
    pub deleted: bool,
}

/// An attachment's metadata; the bytes are fetched on demand by id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub id: String,
    pub content_type: String,
    pub file_name: Option<String>,
    pub size: u64,
}

/// One conversation's summary row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub recipient: Recipient,
    pub title: String,
    pub last_message_at_ms: Option<u64>,
    pub unread: u32,
}
