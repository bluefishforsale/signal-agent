//! Making the profile's MCP server appear in Claude Code and Claude Desktop,
//! and disappear again on uninstall.

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// A Claude app that can host MCP servers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeClient {
    Code,
    Desktop,
}

/// How a client should launch this profile's server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerEntry {
    /// The client-side name, `signal-<profile>`.
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

/// The Claude clients installed for this user.
pub fn detect() -> anyhow::Result<Vec<ClaudeClient>> {
    bail!("Claude client detection is not implemented yet (lane L5)")
}

/// Adds `entry` to `client` unless an entry with that name already exists.
pub fn register(client: ClaudeClient, entry: &ServerEntry) -> anyhow::Result<()> {
    bail!(
        "registering \"{}\" with {client:?} is not implemented yet (lane L5)",
        entry.name
    )
}

/// Removes the entry named `name` from `client`, only if it points at this install.
pub fn unregister(client: ClaudeClient, name: &str) -> anyhow::Result<()> {
    bail!("unregistering \"{name}\" from {client:?} is not implemented yet (lane L5)")
}
