//! `signal-agent doctor`: the one command to run when Claude can't see
//! Signal. It says in plain words which piece is missing, and with `--fix`
//! puts back what can be put back (Claude entries, the login item) without
//! touching messages, keys or the Signal link. It never contacts Signal.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_secrets::{SecretStore, StoreKey};
use relay_setup::{
    Profiles, StoreLock,
    claude::{ClaudeClient, ClientStatus, Clients, ServerEntry},
};
use relay_signal::PresageSignal;
use relay_store::{MessageStore, SqlCipherStore};

/// Opens a profile's stores just far enough to prove the key unlocks them.
#[async_trait]
pub(crate) trait OpenStores: Send + Sync {
    async fn signal(&self, path: &Path, key: &StoreKey) -> anyhow::Result<()>;
    async fn messages(&self, path: &Path, key: &StoreKey) -> anyhow::Result<()>;
}

/// The login item that starts every profile's agent.
pub(crate) trait LoginCheck {
    fn enabled(&self) -> anyhow::Result<bool>;
    fn enable(&self, profile: &ProfileName) -> anyhow::Result<()>;
}

/// One thing doctor looked at, in words for the person reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Finding {
    Ok(String),
    Problem(String),
    Fixed(String),
}

/// The findings for one profile, or for the computer as a whole.
#[derive(Debug)]
pub(crate) struct Section {
    pub(crate) title: String,
    pub(crate) findings: Vec<Finding>,
}

/// Everything doctor looks at.
pub(crate) struct Doctor<'a> {
    pub(crate) profiles: &'a Profiles,
    pub(crate) secrets: &'a dyn SecretStore,
    pub(crate) stores: &'a dyn OpenStores,
    pub(crate) login: &'a dyn LoginCheck,
    pub(crate) clients: &'a Clients,
    /// The `signal-agent` the Claude entries should launch.
    pub(crate) agent: &'a Path,
}

