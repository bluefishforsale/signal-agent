//! Making the profile's MCP server appear in Claude Code and Claude Desktop,
//! and disappear again on uninstall, without ever disturbing an entry the user
//! made themselves.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
            ClaudeClient::Desktop => {
                let mut config = DesktopConfig::load(self.desktop_path()?)?;
                config.servers_mut()?.insert(
                    entry.name.clone(),
                    serde_json::json!({"command": entry.command, "args": entry.args}),
                );
                config.save()?;
            }
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
            ClaudeClient::Desktop => {
                let config = DesktopConfig::load(self.desktop_path()?)?;
                Ok(match config.servers()?.and_then(|s| s.get(&entry.name)) {
                    None => EntryStatus::Missing,
                    Some(server) if points_at(server, &entry.command) => EntryStatus::Ours,
                    Some(_) => EntryStatus::Foreign,
                })
            }
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

    fn desktop_path(&self) -> anyhow::Result<PathBuf> {
        let Some(path) = self.desktop_config() else {
            bail!("Claude Desktop is not installed");
        };
        Ok(path)
    }

    /// Desktop's config file, when Desktop is installed.
    fn desktop_config(&self) -> Option<PathBuf> {
        let dir = self.desktop_dir.as_ref().filter(|d| d.is_dir())?;
        Some(dir.join("claude_desktop_config.json"))
    }
}

/// Claude Desktop's config file, edited without disturbing anything else in it.
struct DesktopConfig {
    path: PathBuf,
    original: Option<String>,
    root: serde_json::Map<String, Value>,
}

impl DesktopConfig {
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let original = match fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(e).with_context(|| format!("unable to read \"{}\"", path.display()));
            }
        };
        let root = match original.as_deref().map(str::trim) {
            None | Some("") => serde_json::Map::new(),
            Some(text) => {
                let value: Value = serde_json::from_str(text).with_context(|| {
                    format!(
                        "\"{}\" is not valid JSON; fix or remove it, then try again",
                        path.display()
                    )
                })?;
                let Value::Object(root) = value else {
                    bail!(
                        "\"{}\" is not a JSON object; fix or remove it, then try again",
                        path.display()
                    );
                };
                root
            }
        };
        Ok(Self {
            path,
            original,
            root,
        })
    }

    fn servers(&self) -> anyhow::Result<Option<&serde_json::Map<String, Value>>> {
        match self.root.get(SERVERS) {
            None => Ok(None),
            Some(Value::Object(servers)) => Ok(Some(servers)),
            Some(_) => bail!(
                "\"{}\" has a \"{SERVERS}\" that is not an object",
                self.path.display()
            ),
        }
    }

    fn servers_mut(&mut self) -> anyhow::Result<&mut serde_json::Map<String, Value>> {
        let servers = self
            .root
            .entry(SERVERS)
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        let Value::Object(servers) = servers else {
            bail!(
                "\"{}\" has a \"{SERVERS}\" that is not an object",
                self.path.display()
            );
        };
        Ok(servers)
    }

    /// Backs up the file as it was, then replaces it in one rename so Desktop
    /// never reads a half-written config.
    fn save(&self) -> anyhow::Result<()> {
        if let Some(original) = &self.original {
            let backup = sibling(&self.path, ".bak-signal-agent");
            fs::write(&backup, original)
                .with_context(|| format!("unable to back up to \"{}\"", backup.display()))?;
        }
        let mut text = serde_json::to_string_pretty(&self.root)?;
        text.push('\n');
        let tmp = sibling(&self.path, ".tmp-signal-agent");
        fs::write(&tmp, text).with_context(|| format!("unable to write \"{}\"", tmp.display()))?;
        fs::rename(&tmp, &self.path)
            .with_context(|| format!("unable to replace \"{}\"", self.path.display()))
    }
}

const SERVERS: &str = "mcpServers";

/// Whether a Desktop server entry launches `binary`, directly or as an argument
/// to a wrapper.
fn points_at(server: &Value, binary: &str) -> bool {
    let command = server.get("command").and_then(Value::as_str);
    let args = server
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    command
        .into_iter()
        .chain(args.filter_map(Value::as_str))
        .any(|s| s.contains(binary))
}

