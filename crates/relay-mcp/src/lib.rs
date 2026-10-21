//! Ways for an MCP client to reach a profile's agent. The agent speaks MCP
//! itself, so these are transports: stdio for local clients, HTTP for remote.

mod stdio;

pub use stdio::run_stdio;