impl Doctor<'_> {
    /// Checks `only`, or every listed profile, then the login item, repairing
    /// what it safely can when `fix`.
    pub(crate) async fn run(
        &self,
        only: Option<&ProfileName>,
        fix: bool,
    ) -> anyhow::Result<Vec<Section>> {
        let listed = self.profiles.list()?;
        let profiles = match only {
            Some(profile) => vec![profile.clone()],
            None if listed.is_empty() => vec![ProfileName::default_profile()],
            None => listed.clone(),
        };
        let mut sections = Vec::new();
        let mut any_set_up = None;
        for profile in &profiles {
            let findings = self.profile(profile, listed.contains(profile), fix).await?;
            if self.profiles.paths(profile).load_access().is_ok() {
                any_set_up.get_or_insert(profile.clone());
            }
            sections.push(Section {
                title: format!("Profile \"{profile}\""),
                findings,
            });
        }
        if let Some(profile) = any_set_up {
            sections.push(Section {
                title: "This computer".into(),
                findings: vec![self.login_item(&profile, fix)],
            });
        }
        Ok(sections)
    }

    async fn profile(
        &self,
        profile: &ProfileName,
        listed: bool,
        fix: bool,
    ) -> anyhow::Result<Vec<Finding>> {
        let paths = self.profiles.paths(profile);
        let mut findings = Vec::new();
        if !listed {
            findings.push(Finding::Problem(
                "It isn't in the list of set-up accounts. Run setup for it.".into(),
            ));
        }
        let access = match paths.load_access() {
            Ok(access) => access,
            Err(_) => {
                findings.push(Finding::Problem(
                    "Setup didn't finish for it. Run setup again.".into(),
                ));
                return Ok(findings);
            }
        };
        findings.push(Finding::Ok(match access {
            AccessMode::Full => "Set up; Claude may read and send messages.".into(),
            AccessMode::ReadOnly => "Set up; Claude may only read messages.".into(),
        }));
        let Some(key) = self.key(profile, &mut findings) else {
            return Ok(findings);
        };
        self.stores_open(&paths, &key, &mut findings).await;
        findings.extend(self.claude_entries(profile, fix)?);
        Ok(findings)
    }

    fn key(&self, profile: &ProfileName, findings: &mut Vec<Finding>) -> Option<StoreKey> {
        match self.secrets.get(profile) {
            Ok(Some(key)) => {
                findings.push(Finding::Ok("Its database key can be read.".into()));
                Some(key)
            }
            Ok(None) => {
                findings.push(Finding::Problem(
                    "Its database key is missing, so its messages can't be opened. \
                     Run setup again to start it over."
                        .into(),
                ));
                None
            }
            Err(e) => {
                findings.push(Finding::Problem(format!(
                    "Its database key can't be read right now ({e:#}). If your login \
                     keyring is locked, unlock it; the agent starts by itself once it can."
                )));
                None
            }
        }
    }

    async fn stores_open(&self, paths: &ProfilePaths, key: &StoreKey, findings: &mut Vec<Finding>) {
        if relay_setup::setup_running(paths) {
            findings.push(Finding::Ok(
                "Setup is running for it, so its databases weren't opened.".into(),
            ));
            return;
        }
        // NOTE: never open a store the agent has open; two processes driving one
        // Signal store corrupt it. The lock also keeps an agent from starting
        // mid-check.
        let _held = match StoreLock::try_acquire(paths) {
            Ok(Some(held)) => held,
            Ok(None) => {
                findings.push(Finding::Ok(
                    "Its agent is running and has its databases open.".into(),
                ));
                return;
            }
            Err(e) => {
                findings.push(Finding::Problem(format!(
                    "Its folder can't be checked ({e:#})."
                )));
                return;
            }
        };
        if !paths.signal_store.exists() {
            findings.push(Finding::Problem(
                "It has no Signal account on this computer. Run setup again to link it.".into(),
            ));
            return;
        }
        findings.push(match self.stores.signal(&paths.signal_store, key).await {
            Ok(()) => Finding::Ok("Its Signal account opens with its key.".into()),
            Err(e) => Finding::Problem(format!(
                "Its Signal account doesn't open with its key ({e:#})."
            )),
        });
        if !paths.message_store.exists() {
            findings.push(Finding::Ok(
                "No messages saved yet; the agent starts saving them once it runs.".into(),
            ));
            return;
        }
        findings.push(
            match self.stores.messages(&paths.message_store, key).await {
                Ok(()) => Finding::Ok("Its saved messages open with its key.".into()),
                Err(e) => Finding::Problem(format!(
                    "Its saved messages don't open with its key ({e:#})."
                )),
            },
        );
    }

    fn claude_entries(&self, profile: &ProfileName, fix: bool) -> anyhow::Result<Vec<Finding>> {
        let entry = ServerEntry::for_profile(profile, self.agent, ["mcp", "--profile", profile])?;
        let mut findings = Vec::new();
        for report in self.clients.doctor(&entry) {
            let client = report.client;
            findings.push(match report.status {
                ClientStatus::NotInstalled => continue,
                ClientStatus::Registered => Finding::Ok(format!("{client} can reach it.")),
                ClientStatus::Missing if fix => match self.clients.register(client, &entry) {
                    Ok(_) => {
                        Finding::Fixed(format!("Added it back to {client}.{}", restart(client)))
                    }
                    Err(e) => Finding::Problem(format!(
                        "{client} doesn't have it, and adding it failed ({e:#})."
                    )),
                },
                ClientStatus::Missing => Finding::Problem(format!(
                    "{client} doesn't have it. Run `signal-agent doctor --fix` to add it back."
                )),
                ClientStatus::Foreign => Finding::Problem(format!(
                    "{client} has a different server called \"{}\"; it was left alone.",
                    entry.name
                )),
                ClientStatus::Unreadable { error } => {
                    Finding::Problem(format!("{client}'s settings can't be read ({error})."))
                }
            });
        }
        Ok(findings)
    }

    fn login_item(&self, profile: &ProfileName, fix: bool) -> Finding {
        match self.login.enabled() {
            Ok(true) => Finding::Ok("The agent starts when you log in.".into()),
            Ok(false) if fix => match self.login.enable(profile) {
                Ok(()) => Finding::Fixed("Turned the agent's login item back on.".into()),
                Err(e) => Finding::Problem(format!(
                    "The agent doesn't start when you log in, and turning it on failed ({e:#})."
                )),
            },
            Ok(false) => Finding::Problem(
                "The agent doesn't start when you log in. Run `signal-agent doctor --fix`.".into(),
            ),
            Err(e) => Finding::Problem(format!(
                "Whether the agent starts at login can't be told ({e:#})."
            )),
        }
    }
}