/// `path` with `suffix` appended to its file name.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
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

    /// An existing Code entry with our name is never replaced, whether it points
    /// at this install or at something the user set up by hand.
    #[test]
    fn code_registration_never_overwrites_an_existing_entry() {
        for (command, expected) in [
            (entry().command, Registration::Present),
            (
                "/usr/local/bin/someone-elses-signal".to_owned(),
                Registration::Foreign,
            ),
        ] {
            let m = machine(Platform::MacOs, true, false);
            m.fake
                .servers
                .lock()
                .expect("servers lock")
                .insert(entry().name, (command.clone(), vec![]));
            let result = m
                .clients
                .register(ClaudeClient::Code, &entry())
                .expect("register");
            assert_eq!(result, expected, "existing command {command}");
            let adds: Vec<_> = m
                .fake
                .calls()
                .into_iter()
                .filter(|c| c.get(2).is_some_and(|a| a == "add"))
                .collect();
            assert!(adds.is_empty(), "overwrote {command}: {adds:?}");
            assert_eq!(
                m.fake
                    .servers
                    .lock()
                    .expect("servers lock")
                    .get(&entry().name)
                    .map(|s| &s.0),
                Some(&command)
            );
        }
    }

    fn read_json(path: &Path) -> serde_json::Value {
        let text = std::fs::read_to_string(path).expect("read config");
        serde_json::from_str(&text).expect("config is JSON")
    }

    /// Desktop launches the server from its config file, creating it if Desktop
    /// never wrote one.
    #[test]
    fn desktop_registration_creates_a_missing_config() {
        let m = machine(Platform::MacOs, false, true);
        let result = m
            .clients
            .register(ClaudeClient::Desktop, &entry())
            .expect("register");
        assert_eq!(result, Registration::Added);
        let e = entry();
        assert_eq!(
            read_json(&m.desktop_dir.join("claude_desktop_config.json")),
            serde_json::json!({"mcpServers": {e.name: {"command": e.command, "args": e.args}}})
        );
    }

    /// Registration must keep every other key and server exactly, in the user's
    /// order, and leave the previous file as a backup before replacing it.
    #[test]
    fn desktop_registration_preserves_the_config_and_backs_it_up_first() {
        let m = machine(Platform::MacOs, false, true);
        let path = m.desktop_dir.join("claude_desktop_config.json");
        let original = r#"{"zeta": {"keep": [1, 2]}, "mcpServers": {"other": {"command": "npx", "args": ["x"], "env": {"K": "v"}}}, "alpha": true}"#;
        std::fs::write(&path, original).expect("seed config");

        m.clients
            .register(ClaudeClient::Desktop, &entry())
            .expect("register");

        let backup = m
            .desktop_dir
            .join("claude_desktop_config.json.bak-signal-agent");
        assert_eq!(std::fs::read_to_string(&backup).expect("backup"), original);
        let config = read_json(&path);
        let keys: Vec<_> = config
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            keys,
            ["zeta", "mcpServers", "alpha"],
            "top-level order changed"
        );
        assert_eq!(config["zeta"], serde_json::json!({"keep": [1, 2]}));
        assert_eq!(config["alpha"], serde_json::json!(true));
        assert_eq!(
            config["mcpServers"]["other"],
            serde_json::json!({"command": "npx", "args": ["x"], "env": {"K": "v"}})
        );
        assert_eq!(
            config["mcpServers"][entry().name]["command"],
            serde_json::json!(entry().command)
        );
        let leftovers: Vec<_> = std::fs::read_dir(&m.desktop_dir)
            .expect("list dir")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    /// An existing Desktop entry with our name is never replaced, and the file
    /// isn't rewritten or backed up at all when there's nothing to add.
    #[test]
    fn desktop_registration_never_overwrites_an_existing_entry() {
        for (command, expected) in [
            (entry().command, Registration::Present),
            ("/opt/other/signal".to_owned(), Registration::Foreign),
        ] {
            let m = machine(Platform::MacOs, false, true);
            let path = m.desktop_dir.join("claude_desktop_config.json");
            let original =
                serde_json::json!({"mcpServers": {entry().name: {"command": command}}}).to_string();
            std::fs::write(&path, &original).expect("seed config");

            let result = m
                .clients
                .register(ClaudeClient::Desktop, &entry())
                .expect("register");

            assert_eq!(result, expected, "existing command {command}");
            assert_eq!(std::fs::read_to_string(&path).expect("config"), original);
            assert!(
                !m.desktop_dir
                    .join("claude_desktop_config.json.bak-signal-agent")
                    .exists()
            );
        }
    }

    /// A config that isn't valid JSON is refused with an error naming the file,
    /// rather than replaced by one that drops whatever the user had in it.
    #[test]
    fn desktop_registration_refuses_invalid_json_and_leaves_it_alone() {
        for original in ["{\"mcpServers\": {", "[1, 2]", "{\"mcpServers\": []}"] {
            let m = machine(Platform::MacOs, false, true);
            let path = m.desktop_dir.join("claude_desktop_config.json");
            std::fs::write(&path, original).expect("seed config");

            let err = m
                .clients
                .register(ClaudeClient::Desktop, &entry())
                .expect_err("registered into a broken config");

            assert!(
                format!("{err:#}").contains("claude_desktop_config.json"),
                "error doesn't name the file: {err:#}"
            );
            assert_eq!(std::fs::read_to_string(&path).expect("config"), original);
        }
    }
}
