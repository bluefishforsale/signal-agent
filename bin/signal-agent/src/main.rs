//! The `signal-agent` command: the background agent, the stdio MCP shim that
//! Claude clients launch, and the terminal setup flow, in one binary so every
//! platform ships a single signed executable.

mod doctor;
mod http_cli;
mod setup_tty;

use std::io::Write;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use relay_core::prelude::*;
use relay_setup::KeyAction;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "signal-agent", version, about = "MCP Relay for Signal")]
struct Cli {
    /// Which account to act on: `personal` unless named, except that doctor
    /// checks every account unless one is named.
    #[arg(long, global = true)]
    profile: Option<String>,
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
    /// Check every piece and say what's wrong; never contacts Signal.
    Doctor {
        /// Put back missing Claude entries and the login item. Never touches
        /// messages, keys or the Signal link.
        #[arg(long)]
        fix: bool,
    },
    /// Remove this install's agent, Claude entries and files from this computer.
    Uninstall {
        /// Also delete messages, account data and the keys that unlock them.
        #[arg(long)]
        delete_data: bool,
    },
    /// The HTTP endpoint for remote MCP clients (Advanced).
    #[command(subcommand)]
    Http(http_cli::HttpCmd),
    /// The setup app's way to a profile's database key, so only this program
    /// ever touches the OS secret store.
    #[command(hide = true)]
    Key {
        #[command(subcommand)]
        action: KeyCmd,
    },
}

#[derive(Clone, Copy, Subcommand)]
enum KeyCmd {
    Get,
    Ensure,
    Delete,
}

impl From<KeyCmd> for KeyAction {
    fn from(cmd: KeyCmd) -> Self {
        match cmd {
            KeyCmd::Get => Self::Get,
            KeyCmd::Ensure => Self::Ensure,
            KeyCmd::Delete => Self::Delete,
        }
    }
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
    let named = cli.profile.map(ProfileName::new).transpose()?;
    let profile = named.clone().unwrap_or_else(ProfileName::default_profile);
    match cli.command {
        Cmd::Agent { all_profiles: true } => relay_agent::run_all_profiles().await,
        Cmd::Agent {
            all_profiles: false,
        } => {
            let paths = ProfilePaths::for_profile(&profile)?;
            let access = startup_access(&paths);
            let (http_profile, http_paths) = (profile.clone(), paths.clone());
            // NOTE: the endpoint failing must never stop the agent, which is
            // also what captures messages.
            tokio::spawn(async move {
                if let Err(e) = relay_mcp::run_http(&http_profile, &http_paths).await {
                    tracing::error!("the HTTP endpoint stopped: {e:#}");
                }
            });
            relay_agent::run(relay_agent::AgentConfig {
                profile,
                paths,
                access,
            })
            .await
        }
        Cmd::Mcp => relay_mcp::run_stdio(&profile).await,
        Cmd::Setup => setup_tty::run(profile).await,
        Cmd::Doctor { fix } => doctor::run(named.as_ref(), fix).await,
        Cmd::Uninstall { delete_data } => uninstall(delete_data),
        Cmd::Http(cmd) => {
            let paths = ProfilePaths::for_profile(&profile)?;
            http_cli::run(cmd, &paths, &mut std::io::stdout())
        }
        Cmd::Key { action } => {
            let store = relay_secrets::platform_store()?;
            let printed = relay_setup::serve_key(action.into(), &profile, store.as_ref())?;
            std::io::stdout()
                .write_all(printed.as_bytes())
                .context("unable to hand the key to the setup app")
        }
    }
}

/// Removes every profile's agent, Claude entries and files, reporting each
/// step; the Windows uninstaller runs this too.
fn uninstall(delete_data: bool) -> anyhow::Result<()> {
    let agent = std::env::current_exe().context("unable to locate this executable")?;
    let secrets = relay_secrets::platform_store()?;
    let outcomes = relay_setup::uninstall(
        &agent,
        relay_agent::login::unregister,
        secrets.as_ref(),
        delete_data,
    )?;
    let mut failed = 0;
    for outcome in &outcomes {
        match &outcome.result {
            Ok(()) => println!("done    {}", outcome.name),
            Err(e) => {
                failed += 1;
                eprintln!("FAILED  {}: {e}", outcome.name);
            }
        }
    }
    if failed > 0 {
        bail!(
            "{failed} of {} steps failed; fix what they name and run `signal-agent uninstall` again",
            outcomes.len()
        );
    }
    Ok(())
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

    /// The setup app reaches each profile's key only through these exact
    /// arguments, so every one must parse as that key action for that profile.
    #[test]
    fn the_setup_apps_key_requests_parse_as_key_actions() {
        let work = ProfileName::new("work").expect("valid name");
        for action in [KeyAction::Get, KeyAction::Ensure, KeyAction::Delete] {
            let args = std::iter::once("signal-agent".to_owned()).chain(action.args(&work));
            let cli = Cli::try_parse_from(args).expect("parse");
            assert_eq!(cli.profile.as_deref(), Some("work"));
            assert!(
                matches!(cli.command, Cmd::Key { action: a } if KeyAction::from(a) == action),
                "{action:?}"
            );
        }
    }

    /// Doctor checks every profile unless one is named, and repairs only
    /// when asked to.
    #[test]
    fn doctor_checks_every_profile_unless_named_and_fixes_only_when_asked() {
        let cli = Cli::try_parse_from(["signal-agent", "doctor"]).expect("parse");
        assert!(cli.profile.is_none(), "doctor defaulted to one profile");
        assert!(matches!(cli.command, Cmd::Doctor { fix: false }));
        let cli = Cli::try_parse_from(["signal-agent", "doctor", "--profile", "work", "--fix"])
            .expect("parse");
        assert_eq!(cli.profile.as_deref(), Some("work"));
        assert!(matches!(cli.command, Cmd::Doctor { fix: true }));
    }

    /// The Windows uninstaller passes the "delete app data" checkbox through
    /// as this flag; without it nothing of the user's messages may go.
    #[test]
    fn uninstall_deletes_data_only_when_asked() {
        for (args, expected) in [
            (&["signal-agent", "uninstall"][..], false),
            (&["signal-agent", "uninstall", "--delete-data"][..], true),
        ] {
            let cli = Cli::try_parse_from(args).expect("parse");
            assert!(
                matches!(cli.command, Cmd::Uninstall { delete_data } if delete_data == expected),
                "{args:?}"
            );
        }
    }
}