/// Checks this computer for `only`, or every profile, printing the report and
/// failing if problems are left.
pub(crate) async fn run(only: Option<&ProfileName>, fix: bool) -> anyhow::Result<()> {
    let agent = std::env::current_exe().context("unable to locate this executable")?;
    let profiles = Profiles::system()?;
    let secrets = relay_secrets::platform_store()?;
    let clients = Clients::system();
    let sections = Doctor {
        profiles: &profiles,
        secrets: secrets.as_ref(),
        stores: &RealStores,
        login: &RealLogin,
        clients: &clients,
        agent: &agent,
    }
    .run(only, fix)
    .await?;
    let left = print(&sections, fix, &mut io::stdout())?;
    if left > 0 {
        bail!("doctor found {left} problem(s)");
    }
    Ok(())
}

/// The stores as the agent opens them.
// NOTE: opening the Signal store loads the account only; nothing reaches
// Signal until something is sent or received (PresageSignal::open).
struct RealStores;

#[async_trait]
impl OpenStores for RealStores {
    async fn signal(&self, path: &Path, key: &StoreKey) -> anyhow::Result<()> {
        drop(PresageSignal::open(path, key.as_bytes()).await?);
        Ok(())
    }

    async fn messages(&self, path: &Path, key: &StoreKey) -> anyhow::Result<()> {
        SqlCipherStore::open(path, key).await?.close().await
    }
}

struct RealLogin;

impl LoginCheck for RealLogin {
    fn enabled(&self) -> anyhow::Result<bool> {
        relay_agent::login::is_registered()
    }

    fn enable(&self, profile: &ProfileName) -> anyhow::Result<()> {
        relay_agent::login::register(profile)
    }
}

/// Writes `sections` for a person to read, returning how many problems are
/// left. `fixed` says whether this run already tried `--fix`.
pub(crate) fn print(sections: &[Section], fixed: bool, out: &mut impl Write) -> io::Result<usize> {
    let mut problems = 0;
    for section in sections {
        writeln!(out, "{}", section.title)?;
        for finding in &section.findings {
            let (mark, text) = match finding {
                Finding::Ok(text) => ("ok", text),
                Finding::Fixed(text) => ("fixed", text),
                Finding::Problem(text) => {
                    problems += 1;
                    ("PROBLEM", text)
                }
            };
            writeln!(out, "  {mark:<8} {text}")?;
        }
        writeln!(out)?;
    }
    let plural = if problems == 1 { "" } else { "s" };
    match problems {
        0 => writeln!(out, "Everything looks fine.")?,
        _ if fixed => writeln!(
            out,
            "{problems} problem{plural} left that doctor can't repair by itself; each says what to do."
        )?,
        _ => writeln!(
            out,
            "{problems} problem{plural} found. Run `signal-agent doctor --fix` to repair what can be repaired."
        )?,
    }
    Ok(problems)
}

