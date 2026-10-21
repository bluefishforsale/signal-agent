//! The one process per profile that holds the Signal connection. Everything
//! else (stdio shims, the HTTP front end, setup) reaches Signal through it,
//! because two processes driving one Signal device corrupt its sessions.

mod capture;
pub mod login;

use anyhow::bail;
use relay_core::prelude::*;

/// What the agent needs to start.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub profile: ProfileName,
    pub paths: ProfilePaths,
    pub access: AccessMode,
}

/// Runs the agent until it is told to stop: receives continuously into the
/// store and serves MCP JSON-RPC on the profile's user-only local endpoint.
pub async fn run(config: AgentConfig) -> anyhow::Result<()> {
    bail!(
        "agent for profile \"{}\" is not implemented yet (lane L4)",
        config.profile
    )
}
