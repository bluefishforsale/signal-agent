//! Everything an install leaves on a machine, as data. Uninstall removes
//! exactly this list and the acceptance checklist is printed from it, so the
//! two can't drift apart.

use std::path::PathBuf;

use relay_core::{ProfileName, ProfilePaths};

use crate::claude::{ClaudeClient, Platform};

/// The bundle identifier in `app/src-tauri/tauri.conf.json`, which names the
/// app's own WebView and cache folders.
pub(crate) const APP_ID: &str = "com.bluefishforsale.signalagent";

/// The launchd label in the app bundle's login agent plist.
pub(crate) const LAUNCHD_LABEL: &str = "com.bluefishforsale.signalagent.agent";

/// The words on the checkbox that deletes messages, account data and keys.
pub const DELETE_DATA_LABEL: &str =
    "Also delete my messages, account data and the keys that unlock them";

/// The per-user folders an install writes under, as the OS names them.
#[derive(Clone, Debug)]
pub struct Roots {
    pub home: PathBuf,
    /// Application Support on macOS, `%LOCALAPPDATA%` on Windows,
    /// `$XDG_DATA_HOME` on Linux.
    pub data_local: PathBuf,
    /// Application Support on macOS, `%APPDATA%` on Windows,
    /// `$XDG_CONFIG_HOME` on Linux.
    pub config: PathBuf,
}

impl Roots {
    /// This user's folders.
    pub fn system() -> anyhow::Result<Self> {
        use anyhow::Context;
        Ok(Self {
            home: dirs::home_dir().context("no home directory")?,
            data_local: dirs::data_local_dir().context("no per-user data directory")?,
            config: dirs::config_dir().context("no per-user config directory")?,
        })
    }
}

/// One thing an install leaves behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Leftover {
    /// The login item that starts the agents at login.
    LoginItem,
    /// Agents still running outside the login item, e.g. started by a Claude
    /// client's shim.
    RunningAgents,
    /// This install's MCP server entry in a Claude client.
    ClaudeEntry {
        client: ClaudeClient,
        profile: ProfileName,
    },
    /// A profile's database key in the OS secret store.
    Key(ProfileName),
    /// A file or folder this install created, with everything under it.
    Path(PathBuf),
    /// Something only the user can remove.
    Manual(&'static str),
}

/// A leftover and whether choosing to keep messages keeps it too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub leftover: Leftover,
    pub kept_with_data: bool,
}

impl Item {
    fn always(leftover: Leftover) -> Self {
        Self {
            leftover,
            kept_with_data: false,
        }
    }

    fn with_data(leftover: Leftover) -> Self {
        Self {
            leftover,
            kept_with_data: true,
        }
    }
}

/// Where one OS keeps this install's pieces.
#[derive(Clone, Debug)]
pub struct Footprint {
    platform: Platform,
    roots: Roots,
}

impl Footprint {
    pub fn new(platform: Platform, roots: Roots) -> Self {
        Self { platform, roots }
    }

    /// The folder every profile lives under.
    pub fn data_dir(&self) -> PathBuf {
        self.roots.data_local.join("signal-agent")
    }

    /// Where the app remembers when it last looked for an update.
    pub fn update_check_file(&self) -> PathBuf {
        self.data_dir().join("update-check.json")
    }

    /// Claude Desktop's config folder, on the OSes Desktop runs on.
    pub fn desktop_dir(&self) -> Option<PathBuf> {
        match self.platform {
            Platform::MacOs | Platform::Windows => Some(self.roots.config.join("Claude")),
            Platform::Linux => None,
        }
    }

