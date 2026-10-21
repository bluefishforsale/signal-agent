//! The `signal-agent` command: the background agent, the stdio MCP shim that
//! Claude clients launch, and the terminal setup flow, in one binary so every
//! platform ships a single signed executable.

use anyhow::bail;
use clap::{Parser, Subcommand};
use relay_core::prelude::*;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "signal-agent", version, about = "MCP Relay for Signal")]
struct Cli {
    /// Which account to act on.
    #[arg(long, global = true, default_value = "personal")]
    profile: String,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the background agent in the foreground.
    Agent,
    /// Serve MCP on stdin/stdout; this is what Claude clients launch.
    Mcp,
    /// Set up a profile in the terminal.
    Setup,
    /// Check every piece and repair what's missing.
    Doctor,
    /// Remove this install's agent, Claude entries and keys.
    Uninstall,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // stderr only: in `mcp` mode stdout carries the protocol, and a detached
    // agent's stderr is its log file.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let profile = ProfileName::new(cli.profile)?;
    match cli.command {
        Cmd::Agent => {
            let paths = ProfilePaths::for_profile(&profile)?;
            relay_agent::run(relay_agent::AgentConfig {
                profile,
                paths,
                access: AccessMode::Full,
            })
            .await
        }
        Cmd::Mcp => relay_mcp::run_stdio(&profile).await,
        Cmd::Setup => bail!("terminal setup is not implemented yet (lane L9)"),
        Cmd::Doctor => bail!("doctor is not implemented yet (lane L5)"),
        Cmd::Uninstall => bail!("uninstall is not implemented yet (lane L12)"),
    }
}
