//! What a tool is and what it gets to work with.

use std::sync::Arc;

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::{SendRoots, SignalBackend};
use relay_store::MessageStore;

use crate::WebhookUrl;
use serde_json::Value;

/// Everything a tool call can touch.
#[derive(Clone)]
pub struct ToolContext {
    pub store: Arc<dyn MessageStore>,
    pub signal: Arc<dyn SignalBackend>,
    pub access: AccessMode,
    pub kind: ProfileKind,
    /// The folders a tool may send files from.
    pub send_roots: Arc<SendRoots>,
    /// The webhook from Settings, a copy: nothing a tool does to it reaches
    /// the agent's capture loop.
    pub webhook: Option<WebhookUrl>,
}

/// Why a tool call failed, in the categories an MCP client needs to tell apart.
// NOTE: this is the one typed error at the MCP seam. Internal failures stay
// anyhow and are wrapped as Internal; don't add a variant per tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("unknown tool \"{0}\"")]
    Unknown(String),
    #[error("\"{0}\" changes your Signal account, and this profile is read-only")]
    ReadOnly(&'static str),
    #[error("\"{0}\" only works on a profile with its own number, not a linked device")]
    NeedsPrimary(&'static str),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// One MCP tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the input object.
    fn input_schema(&self) -> Value;
    /// True when the tool sends, edits, deletes, or changes account state.
    fn writes(&self) -> bool;
    /// True when Signal only allows this from a primary device.
    fn needs_primary(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError>;
}