    /// Every leftover for `profiles`, in the order uninstall removes them.
    // NOTE: the order matters. The profile list goes first so the supervisor
    // stops restarting agents, the login item next so nothing starts at
    // login, Claude entries before running agents are stopped so no client
    // starts a fresh one afterwards, and files last so no agent rewrites them.
    pub fn items(&self, profiles: &[ProfileName]) -> Vec<Item> {
        let data = self.data_dir();
        let mut items = vec![
            Item::always(Leftover::Path(data.join("profiles.json"))),
            Item::always(Leftover::LoginItem),
        ];
        let clients: &[ClaudeClient] = match self.platform {
            Platform::MacOs | Platform::Windows => &[ClaudeClient::Code, ClaudeClient::Desktop],
            Platform::Linux => &[ClaudeClient::Code],
        };
        for client in clients {
            items.extend(profiles.iter().map(|profile| {
                Item::always(Leftover::ClaudeEntry {
                    client: *client,
                    profile: profile.clone(),
                })
            }));
        }
        if let Some(desktop) = self.desktop_dir() {
            items.extend(
                [
                    "claude_desktop_config.json.bak-signal-agent",
                    "claude_desktop_config.json.tmp-signal-agent",
                ]
                .map(|name| Item::always(Leftover::Path(desktop.join(name)))),
            );
        }
        items.push(Item::always(Leftover::RunningAgents));
        items.extend(
            profiles
                .iter()
                .map(|profile| Item::with_data(Leftover::Key(profile.clone()))),
        );
        for profile in profiles {
            let paths = ProfilePaths::under(data.clone(), profile);
            items.push(Item::always(Leftover::Path(paths.agent_log)));
            if self.platform != Platform::Windows {
                items.push(Item::always(Leftover::Path(paths.root.join("agent.sock"))));
            }
        }
        if self.platform != Platform::Linux {
            items.push(Item::always(Leftover::Path(self.update_check_file())));
        }
        if self.platform == Platform::MacOs {
            items.push(Item::always(Leftover::Path(
                data.join("login-item-supervisor"),
            )));
        }
        items.extend(
            self.app_dirs()
                .into_iter()
                .map(|dir| Item::always(Leftover::Path(dir))),
        );
        items.push(Item::with_data(Leftover::Path(data)));
        items.extend(
            self.manual()
                .into_iter()
                .map(|what| Item::always(Leftover::Manual(what))),
        );
        items
    }

    /// What `leftover` is and where it lives, in words a user can check by hand.
    pub fn describe(&self, leftover: &Leftover) -> [String; 2] {
        let [what, place] = match leftover {
            Leftover::LoginItem => match self.platform {
                Platform::MacOs => [
                    "Login item".into(),
                    format!("launch agent {LAUNCHD_LABEL}, listed in System Settings > General > Login Items"),
                ],
                Platform::Windows => [
                    "Logon task".into(),
                    "Task Scheduler task signal-agent, and any signal-agent-<profile> task from an older version".into(),
                ],
                Platform::Linux => {
                    let units = self.roots.config.join("systemd").join("user");
                    [
                        "systemd user unit".into(),
                        format!(
                            "{}, and any signal-agent-<profile>.service from an older version",
                            units.join("signal-agent.service").display()
                        ),
                    ]
                }
            },
            Leftover::RunningAgents => [
                "Running agents".into(),
                "every signal-agent process of this user".into(),
            ],
            Leftover::ClaudeEntry { client, profile } => {
                let name = profile.mcp_server_name();
                let place = match client {
                    ClaudeClient::Code => format!("user-scope MCP server {name} in {}", self.roots.home.join(".claude.json").display()),
                    ClaudeClient::Desktop => format!(
                        "mcpServers.{name} in {}",
                        self.roots.config.join("Claude").join("claude_desktop_config.json").display()
                    ),
                };
                [format!("{client} entry"), place]
            }
            Leftover::Key(profile) => [
                "Database key".into(),
                match self.platform {
                    Platform::MacOs => format!("login Keychain generic password, service signal-agent, account {profile}"),
                    Platform::Windows => format!("DPAPI-protected key file in the {profile} profile folder"),
                    Platform::Linux => format!(
                        "Secret Service item (service signal-agent, profile {profile}), or the 0600 key file in the profile folder on a machine without one"
                    ),
                },
            ],
            Leftover::Path(path) => ["File or folder".into(), path.display().to_string()],
            Leftover::Manual(what) => ["Left for you".into(), (*what).into()],
        };
        [what, self.native_separators(place)]
    }

