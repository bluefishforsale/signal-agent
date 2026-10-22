//! The `signal-agent` command: the background agent, the stdio MCP shim that
//! Claude clients launch, and the terminal setup flow, in one binary so every
//! platform ships a single signed executable.

mod setup_tty;

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
    Agent {
        /// Run every set-up profile's agent as a child process instead; this
        /// is what the login item starts.
        #[arg(long)]
        all_profiles: bool,
    },
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
        Cmd::Agent { all_profiles: true } => relay_agent::run_all_profiles().await,
        Cmd::Agent {
            all_profiles: false,
        } => {
            let paths = ProfilePaths::for_profile(&profile)?;
            let access = startup_access(&paths);
            relay_agent::run(relay_agent::AgentConfig {
                profile,
                paths,
                access,
            })
            .await
        }
        Cmd::Mcp => relay_mcp::run_stdio(&profile).await,
        Cmd::Setup => setup_tty::run(profile).await,
        Cmd::Doctor => bail!("doctor is not implemented yet (lane L5)"),
        Cmd::Uninstall => bail!("uninstall is not implemented yet (lane L12)"),
    }
}

/// The access mode the user chose in setup.
// NOTE: anything but a readable saved choice starts read-only. Defaulting to
// Full would let Claude send as a user who answered No.
fn startup_access(paths: &ProfilePaths) -> AccessMode {
    paths.load_access().unwrap_or_else(|e| {
        tracing::warn!("starting read-only: {e:#}");
        AccessMode::ReadOnly
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> ProfilePaths {
        ProfilePaths::under(dir.path().to_path_buf(), &ProfileName::default_profile())
    }

    /// Answering No to "Let Claude send messages as you?" must reach the agent.
    #[test]
    fn a_profile_whose_access_choice_was_no_starts_its_agent_read_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        paths.save_access(AccessMode::ReadOnly).expect("save");
        assert_eq!(startup_access(&paths), AccessMode::ReadOnly);
        paths.save_access(AccessMode::Full).expect("save");
        assert_eq!(startup_access(&paths), AccessMode::Full);
    }

    /// Every OS's login item runs exactly this command line, so it must parse
    /// as the supervisor, not as the default profile's agent.
    #[test]
    fn the_login_items_command_line_runs_the_supervisor() {
        let cli = Cli::try_parse_from(["signal-agent", "agent", "--all-profiles"]).expect("parse");
        assert!(
            matches!(cli.command, Cmd::Agent { all_profiles: true }),
            "parsed as a single profile's agent"
        );
    }

    /// A missing or damaged choice must never grant write access.
    #[test]
    fn a_missing_or_unreadable_access_choice_starts_read_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        assert_eq!(startup_access(&paths), AccessMode::ReadOnly, "missing");
        paths.save_access(AccessMode::Full).expect("save");
        let file = std::fs::read_dir(&paths.root)
            .expect("root")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .next()
            .expect("saved file");
        std::fs::write(file, "full").expect("damage");
        assert_eq!(startup_access(&paths), AccessMode::ReadOnly, "damaged");
    }
}
