//! Uninstall as an ordered list of named steps, so the in-app button, the
//! Windows uninstaller and the CLI remove the same things in the same order.

mod footprint;

use std::{fs, io, path::Path};

use anyhow::Context;
use relay_core::ProfileName;
use relay_secrets::SecretStore;
use serde::{Deserialize, Serialize};

use crate::claude::{ClaudeClient, Clients, ServerEntry};

pub use footprint::{DELETE_DATA_LABEL, Footprint, Leftover, Roots, checklist};

/// One named piece of cleanup.
pub struct Step<'a> {
    name: String,
    action: Box<dyn FnOnce() -> anyhow::Result<()> + 'a>,
}

impl<'a> Step<'a> {
    pub fn new(name: impl Into<String>, action: impl FnOnce() -> anyhow::Result<()> + 'a) -> Self {
        Self {
            name: name.into(),
            action: Box::new(action),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

/// What one step did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub name: String,
    pub result: Result<(), String>,
}

/// Runs every step in order, carrying on past failures.
pub fn run_steps(steps: Vec<Step<'_>>) -> Vec<StepOutcome> {
    steps
        .into_iter()
        .map(|step| StepOutcome {
            result: (step.action)().map_err(|e| format!("{e:#}")),
            name: step.name,
        })
        .collect()
}

/// The cleanup for one profile: Claude entries first, then the caller's login
/// item and secrets steps, then data only if the user asked for it.
pub fn uninstall_steps<'a>(
    clients: &'a Clients,
    entry: &'a ServerEntry,
    login_item: Step<'a>,
    secrets: Step<'a>,
    delete_data: Option<Step<'a>>,
) -> Vec<Step<'a>> {
    let unregister = clients.detect().into_iter().map(|client| {
        Step::new(format!("Remove from {client}"), move || {
            clients.unregister(client, entry).map(|_| ())
        })
    });
    unregister
        .chain([login_item, secrets])
        .chain(delete_data)
        .collect()
}

/// What uninstall reaches besides files: the Claude clients, the OS secret
/// store, and the caller's login item and process control.
pub struct Machine<'a> {
    pub clients: &'a Clients,
    pub secrets: &'a dyn SecretStore,
    /// The `signal-agent` binary this install's Claude entries launch; an
    /// entry launching anything else is not ours and is left alone.
    pub agent: &'a Path,
    pub remove_login_item: Box<dyn Fn() -> anyhow::Result<()> + 'a>,
    pub stop_agents: Box<dyn Fn() -> anyhow::Result<()> + 'a>,
}

/// The steps that remove `footprint`'s leftovers for `profiles`. Messages,
/// account data and the keys that unlock them go only when `delete_data`.
pub fn removal_steps<'a>(
    footprint: &Footprint,
    profiles: &[ProfileName],
    machine: &'a Machine<'a>,
    delete_data: bool,
) -> Vec<Step<'a>> {
    footprint
        .items(profiles)
        .into_iter()
        .filter(|item| delete_data || !item.kept_with_data)
        .filter_map(|item| step(footprint, item.leftover, machine))
        .collect()
}

fn step<'a>(
    footprint: &Footprint,
    leftover: Leftover,
    machine: &'a Machine<'a>,
) -> Option<Step<'a>> {
    let name = footprint.describe(&leftover).join(": ");
    Some(match leftover {
        Leftover::LoginItem => Step::new(name, || (machine.remove_login_item)()),
        Leftover::RunningAgents => Step::new(name, || (machine.stop_agents)()),
        Leftover::ClaudeEntry { client, profile } => {
            Step::new(name, move || remove_entry(machine, client, &profile))
        }
        Leftover::Key(profile) => Step::new(name, move || machine.secrets.delete(&profile)),
        Leftover::Path(path) => Step::new(name, move || remove_path(&path)),
        Leftover::Manual(_) => return None,
    })
}