    /// `text` with path separators as this footprint's OS writes them, so a
    /// checklist printed on one OS reads the same as on another.
    fn native_separators(&self, text: String) -> String {
        match self.platform {
            Platform::Windows => text.replace('/', "\\"),
            Platform::MacOs | Platform::Linux => text.replace('\\', "/"),
        }
    }

    /// The setup app's own WebView, cache and window-state folders.
    fn app_dirs(&self) -> Vec<PathBuf> {
        let Roots {
            home,
            data_local,
            config,
        } = &self.roots;
        match self.platform {
            Platform::MacOs => {
                let library = home.join("Library");
                vec![
                    data_local.join(APP_ID),
                    library.join("Caches").join(APP_ID),
                    library.join("WebKit").join(APP_ID),
                    library.join("HTTPStorages").join(APP_ID),
                    library
                        .join("Saved Application State")
                        .join(format!("{APP_ID}.savedState")),
                ]
            }
            Platform::Windows => vec![data_local.join(APP_ID), config.join(APP_ID)],
            Platform::Linux => vec![],
        }
    }

    fn manual(&self) -> Vec<&'static str> {
        match self.platform {
            Platform::MacOs => {
                vec!["The app itself: drag MCP Relay for Signal from Applications to the Trash"]
            }
            Platform::Windows => vec![
                "The app itself: Settings > Apps > MCP Relay for Signal > Uninstall, which runs this same cleanup first",
            ],
            Platform::Linux => vec![
                "The signal-agent program itself, e.g. ~/.local/bin/signal-agent",
                "User lingering, which setup turned on so the agent runs after logout: `loginctl disable-linger` if nothing else needs it",
            ],
        }
    }
}

/// The acceptance checklist of everything uninstall removes on each OS,
/// printed from the same list uninstall works through.
pub fn checklist() -> String {
    let profile = ProfileName::default_profile();
    let oses = [
        (
            "macOS",
            Platform::MacOs,
            "~",
            "~/Library/Application Support",
            "~/Library/Application Support",
        ),
        (
            "Windows",
            Platform::Windows,
            "%USERPROFILE%",
            "%LOCALAPPDATA%",
            "%APPDATA%",
        ),
        ("Linux", Platform::Linux, "~", "~/.local/share", "~/.config"),
    ];
    let mut out = format!(
        "# Uninstall checklist\n\n\
         Generated from `crates/relay-setup/src/uninstall/footprint.rs`, the list uninstall itself works through; \
         a test fails if this file and the code disagree. Regenerate with \
         `UPDATE_UNINSTALL_CHECKLIST=1 cargo test -p relay-setup uninstall_checklist`.\n\n\
         For PRD acceptance 2, after Uninstall every row marked *always* must be gone. Rows marked *if ticked* \
         are gone only when \"{DELETE_DATA_LABEL}\" was ticked, and must otherwise still be there. \
         Rows marked *by you* are outside what an app may remove. Rows are shown for the default profile \
         `{profile}`; every other profile adds the same per-profile rows under its own name.\n"
    );
    for (name, platform, home, data_local, config) in oses {
        let footprint = Footprint::new(
            platform,
            Roots {
                home: home.into(),
                data_local: data_local.into(),
                config: config.into(),
            },
        );
        out.push_str(&format!(
            "\n## {name}\n\n| # | What | Where | Removed |\n|---|---|---|---|\n"
        ));
        for (n, item) in footprint
            .items(std::slice::from_ref(&profile))
            .iter()
            .enumerate()
        {
            let [what, place] = footprint.describe(&item.leftover);
            let when = match (&item.leftover, item.kept_with_data) {
                (Leftover::Manual(_), _) => "by you",
                (_, true) => "if ticked",
                (_, false) => "always",
            };
            let place = match item.leftover {
                Leftover::Manual(_) => place,
                _ => format!("`{place}`"),
            };
            out.push_str(&format!("| {} | {what} | {place} | {when} |\n", n + 1));
        }
    }
    out
}
