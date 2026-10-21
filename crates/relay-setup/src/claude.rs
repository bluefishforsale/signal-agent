//! Making the profile's MCP server appear in Claude Code and Claude Desktop,
//! and disappear again on uninstall, without ever disturbing an entry the user
//! made themselves.

use std::path::{Path, PathBuf};

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
    /// An absolute path: Claude Desktop launches servers without a shell PATH.
    pub command: String,
    pub args: Vec<String>,
}

/// The operating system whose conventions apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    Linux,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// What a finished process reported.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs external programs. The real one spawns processes; tests inject a fake.
pub trait Runner: Send + Sync {
    fn run(&self, program: &Path, args: &[String]) -> anyhow::Result<RunOutput>;
}

/// What registering did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Registration {
    Added,
    /// An entry pointing at this install was already there.
    Present,
    /// An entry with the same name points elsewhere; it was left untouched.
    Foreign,
}

/// Whose entry, if any, a client has under a server's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryStatus {
    Missing,
    Ours,
    Foreign,
}

/// The Claude clients on this machine and how to reach them.
pub struct Clients {
    platform: Platform,
    runner: Box<dyn Runner>,
    claude_cli: Option<PathBuf>,
    desktop_dir: Option<PathBuf>,
}

impl Clients {
    /// Clients reached through `runner`, with Claude Code's CLI at `claude_cli`
    /// and Claude Desktop's config directory at `desktop_dir`, when they exist.
    pub fn new(
        platform: Platform,
        runner: Box<dyn Runner>,
        claude_cli: Option<PathBuf>,
        desktop_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            platform,
            runner,
            claude_cli,
            desktop_dir,
        }
    }

    /// The Claude clients installed for this user.
    pub fn detect(&self) -> Vec<ClaudeClient> {
        let code = self.claude_cli.is_some().then_some(ClaudeClient::Code);
        let desktop = self
            .desktop_config()
            .is_some()
            .then_some(ClaudeClient::Desktop);
        code.into_iter().chain(desktop).collect()
    }

    /// Adds `entry` to `client` unless an entry with that name already exists.
    pub fn register(
        &self,
        client: ClaudeClient,
        entry: &ServerEntry,
    ) -> anyhow::Result<Registration> {
        match self.status(client, entry)? {
            EntryStatus::Ours => return Ok(Registration::Present),
            EntryStatus::Foreign => return Ok(Registration::Foreign),
            EntryStatus::Missing => {}
        }
        match client {
            ClaudeClient::Code => {
                let mut args = strings(&["mcp", "add", "-s", "user", &entry.name, "--"]);
                args.push(entry.command.clone());
                args.extend(entry.args.iter().cloned());
                self.claude(&args)?;
            }
            ClaudeClient::Desktop => bail!("Desktop registration is not implemented yet"),
        }
        Ok(Registration::Added)
    }

    /// Whether `client` has an entry named like `entry`, and whose it is.
    fn status(&self, client: ClaudeClient, entry: &ServerEntry) -> anyhow::Result<EntryStatus> {
        match client {
            ClaudeClient::Code => {
                // NOTE: `claude mcp get` launches the server to health-check it, so
                // only its exit status and the command line are trusted here,
                // never the connection status it prints.
                let out = self
                    .runner
                    .run(self.cli()?, &strings(&["mcp", "get", &entry.name]))?;
                if !out.success {
                    return Ok(EntryStatus::Missing);
                }
                Ok(if out.stdout.contains(&entry.command) {
                    EntryStatus::Ours
                } else {
                    EntryStatus::Foreign
                })
            }
            ClaudeClient::Desktop => Ok(EntryStatus::Missing),
        }
    }

    /// Runs the `claude` CLI, failing with its output when it fails.
    fn claude(&self, args: &[String]) -> anyhow::Result<RunOutput> {
        let out = self.runner.run(self.cli()?, args)?;
        if !out.success {
            bail!(
                "`claude {}` failed: {}{}",
                args.join(" "),
                out.stdout.trim(),
                out.stderr.trim()
            );
        }
        Ok(out)
    }

    fn cli(&self) -> anyhow::Result<&Path> {
        let Some(cli) = &self.claude_cli else {
            bail!("Claude Code is not installed");
        };
        Ok(cli)
    }

    /// Desktop's config file, when Desktop is installed.
    fn desktop_config(&self) -> Option<PathBuf> {
        let dir = self.desktop_dir.as_ref().filter(|d| d.is_dir())?;
        Some(dir.join("claude_desktop_config.json"))
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use super::*;

    /// A stand-in for every external program: the `claude` CLI keeps its servers
    /// in memory, and every call is recorded.
    #[derive(Clone, Default)]
    struct Fake {
        servers: Arc<Mutex<BTreeMap<String, (String, Vec<String>)>>>,
        calls: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl Fake {
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl Runner for Fake {
        fn run(&self, program: &Path, args: &[String]) -> anyhow::Result<RunOutput> {
            let mut call = vec![program.display().to_string()];
            call.extend(args.iter().cloned());
            self.calls.lock().expect("calls lock").push(call);
            let mut servers = self.servers.lock().expect("servers lock");
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = match args.as_slice() {
                ["mcp", "get", name] => match servers.get(*name) {
                    Some((command, args)) => RunOutput {
                        success: true,
                        stdout: format!(
                            "{name}:\n  Scope: User config\n  Status: ✘ Failed to connect\n  Type: stdio\n  Command: {command}\n  Args: {}\n",
                            args.join(" ")
                        ),
                        stderr: String::new(),
                    },
                    None => RunOutput {
                        success: false,
                        stdout: format!("No MCP server named \"{name}\"."),
                        stderr: String::new(),
                    },
                },
                ["mcp", "add", "-s", "user", name, "--", command, rest @ ..] => {
                    let rest = rest.iter().map(|a| a.to_string()).collect();
                    servers.insert(name.to_string(), (command.to_string(), rest));
                    RunOutput {
                        success: true,
                        ..RunOutput::default()
                    }
                }
                ["mcp", "remove", "-s", "user", name] => RunOutput {
                    success: servers.remove(*name).is_some(),
                    ..RunOutput::default()
                },
                _ => RunOutput {
                    success: true,
                    ..RunOutput::default()
                },
            };
            Ok(output)
        }
    }

    struct Machine {
        _home: tempfile::TempDir,
        desktop_dir: PathBuf,
        fake: Fake,
        clients: Clients,
    }

    /// A machine rooted in a temp dir, with Claude Code and Claude Desktop
    /// installed or not.
    fn machine(platform: Platform, code: bool, desktop: bool) -> Machine {
        let home = tempfile::tempdir().expect("temp home");
        let desktop_dir = home.path().join("Claude");
        if desktop {
            std::fs::create_dir(&desktop_dir).expect("create Desktop dir");
        }
        let fake = Fake::default();
        let clients = Clients::new(
            platform,
            Box::new(fake.clone()),
            code.then(|| PathBuf::from("claude")),
            Some(desktop_dir.clone()),
        );
        Machine {
            _home: home,
            desktop_dir,
            fake,
            clients,
        }
    }

    /// Code counts as installed when its CLI was found, Desktop when its config
    /// directory exists; neither is assumed.
    #[test]
    fn detection_reports_only_installed_clients() {
        let cases = [
            (false, false, vec![]),
            (true, false, vec![ClaudeClient::Code]),
            (false, true, vec![ClaudeClient::Desktop]),
            (true, true, vec![ClaudeClient::Code, ClaudeClient::Desktop]),
        ];
        for (code, desktop, expected) in cases {
            let m = machine(Platform::MacOs, code, desktop);
            assert_eq!(
                m.clients.detect(),
                expected,
                "code={code} desktop={desktop}"
            );
        }
    }

    fn entry() -> ServerEntry {
        ServerEntry {
            name: "signal-personal".into(),
            command: "/Applications/MCP Relay for Signal.app/Contents/MacOS/signal-agent".into(),
            args: vec!["mcp".into(), "--profile".into(), "personal".into()],
        }
    }

    /// A missing Code entry is added at user scope, so it works from every project.
    #[test]
    fn code_registration_adds_a_missing_entry_at_user_scope() {
        let m = machine(Platform::MacOs, true, false);
        let added = m
            .clients
            .register(ClaudeClient::Code, &entry())
            .expect("register");
        assert_eq!(added, Registration::Added);
        let e = entry();
        let mut add = vec![
            "claude", "mcp", "add", "-s", "user", &e.name, "--", &e.command,
        ];
        add.extend(e.args.iter().map(String::as_str));
        assert!(
            m.fake.calls().iter().any(|c| *c == add),
            "no user-scope add in {:?}",
            m.fake.calls()
        );
    }
}
