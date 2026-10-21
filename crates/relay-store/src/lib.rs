//! The local message history every read tool answers from. The agent writes
//! everything it receives here, so history survives while no Claude app runs.

mod memory;

use relay_core::prelude::*;
use serde::{Deserialize, Serialize};

pub use memory::MemoryStore;

/// A full-text search with optional filters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub sender: Option<ServiceId>,
    pub conversation: Option<Recipient>,
    pub since_ms: Option<u64>,
    /// Exclusive.
    pub until_ms: Option<u64>,
    pub limit: u32,
    pub offset: u32,
}

/// Counts and bounds of what's stored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreStats {
    pub messages: u64,
    pub conversations: u64,
    pub unread: u64,
    pub oldest_ms: Option<u64>,
    pub newest_ms: Option<u64>,
}

/// One profile's message history.
pub trait MessageStore: Send + Sync {
    /// Inserts or replaces the message with the same id.
    fn upsert(&self, message: &Message) -> anyhow::Result<()>;
    /// Fails unless `editor` wrote `target`.
    fn apply_edit(&self, target: MessageId, editor: ServiceId, body: &str) -> anyhow::Result<()>;
    /// Fails unless `deleter` wrote `target`.
    fn mark_deleted(&self, target: MessageId, deleter: ServiceId) -> anyhow::Result<()>;
    fn mark_read(&self, targets: &[MessageId]) -> anyhow::Result<()>;
    fn set_title(&self, conversation: Recipient, title: &str) -> anyhow::Result<()>;
    /// Most recent first.
    fn conversations(&self) -> anyhow::Result<Vec<Conversation>>;
    /// Newest first, strictly older than `before_ms` when given.
    fn history(
        &self,
        conversation: Recipient,
        before_ms: Option<u64>,
        limit: u32,
    ) -> anyhow::Result<Vec<Message>>;
    /// Best match first.
    fn search(&self, query: &SearchQuery) -> anyhow::Result<Vec<Message>>;
    fn unread(&self) -> anyhow::Result<Vec<Message>>;
    fn stats(&self) -> anyhow::Result<StoreStats>;
}