fn restart(client: ClaudeClient) -> &'static str {
    match client {
        ClaudeClient::Desktop => " Restart Claude Desktop to pick it up.",
        ClaudeClient::Code => "",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use relay_secrets::{MemorySecretStore, SecretStore};
    use relay_setup::claude::{Platform, RunOutput, Runner};

    use super::*;

    /// Opens nothing; reports each check and fails the ones it is told to.
    #[derive(Default)]
    struct Stores {
        opened: Mutex<Vec<String>>,
        broken: Vec<&'static str>,
    }

    #[async_trait]
    impl OpenStores for Stores {
        async fn signal(&self, _: &Path, _: &StoreKey) -> anyhow::Result<()> {
            self.open("signal")
        }
        async fn messages(&self, _: &Path, _: &StoreKey) -> anyhow::Result<()> {
            self.open("messages")
        }
    }

    impl Stores {
        fn open(&self, which: &'static str) -> anyhow::Result<()> {
            self.opened.lock().expect("opened").push(which.into());
            if self.broken.contains(&which) {
                anyhow::bail!("file is not a database");
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct Login {
        enabled: Mutex<bool>,
    }

    impl LoginCheck for Login {
        fn enabled(&self) -> anyhow::Result<bool> {
            Ok(*self.enabled.lock().expect("login"))
        }
        fn enable(&self, _: &ProfileName) -> anyhow::Result<()> {
            *self.enabled.lock().expect("login") = true;
            Ok(())
        }
    }

    struct NoCli;

    impl Runner for NoCli {
        fn run(&self, _: &Path, _: &[String]) -> anyhow::Result<RunOutput> {
            anyhow::bail!("no Claude Code here")
        }
    }

    /// A machine with one profile, `personal`, set up and healthy: its key
    /// saved, both stores on disk, the login item on, and Claude Desktop
    /// holding its entry.
    struct Machine {
        dir: tempfile::TempDir,
        profiles: Profiles,
        secrets: MemorySecretStore,
        stores: Stores,
        login: Login,
        clients: Clients,
    }

    impl Machine {
        fn healthy() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let profiles = Profiles::under(dir.path().join("data"));
            let personal = profiles.ensure_default().expect("profile");
            let paths = profiles.paths(&personal);
            paths.save_access(AccessMode::Full).expect("access");
            std::fs::write(&paths.signal_store, b"").expect("signal.db");
            std::fs::write(&paths.message_store, b"").expect("messages.db");
            let secrets = MemorySecretStore::default();
            secrets.ensure(&personal).expect("key");
            let desktop = dir.path().join("Claude");
            std::fs::create_dir_all(&desktop).expect("desktop");
            let clients = Clients::new(Platform::Linux, Box::new(NoCli), None, Some(desktop));
            let machine = Self {
                profiles,
                secrets,
                stores: Stores::default(),
                login: Login {
                    enabled: Mutex::new(true),
                },
                clients,
                dir,
            };
            let entry = machine.entry(&personal);
            machine
                .clients
                .register(ClaudeClient::Desktop, &entry)
                .expect("desktop entry");
            machine
        }

        fn agent(&self) -> std::path::PathBuf {
            self.dir.path().join("signal-agent")
        }

        fn entry(&self, profile: &ProfileName) -> ServerEntry {
            ServerEntry::for_profile(profile, &self.agent(), ["mcp", "--profile", profile])
                .expect("entry")
        }

        async fn check(&self, fix: bool) -> Vec<Section> {
            let agent = self.agent();
            Doctor {
                profiles: &self.profiles,
                secrets: &self.secrets,
                stores: &self.stores,
                login: &self.login,
                clients: &self.clients,
                agent: &agent,
            }
            .run(None, fix)
            .await
            .expect("doctor")
        }
    }

    fn problems(sections: &[Section]) -> Vec<String> {
        sections
            .iter()
            .flat_map(|s| &s.findings)
            .filter_map(|f| match f {
                Finding::Problem(p) => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// A healthy machine must come back with no problems, after opening
    /// both stores under the saved key, so "all good" means something.
    #[tokio::test]
    async fn a_healthy_profile_reports_no_problems_after_opening_both_stores() {
        let machine = Machine::healthy();
        let sections = machine.check(false).await;
        assert_eq!(problems(&sections), Vec::<String>::new(), "{sections:?}");
        assert_eq!(
            *machine.stores.opened.lock().expect("opened"),
            ["signal", "messages"]
        );
    }

    /// Without --fix doctor only reports. With it, a missing Claude entry and
    /// a disabled login item come back, and a second run finds nothing wrong.
    #[tokio::test]
    async fn fix_puts_back_claude_entries_and_the_login_item_and_only_with_fix() {
        let machine = Machine::healthy();
        let personal = ProfileName::default_profile();
        machine
            .clients
            .unregister(ClaudeClient::Desktop, &machine.entry(&personal))
            .expect("remove entry");
        *machine.login.enabled.lock().expect("login") = false;

        let reported = machine.check(false).await;
        assert_eq!(problems(&reported).len(), 2, "{reported:?}");
        assert_eq!(
            problems(&machine.check(false).await).len(),
            2,
            "changed without --fix"
        );

        let fixed = machine.check(true).await;
        let fixes = fixed
            .iter()
            .flat_map(|s| &s.findings)
            .filter(|f| matches!(f, Finding::Fixed(_)))
            .count();
        assert_eq!(fixes, 2, "{fixed:?}");
        assert_eq!(problems(&machine.check(false).await), Vec::<String>::new());
    }

    /// Doctor must never open a store the running agent has open (two
    /// processes on one Signal store corrupt it); an agent holding them means
    /// they opened, which is no problem.
    #[tokio::test]
    async fn doctor_never_opens_stores_the_running_agent_holds() {
        let machine = Machine::healthy();
        let paths = machine.profiles.paths(&ProfileName::default_profile());
        let _agent = StoreLock::try_acquire(&paths).expect("lock").expect("free");
        let sections = machine.check(false).await;
        assert!(
            machine.stores.opened.lock().expect("opened").is_empty(),
            "opened stores under the running agent"
        );
        assert_eq!(problems(&sections), Vec::<String>::new(), "{sections:?}");
    }

    /// A key that's gone or can't be read, or a store its key doesn't open,
    /// must each be a problem that says what happened, and a missing key
    /// must not lead to opening anything.
    #[tokio::test]
    async fn a_missing_key_or_a_store_that_wont_open_is_a_plain_problem() {
        let machine = Machine::healthy();
        machine
            .secrets
            .delete(&ProfileName::default_profile())
            .expect("delete key");
        let sections = machine.check(false).await;
        let found = problems(&sections);
        assert!(
            found.len() == 1 && found[0].contains("key is missing"),
            "{found:?}"
        );
        assert!(machine.stores.opened.lock().expect("opened").is_empty());

        let mut machine = Machine::healthy();
        machine.stores.broken = vec!["messages"];
        let found = problems(&machine.check(false).await);
        assert!(
            found.len() == 1 && found[0].contains("saved messages don't open"),
            "{found:?}"
        );
    }

    /// The report a person reads: every finding under its heading, marked so
    /// problems stand out, and a last line that says whether anything's left
    /// to do and how.
    #[test]
    fn the_report_marks_each_finding_and_ends_with_what_to_do() {
        let sections = [Section {
            title: "Profile \"personal\"".into(),
            findings: vec![
                Finding::Ok("Set up.".into()),
                Finding::Fixed("Added it back to Claude Desktop.".into()),
                Finding::Problem("Its database key is missing.".into()),
            ],
        }];
        let mut out = Vec::new();
        let left = print(&sections, false, &mut out).expect("print");
        let text = String::from_utf8(out).expect("utf-8");
        assert_eq!(left, 1);
        for line in [
            "Profile \"personal\"",
            "  ok       Set up.",
            "  fixed    Added it back to Claude Desktop.",
            "  PROBLEM  Its database key is missing.",
            "1 problem found. Run `signal-agent doctor --fix` to repair what can be repaired.",
        ] {
            assert!(text.contains(line), "missing {line:?} in:\n{text}");
        }
        let mut out = Vec::new();
        print(&sections[..0], false, &mut out).expect("print");
        assert!(
            String::from_utf8(out)
                .expect("utf-8")
                .contains("Everything looks fine.")
        );
    }
}
