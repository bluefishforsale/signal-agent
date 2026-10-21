//! Ways for an MCP client to reach a profile's agent. The agent speaks MCP
//! itself, so these are transports: stdio for local clients, HTTP for remote.

use anyhow::bail;
use relay_core::ProfileName;

/// Serves MCP on stdin/stdout by connecting to the profile's agent (starting
/// it if needed) and copying bytes both ways.
pub async fn run_stdio(profile: &ProfileName) -> anyhow::Result<()> {
    bail!("stdio front end for \"{profile}\" is not implemented yet (lane L4)")
}