/// Removes `profile`'s entry from `client` if the client is installed and the
/// entry launches this install's agent.
fn remove_entry(
    machine: &Machine<'_>,
    client: ClaudeClient,
    profile: &ProfileName,
) -> anyhow::Result<()> {
    if !machine.clients.detect().contains(&client) {
        return Ok(());
    }
    let entry = ServerEntry::for_profile(profile, machine.agent, ["mcp", "--profile", profile])?;
    machine.clients.unregister(client, &entry).map(|_| ())
}

/// Removes the file or folder at `path`; one that isn't there is fine.
fn remove_path(path: &Path) -> anyhow::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
    };
    removed.with_context(|| format!("unable to remove \"{}\"", path.display()))
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::PathBuf};

    use relay_secrets::SecretStore as _;

    use super::*;
    use crate::claude::{Platform, RunOutput, Runner};

    struct Succeeds;

    impl Runner for Succeeds {
        fn run(&self, _: &std::path::Path, _: &[String]) -> anyhow::Result<RunOutput> {
            Ok(RunOutput {
                success: true,
                ..RunOutput::default()
            })
        }
    }

    fn entry() -> ServerEntry {
        ServerEntry {
            name: "signal-personal".into(),
            command: "/bin/signal-agent".into(),
            args: vec!["mcp".into()],
        }
    }

    /// Claude entries go first so no client relaunches the agent mid-uninstall,
    /// and data is deleted only when the user ticked the box.
    #[test]
    fn uninstall_removes_entries_first_and_data_only_on_request() {
        let home = tempfile::tempdir().expect("temp home");
        std::fs::create_dir(home.path().join("Claude")).expect("Desktop dir");
        let clients = Clients::new(
            Platform::MacOs,
            Box::new(Succeeds),
            Some(PathBuf::from("claude")),
            Some(home.path().join("Claude")),
        );
        let entry = entry();
        for delete in [false, true] {
            let ran = RefCell::new(Vec::new());
            let step = |name: &'static str| {
                let ran = &ran;
                Step::new(name, move || {
                    ran.borrow_mut().push(name);
                    Ok(())
                })
            };
            let steps = uninstall_steps(
                &clients,
                &entry,
                step("login item"),
                step("secrets"),
                delete.then(|| step("data")),
            );
            let names: Vec<_> = steps.iter().map(|s| s.name().to_owned()).collect();
            let mut expected = vec![
                "Remove from Claude Code",
                "Remove from Claude Desktop",
                "login item",
                "secrets",
            ];
            if delete {
                expected.push("data");
            }
            assert_eq!(names, expected, "delete={delete}");
        }
    }

    /// One failed step (a locked Keychain, say) must not leave the rest of the
    /// install behind; every step runs and reports its own outcome.
    #[test]
    fn a_failed_step_does_not_stop_the_rest() {
        let ran = RefCell::new(Vec::new());
        let steps = vec![
            Step::new("first", || anyhow::bail!("keychain locked")),
            Step::new("second", || {
                ran.borrow_mut().push("second");
                Ok(())
            }),
        ];
        let outcomes = run_steps(steps);
        assert_eq!(*ran.borrow(), ["second"]);
        assert_eq!(
            outcomes,
            vec![
                StepOutcome {
                    name: "first".into(),
                    result: Err("keychain locked".into())
                },
                StepOutcome {
                    name: "second".into(),
                    result: Ok(())
                },
            ]
        );
    }

    /// A macOS home in a temp dir with one profile fully installed, plus
    /// things that belong to the user or other apps.
    struct Installed {
        home: tempfile::TempDir,
        footprint: Footprint,
        clients: Clients,
        secrets: relay_secrets::MemorySecretStore,
        agent: PathBuf,
        profile: ProfileName,
    }

    impl Installed {
        fn mac() -> Self {
            let home = tempfile::tempdir().expect("temp home");
            let support = home.path().join("Library/Application Support");
            let roots = Roots {
                home: home.path().to_owned(),
                data_local: support.clone(),
                config: support.clone(),
            };
            let footprint = Footprint::new(Platform::MacOs, roots);
            let desktop = footprint.desktop_dir().expect("Desktop dir");
            std::fs::create_dir_all(&desktop).expect("Desktop dir");
            let clients = Clients::new(Platform::MacOs, Box::new(Succeeds), None, Some(desktop));
            let profile = ProfileName::default_profile();
            let agent = home
                .path()
                .join("MCP Relay for Signal.app/Contents/MacOS/signal-agent");
            let installed = Self {
                footprint,
                clients,
                secrets: relay_secrets::MemorySecretStore::default(),
                agent,
                profile,
                home,
            };
            installed.install();
            installed
        }

        fn install(&self) {
            let entry =
                ServerEntry::for_profile(&self.profile, &self.agent, ["mcp"]).expect("entry");
            let config = self
                .footprint
                .desktop_dir()
                .expect("dir")
                .join("claude_desktop_config.json");
            std::fs::write(
                &config,
                serde_json::json!({"theme": "dark", "mcpServers": {"other": {"command": "npx"}}})
                    .to_string(),
            )
            .expect("seed Desktop config");
            self.clients
                .register(ClaudeClient::Desktop, &entry)
                .expect("register");
            self.secrets
                .set(&self.profile, &relay_secrets::StoreKey::from_bytes([7; 32]))
                .expect("key");
            let paths = relay_core::ProfilePaths::under(self.footprint.data_dir(), &self.profile);
            std::fs::create_dir_all(&paths.root).expect("profile dir");
            for file in [
                &paths.message_store,
                &paths.signal_store,
                &paths.agent_log,
                &paths.settings,
            ] {
                std::fs::write(file, b"x").expect("profile file");
            }
            std::fs::write(paths.root.join("agent.sock"), b"").expect("socket");
            for file in [
                "profiles.json",
                "update-check.json",
                "login-item-supervisor",
            ] {
                std::fs::write(self.footprint.data_dir().join(file), b"{}").expect("data file");
            }
            for dir in [
                "Library/Caches/com.bluefishforsale.signalagent",
                "Library/WebKit/com.bluefishforsale.signalagent/WebsiteData",
                "Library/Caches/com.other.app",
            ] {
                std::fs::create_dir_all(self.home.path().join(dir)).expect("app dir");
            }
        }

        fn uninstall(&self, delete_data: bool) -> (Vec<StepOutcome>, Vec<&'static str>) {
            let calls = RefCell::new(Vec::new());
            let machine = Machine {
                clients: &self.clients,
                secrets: &self.secrets,
                agent: &self.agent,
                remove_login_item: Box::new(|| {
                    calls.borrow_mut().push("login item");
                    Ok(())
                }),
                stop_agents: Box::new(|| {
                    calls.borrow_mut().push("stop agents");
                    Ok(())
                }),
            };
            let steps = removal_steps(
                &self.footprint,
                std::slice::from_ref(&self.profile),
                &machine,
                delete_data,
            );
            let outcomes = run_steps(steps);
            drop(machine);
            (outcomes, calls.into_inner())
        }

        fn desktop_config(&self) -> serde_json::Value {
            let path = self
                .footprint
                .desktop_dir()
                .expect("dir")
                .join("claude_desktop_config.json");
            serde_json::from_str(&std::fs::read_to_string(path).expect("config")).expect("json")
        }
    }

    /// Keeping messages must keep exactly the profile folder and the key that
    /// unlocks it, and remove every other trace: the login item, the Claude
    /// entry (and only ours), running agents, logs, sockets and app caches.
    #[test]
    fn keeping_messages_keeps_only_the_profile_data_and_its_key() {
        let mac = Installed::mac();
        let (outcomes, calls) = mac.uninstall(false);
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:#?}");
        assert_eq!(calls, ["login item", "stop agents"]);
        assert_eq!(
            mac.desktop_config(),
            serde_json::json!({"theme": "dark", "mcpServers": {"other": {"command": "npx"}}})
        );
        assert!(
            mac.secrets.get(&mac.profile).expect("get").is_some(),
            "key deleted"
        );
        let data = mac.footprint.data_dir();
        let paths = relay_core::ProfilePaths::under(data.clone(), &mac.profile);
        for kept in [&paths.message_store, &paths.signal_store, &paths.settings] {
            assert!(kept.exists(), "{} deleted", kept.display());
        }
        for gone in [
            paths.agent_log.clone(),
            paths.root.join("agent.sock"),
            data.join("profiles.json"),
            data.join("update-check.json"),
            data.join("login-item-supervisor"),
            mac.home
                .path()
                .join("Library/Caches/com.bluefishforsale.signalagent"),
            mac.home
                .path()
                .join("Library/WebKit/com.bluefishforsale.signalagent"),
            mac.footprint
                .desktop_dir()
                .expect("dir")
                .join("claude_desktop_config.json.bak-signal-agent"),
        ] {
            assert!(!gone.exists(), "{} left behind", gone.display());
        }
        assert!(
            mac.home
                .path()
                .join("Library/Caches/com.other.app")
                .exists(),
            "another app's cache deleted"
        );
    }

    /// With the box ticked nothing of the install may remain, and running
    /// uninstall again (a retry after a failure, or the Windows uninstaller
    /// after the in-app button) must succeed and change nothing.
    #[test]
    fn deleting_messages_leaves_nothing_and_a_second_run_succeeds() {
        let mac = Installed::mac();
        for run in 1..=2 {
            let (outcomes, _) = mac.uninstall(true);
            assert!(
                outcomes.iter().all(|o| o.result.is_ok()),
                "run {run}: {outcomes:#?}"
            );
            assert!(
                mac.secrets.get(&mac.profile).expect("get").is_none(),
                "key kept"
            );
            assert!(!mac.footprint.data_dir().exists(), "data kept");
            let leftovers: Vec<_> = mac
                .footprint
                .items(std::slice::from_ref(&mac.profile))
                .into_iter()
                .filter_map(|item| match item.leftover {
                    Leftover::Path(path) if path.exists() => Some(path),
                    _ => None,
                })
                .collect();
            assert!(leftovers.is_empty(), "run {run} left {leftovers:?}");
        }
        assert_eq!(
            mac.desktop_config(),
            serde_json::json!({"theme": "dark", "mcpServers": {"other": {"command": "npx"}}})
        );
    }

    /// The checklist the owner verifies acceptance 2 against must list what
    /// uninstall really removes; it is printed from the same list, and this
    /// fails when the committed copy is stale.
    #[test]
    fn uninstall_checklist_doc_matches_what_uninstall_removes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/uninstall-checklist.md");
        let expected = checklist();
        if std::env::var_os("UPDATE_UNINSTALL_CHECKLIST").is_some() {
            std::fs::write(&path, &expected).expect("write checklist");
        }
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .replace("\r\n", "\n");
        assert!(
            committed == expected,
            "{} is stale; regenerate it with UPDATE_UNINSTALL_CHECKLIST=1 cargo test -p relay-setup uninstall_checklist",
            path.display()
        );
    }

    /// The footprint names the app's folders by its bundle identifier and the
    /// login item by the shipped plist; if either is renamed, uninstall would
    /// look in the wrong place and leave the real ones behind.
    #[test]
    fn footprint_names_agree_with_the_app_config_and_login_plist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let config: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("app/src-tauri/tauri.conf.json"))
                .expect("tauri.conf.json"),
        )
        .expect("json");
        assert_eq!(config["identifier"], footprint::APP_ID);
        let plist = root
            .join("crates/relay-agent/launchd")
            .join(format!("{}.plist", footprint::LAUNCHD_LABEL));
        assert!(
            plist.is_file(),
            "no login agent plist at {}",
            plist.display()
        );
    }
}
