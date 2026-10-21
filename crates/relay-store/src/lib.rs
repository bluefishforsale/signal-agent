//! The local message history every read tool answers from. The agent writes
//! everything it receives here, so history survives while no Claude app runs.

mod memory;

use async_trait::async_trait;
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
// NOTE: async because the SQLCipher store uses sqlx, which must share one
// libsqlite3-sys with presage-store-sqlite (see docs/slices.md); a sync store
// would need a second SQLite build and the two can't link into one binary.
#[async_trait]
pub trait MessageStore: Send + Sync {
    /// Stores `message` if its id is new and returns true; an existing row is
    /// left untouched and false is returned.
    // NOTE: never overwrite here. A resent or forged message with an existing
    // id would otherwise replace the body or clear edited/deleted state, which
    // bypasses the author checks in apply_edit and mark_deleted.
    async fn insert(&self, message: &Message) -> anyhow::Result<bool>;
    /// Fails unless `editor` wrote `target`.
    async fn apply_edit(
        &self,
        target: MessageId,
        editor: ServiceId,
        body: &str,
    ) -> anyhow::Result<()>;
    /// Fails unless `deleter` wrote `target`.
    async fn mark_deleted(&self, target: MessageId, deleter: ServiceId) -> anyhow::Result<()>;
    async fn mark_read(&self, targets: &[MessageId]) -> anyhow::Result<()>;
    /// Local-only: makes `targets` count as unread again. Nothing is sent to Signal.
    async fn mark_unread(&self, targets: &[MessageId]) -> anyhow::Result<()>;
    async fn set_title(&self, conversation: Recipient, title: &str) -> anyhow::Result<()>;
    /// Most recent first.
    async fn conversations(&self) -> anyhow::Result<Vec<Conversation>>;
    /// Newest first, strictly older than `before_ms` when given.
    async fn history(
        &self,
        conversation: Recipient,
        before_ms: Option<u64>,
        limit: u32,
    ) -> anyhow::Result<Vec<Message>>;
    /// Best match first.
    async fn search(&self, query: &SearchQuery) -> anyhow::Result<Vec<Message>>;
    async fn unread(&self) -> anyhow::Result<Vec<Message>>;
    async fn stats(&self) -> anyhow::Result<StoreStats>;
}
