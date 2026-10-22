//! Runs setup for real: applies the state machine's transitions, performs the
//! commands they return against injected services, and feeds the results back
//! in, so each front end only renders states and reports what the user did.

use std::{collections::VecDeque, fs, path::Path, sync::Arc};

use anyhow::{Context, bail};
use async_trait::async_trait;
use futures::StreamExt;
use relay_core::prelude::*;
use relay_secrets::{SecretStore, StoreKey};
use relay_signal::{LinkEvent, Provisioner, RateLimited, RegistrationStep};
use relay_tools::{Clock, Limits, Settings, SystemClock};
use tokio::sync::watch;

use crate::{
    Command, HistoryGap, SetupEvent, SetupState,
    claude::{Clients, ServerEntry},
    transition,
};

/// Opens the Signal store a new account is created in.
#[async_trait]
pub trait OpenProvisioner: Send + Sync {
    /// The provisioner for the store at `path`, creating it encrypted with `key`.
    async fn open(&self, path: &Path, key: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>>;
}

/// Brings a newly linked device's earlier messages into its store.
#[async_trait]
pub trait HistoryImport: Send + Sync {
    /// Imports everything available, reporting messages imported so far and
    /// the total when known. Returns why there was nothing to import, if so.
    async fn import(
        &self,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> anyhow::Result<Option<HistoryGap>>;
}

/// Signal Desktop's own history on this computer, read only if the user opts in.
#[async_trait]
pub trait DesktopHistory: Send + Sync {
    /// Whether Signal Desktop's data is on this computer.
    fn found(&self) -> bool;

    /// Imports Desktop's messages, reporting how many so far.
    async fn import(&self, progress: &mut (dyn FnMut(u64) + Send)) -> anyhow::Result<()>;
}

/// Starts a profile's agent now and at every login.
pub trait LoginItem: Send + Sync {
    fn register(&self, profile: &ProfileName) -> anyhow::Result<()>;
}

/// Everything setup acts on besides the state machine.
pub struct Services {
    pub signal: Box<dyn OpenProvisioner>,
    pub secrets: Box<dyn SecretStore>,
    pub history: Box<dyn HistoryImport>,
    pub desktop: Box<dyn DesktopHistory>,
    pub login: Box<dyn LoginItem>,
    pub clients: Clients,
}

/// Stops a history import from outside the driver. The import holds the
/// driver until it ends, which can be an hour, so a Skip click queued behind it
/// would never be seen.
#[derive(Clone)]
pub struct HistorySkip(Arc<watch::Sender<bool>>);

impl HistorySkip {
    /// Abandons the history import running now, or the next one to start.
    pub fn skip(&self) {
        self.0.send_replace(true);
    }
}

/// What a front end passes to watch each state as setup enters it.
pub type Show<'a> = &'a mut (dyn FnMut(&SetupState) + Send);

/// Drives one profile's setup from its first state to done.
pub struct Driver {
    state: SetupState,
    profile: ProfileName,
    paths: ProfilePaths,
    server: ServerEntry,
    services: Services,
    provisioner: Option<Box<dyn Provisioner>>,
    clock: Arc<dyn Clock>,
    skip: HistorySkip,
    /// Whether this registration attempt already asked for a call.
    called: bool,
}

impl Driver {
    /// Setup for `profile`, stored under `paths`, registering `server` with
    /// Claude clients once it is done.
    pub fn new(
        start: SetupState,
        profile: ProfileName,
        paths: ProfilePaths,
        server: ServerEntry,
        services: Services,
    ) -> Self {
        Self {
            state: start,
            profile,
            paths,
            server,
            services,
            provisioner: None,
            called: false,
            clock: Arc::new(SystemClock),
            skip: HistorySkip(Arc::new(watch::Sender::new(false))),
        }
    }

    /// Replaces the wall clock the provisioning backoff is judged by.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn state(&self) -> &SetupState {
        &self.state
    }

    /// What a front end calls when the user skips history.
    pub fn history_skip(&self) -> HistorySkip {
        self.skip.clone()
    }

    /// The secret store's warning, for setup to show before anything is stored.
    pub fn key_caveat(&self) -> Option<&'static str> {
        self.services.secrets.caveat()
    }

    /// Applies `event`, then runs every command it leads to, showing each state
    /// as it is entered. Returns once setup waits on the user again.
    pub async fn dispatch(&mut self, event: SetupEvent, show: Show<'_>) {
        let mut pending: VecDeque<Command> = apply(&mut self.state, event, show).into();
        while let Some(command) = pending.pop_front() {
            let Err(e) = self.run(command, show, &mut pending).await else {
                continue;
            };
            let mut message = format!("{e:#}");
            if let Some(Err(d)) = e.downcast_ref().map(|l| self.defer_provisioning(l)) {
                message.push_str(&format!(" ({d:#}, so wait that long yourself)"));
            }
            // NOTE: a failed command drops the rest of its batch. SaveAccess and
            // ConnectClients share one; connecting after a failed save would
            // hand clients a profile in the wrong access mode.
            pending.clear();
            let failed = SetupEvent::CommandFailed { message };
            pending.extend(apply(&mut self.state, failed, show));
        }
    }

    /// Performs `command`, applying each event it produces as it arrives.
    async fn run(
        &mut self,
        command: Command,
        show: Show<'_>,
        pending: &mut VecDeque<Command>,
    ) -> anyhow::Result<()> {
        let event = match command {
            Command::StartLink { device_name } => {
                self.begin_provisioning()?;
                let mut events = self.provisioner().await?.link(device_name);
                loop {
                    let Some(event) = events.next().await else {
                        bail!("Signal stopped offering link codes before the phone scanned one");
                    };
                    match event? {
                        LinkEvent::Uri { uri } => pending.extend(apply(
                            &mut self.state,
                            SetupEvent::LinkUri { uri },
                            show,
                        )),
                        LinkEvent::Linked { .. } => break,
                    }
                }
                self.close_signal_store();
                SetupEvent::Linked
            }
            Command::RequestCode {
                number,
                captcha,
                voice,
            } => {
                // NOTE: a captcha only comes after Signal asked for one, and a
                // call only after an SMS, so both finish the attempt in
                // progress; gating them would leave the user stuck behind the
                // backoff. One call per attempt is our own limit, so a stuck
                // button can't make Signal ring a phone over and over (PRD
                // goal 13); past that, a fresh attempt meets the backoff.
                if voice {
                    if self.called {
                        bail!(
                            "A call for this code was already requested. Wait for it, \
                             or go back and enter your number again to start over."
                        );
                    }
                    self.called = true;
                } else if captcha.is_none() {
                    self.begin_provisioning()?;
                    self.called = false;
                }
                let step = self
                    .provisioner()
                    .await?
                    .request_code(&number, captcha.as_deref(), voice)
                    .await?;
                match step {
                    RegistrationStep::CaptchaRequired => SetupEvent::CaptchaRequired,
                    RegistrationStep::CodeSent => SetupEvent::CodeSent,
                    RegistrationStep::Registered { .. } => {
                        bail!("Signal registered {number} without sending a code")
                    }
                }
            }
            Command::VerifyCode { code } => {
                let step = self.provisioner().await?.verify_code(&code).await?;
                let RegistrationStep::Registered { .. } = step else {
                    bail!("Signal did not accept the code");
                };
                self.close_signal_store();
                SetupEvent::Registered
            }
            Command::SaveAccess { access } => {
                self.paths.save_access(access)?;
                return Ok(());
            }
            Command::ImportHistory => {
                let state = &mut self.state;
                let mut progress = |imported, total| {
                    let event = SetupEvent::HistoryProgress { imported, total };
                    pending.extend(apply(state, event, show));
                };
                let mut skipped = self.skip.0.subscribe();
                let outcome = tokio::select! {
                    outcome = self.services.history.import(&mut progress) => Some(outcome),
                    _ = skipped.wait_for(|skip| *skip) => None,
                };
                // Cleared only once the import ends, so a click that lands just
                // before it starts still counts, and a late one can't cancel a retry.
                self.skip.0.send_replace(false);
                match outcome {
                    None => SetupEvent::SkipHistory,
                    Some(outcome) => match outcome? {
                        None => SetupEvent::HistoryDone,
                        Some(reason) => SetupEvent::NoHistory { reason },
                    },
                }
            }
            Command::FindDesktop if self.services.desktop.found() => SetupEvent::DesktopFound,
            Command::FindDesktop => SetupEvent::DesktopAbsent,
            Command::ImportDesktop => {
                let state = &mut self.state;
                let mut progress = |imported| {
                    pending.extend(apply(state, SetupEvent::DesktopProgress { imported }, show));
                };
                self.services.desktop.import(&mut progress).await?;
                SetupEvent::DesktopImportDone
            }
            Command::ConnectClients => {
                self.services.login.register(&self.profile)?;
                SetupEvent::ClientsConnected {
                    clients: self.services.clients.reregister_missing(&self.server),
                }
            }
            Command::RestartDesktop => {
                self.services.clients.restart_desktop()?;
                return Ok(());
            }
        };
        pending.extend(apply(&mut self.state, event, show));
        Ok(())
    }

    /// The provisioner for this profile's store, opened on first use and kept,
    /// because a verification code only verifies on the session that asked.
    async fn provisioner(&mut self) -> anyhow::Result<&dyn Provisioner> {
        let provisioner = match self.provisioner.take() {
            Some(provisioner) => provisioner,
            None => {
                let key = self.store_key()?;
                fs::create_dir_all(&self.paths.root).with_context(|| {
                    format!("unable to create \"{}\"", self.paths.root.display())
                })?;
                self.services
                    .signal
                    .open(&self.paths.signal_store, &key)
                    .await?
            }
        };
        Ok(&**self.provisioner.insert(provisioner))
    }

    /// Refuses a link or registration too soon after the last one, before
    /// Signal hears of it.
    fn begin_provisioning(&self) -> anyhow::Result<()> {
        let root = &self.paths.root;
        fs::create_dir_all(root)
            .with_context(|| format!("unable to create \"{}\"", root.display()))?;
        // A damaged settings file must not block setup, which may be how the
        // user gets back to Advanced to fix it; the defaults are conservative.
        let limits = Settings::load(&self.paths.settings)
            .map(|s| s.limits)
            .unwrap_or_else(|e| {
                tracing::warn!("using the default provisioning backoff: {e:#}");
                Limits::default()
            });
        relay_tools::begin_provisioning(&self.paths.provisioning, &limits, self.clock.now_ms())
    }

    /// Holds the next link or registration back until Signal's wait is over.
    fn defer_provisioning(&self, limited: &RateLimited) -> anyhow::Result<()> {
        relay_tools::defer_provisioning(&self.paths.provisioning, limited, self.clock.now_ms())
    }

    // NOTE: the agent opens this same store once it starts, and two processes
    // driving one Signal store corrupt its sessions, so the provisioner must be
    // gone before ConnectClients starts the agent.
    fn close_signal_store(&mut self) {
        self.provisioner = None;
    }

    /// The profile's saved key, or a new one saved before anything uses it:
    /// a store encrypted with a key that was never saved is lost.
    fn store_key(&self) -> anyhow::Result<StoreKey> {
        let secrets = &self.services.secrets;
        if let Some(key) = secrets.get(&self.profile)? {
            return Ok(key);
        }
        let key = StoreKey::generate()?;
        secrets.set(&self.profile, &key)?;
        Ok(key)
    }
}

/// Moves `state` on by `event`, showing it if it changed.
fn apply(state: &mut SetupState, event: SetupEvent, show: Show<'_>) -> Vec<Command> {
    let (next, commands) = transition(state, event);
    if next != *state {
        *state = next;
        show(state);
    }
    commands
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use futures::stream::BoxStream;
    use proptest::prelude::*;
    use relay_secrets::MemorySecretStore;
    use relay_signal::Account;

    use super::*;
    use crate::{
        AccountChoice, ClientOutcome,
        claude::{ClaudeClient, Platform, RunOutput, Runner},
    };

    fn account(kind: ProfileKind) -> Account {
        Account {
            service_id: ServiceId::new(uuid::Uuid::from_u128(7)),
            phone_number: None,
            kind,
            device_name: "Claude (box)".into(),
        }
    }

    type Imports = VecDeque<anyhow::Result<(Vec<(u64, Option<u64>)>, Option<HistoryGap>)>>;

    /// The fakes' shared state: what Signal and the OS will do, and everything
    /// every fake did, in order. Clones share it, so two setup runs can see one
    /// machine.
    #[derive(Clone)]
    struct World {
        dir: Arc<tempfile::TempDir>,
        log: Arc<Mutex<Vec<String>>>,
        links: Arc<Mutex<VecDeque<Vec<anyhow::Result<LinkEvent>>>>>,
        codes: Arc<Mutex<VecDeque<anyhow::Result<RegistrationStep>>>>,
        imports: Arc<Mutex<Imports>>,
        stalled: Arc<std::sync::atomic::AtomicBool>,
        desktop: Arc<Mutex<Option<Vec<u64>>>>,
        failures: Arc<Mutex<Vec<&'static str>>>,
        opened_with: Arc<Mutex<Vec<StoreKey>>>,
        secrets: Arc<MemorySecretStore>,
        clock: Arc<FakeClock>,
    }

    /// Setup's clock. Unless stopped, each reading is an hour after the last,
    /// so tests that aren't about the provisioning backoff never land in it.
    struct FakeClock {
        now: AtomicU64,
        step: u64,
    }

    impl relay_tools::Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.fetch_add(self.step, Ordering::SeqCst)
        }
    }

    impl FakeClock {
        fn advance(&self, ms: u64) {
            self.now.fetch_add(ms, Ordering::SeqCst);
        }
    }

    impl World {
        fn new() -> Self {
            Self {
                dir: Arc::new(tempfile::tempdir().expect("tempdir")),
                log: Arc::default(),
                links: Arc::default(),
                codes: Arc::default(),
                imports: Arc::default(),
                stalled: Arc::default(),
                desktop: Arc::default(),
                failures: Arc::default(),
                opened_with: Arc::default(),
                secrets: Arc::default(),
                clock: Arc::new(FakeClock {
                    now: AtomicU64::new(1_700_000_000_000),
                    step: 60 * 60 * 1_000,
                }),
            }
        }

        /// Time only moves when the test advances it.
        fn stopped_clock(mut self) -> Self {
            self.clock = Arc::new(FakeClock {
                now: AtomicU64::new(1_700_000_000_000),
                step: 0,
            });
            self
        }

        /// The next link session yields `events`.
        fn link(self, events: Vec<anyhow::Result<LinkEvent>>) -> Self {
            self.links.lock().expect("links").push_back(events);
            self
        }

        /// The next request_code or verify_code call answers `step`.
        fn code(self, step: anyhow::Result<RegistrationStep>) -> Self {
            self.codes.lock().expect("codes").push_back(step);
            self
        }

        /// The next history import reports `progress`, or fails.
        fn import(self, progress: anyhow::Result<Vec<(u64, Option<u64>)>>) -> Self {
            let import = progress.map(|steps| (steps, None));
            self.imports.lock().expect("imports").push_back(import);
            self
        }

        /// History imports wait on the phone forever, as a long-poll the phone
        /// never answers does.
        fn stalled_history(self) -> Self {
            self.stalled.store(true, Ordering::SeqCst);
            self
        }

        /// The next history import finds nothing to import, for `reason`.
        fn no_history(self, reason: HistoryGap) -> Self {
            let import = Ok((Vec::new(), Some(reason)));
            self.imports.lock().expect("imports").push_back(import);
            self
        }

        /// Signal Desktop is on this computer, and importing it reports `progress`.
        fn with_desktop(self, progress: Vec<u64>) -> Self {
            *self.desktop.lock().expect("desktop") = Some(progress);
            self
        }

        /// The next `action` ("start agent", "restart desktop") fails once.
        fn fail_once(self, action: &'static str) -> Self {
            self.failures.lock().expect("failures").push(action);
            self
        }

        fn fails(&self, action: &str) -> bool {
            let mut failures = self.failures.lock().expect("failures");
            let Some(i) = failures.iter().position(|f| *f == action) else {
                return false;
            };
            failures.remove(i);
            true
        }

        fn paths(&self) -> ProfilePaths {
            ProfilePaths::under(
                self.dir.path().to_path_buf(),
                &ProfileName::default_profile(),
            )
        }

        /// The access mode on disk right now, as the agent would read it.
        fn saved_access(&self) -> String {
            self.paths()
                .load_access()
                .map_or_else(|_| "unsaved".into(), |a| format!("{a:?}"))
        }

        fn push(&self, entry: impl Into<String>) {
            self.log.lock().expect("log").push(entry.into());
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().expect("log").clone()
        }

        fn opened_with(&self) -> Vec<StoreKey> {
            self.opened_with.lock().expect("keys").clone()
        }

        fn next_code(&self) -> anyhow::Result<RegistrationStep> {
            self.codes
                .lock()
                .expect("codes")
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no scripted code step")))
        }
    }

    struct FakeProvisioner(World);

    impl Drop for FakeProvisioner {
        fn drop(&mut self) {
            self.0.push("close store");
        }
    }

    #[async_trait]
    impl Provisioner for FakeProvisioner {
        fn link(&self, device_name: String) -> BoxStream<'static, anyhow::Result<LinkEvent>> {
            self.0.push(format!("link as {device_name}"));
            let events = self
                .0
                .links
                .lock()
                .expect("links")
                .pop_front()
                .unwrap_or_default();
            futures::stream::iter(events).boxed()
        }

        async fn request_code(
            &self,
            number: &str,
            captcha: Option<&str>,
            voice: bool,
        ) -> anyhow::Result<RegistrationStep> {
            self.0
                .push(format!("request code {number} {captcha:?} voice={voice}"));
            self.0.next_code()
        }

        async fn verify_code(&self, code: &str) -> anyhow::Result<RegistrationStep> {
            self.0.push(format!("verify {code}"));
            self.0.next_code()
        }
    }

    #[async_trait]
    impl OpenProvisioner for World {
        async fn open(&self, path: &Path, key: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
            self.push(format!("open {}", path.display()));
            self.opened_with.lock().expect("keys").push(key.clone());
            Ok(Box::new(FakeProvisioner(self.clone())))
        }
    }

    impl SecretStore for World {
        fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
            self.secrets.get(profile)
        }
        fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
            self.push("save key");
            self.secrets.set(profile, key)
        }
        fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
            self.secrets.delete(profile)
        }
    }

    #[async_trait]
    impl HistoryImport for World {
        async fn import(
            &self,
            progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
        ) -> anyhow::Result<Option<HistoryGap>> {
            self.push("import history");
            if self.stalled.load(Ordering::SeqCst) {
                let _waiting = Waiting(self.clone());
                futures::future::pending::<()>().await;
            }
            let (steps, gap) = self
                .imports
                .lock()
                .expect("imports")
                .pop_front()
                .unwrap_or_else(|| Ok((vec![], None)))?;
            for (imported, total) in steps {
                progress(imported, total);
            }
            Ok(gap)
        }
    }

    /// Logs when a stalled import is abandoned.
    struct Waiting(World);

    impl Drop for Waiting {
        fn drop(&mut self) {
            self.0.push("stop waiting for history");
        }
    }

    #[async_trait]
    impl DesktopHistory for World {
        fn found(&self) -> bool {
            self.desktop.lock().expect("desktop").is_some()
        }

        async fn import(&self, progress: &mut (dyn FnMut(u64) + Send)) -> anyhow::Result<()> {
            self.push("import desktop");
            let steps = self.desktop.lock().expect("desktop").clone();
            for imported in steps.unwrap_or_default() {
                progress(imported);
            }
            Ok(())
        }
    }

    impl LoginItem for World {
        fn register(&self, profile: &ProfileName) -> anyhow::Result<()> {
            self.push(format!(
                "start agent {profile}, access {}",
                self.saved_access()
            ));
            if self.fails("start agent") {
                anyhow::bail!("systemctl failed");
            }
            Ok(())
        }
    }

    // Claude Code's CLI, where `mcp get` finds nothing until `mcp add` ran.
    impl Runner for World {
        fn run(&self, program: &Path, args: &[String]) -> anyhow::Result<RunOutput> {
            let line = format!("{} {}", program.display(), args.join(" "));
            let success = match args.first().map(String::as_str) {
                Some("mcp") if args.get(1).is_some_and(|a| a == "get") => false,
                Some("mcp") => {
                    self.push(format!("{line}, access {}", self.saved_access()));
                    true
                }
                _ => {
                    self.push("restart desktop");
                    !self.fails("restart desktop")
                }
            };
            Ok(RunOutput {
                success,
                ..RunOutput::default()
            })
        }
    }

    struct Harness {
        driver: Driver,
        shown: Vec<SetupState>,
    }

    const COMPUTER: &str = "box";

    impl Harness {
        fn new(world: &World) -> Self {
            let profile = ProfileName::default_profile();
            let server = ServerEntry::for_profile(
                &profile,
                &std::env::temp_dir().join("signal-agent"),
                ["mcp"],
            )
            .expect("entry");
            let services = Services {
                signal: Box::new(world.clone()),
                secrets: Box::new(world.clone()),
                history: Box::new(world.clone()),
                desktop: Box::new(world.clone()),
                login: Box::new(world.clone()),
                clients: Clients::new(
                    Platform::MacOs,
                    Box::new(world.clone()),
                    Some(PathBuf::from("claude")),
                    None,
                ),
            };
            Self {
                driver: Driver::new(
                    SetupState::start(COMPUTER),
                    profile,
                    world.paths(),
                    server,
                    services,
                )
                .with_clock(world.clock.clone()),
                shown: Vec::new(),
            }
        }

        async fn send(&mut self, event: SetupEvent) -> SetupState {
            let shown = &mut self.shown;
            self.driver
                .dispatch(event, &mut |s| shown.push(s.clone()))
                .await;
            self.driver.state().clone()
        }
    }

    fn uri(uri: &str) -> anyhow::Result<LinkEvent> {
        Ok(LinkEvent::Uri { uri: uri.into() })
    }

    fn linked() -> anyhow::Result<LinkEvent> {
        Ok(LinkEvent::Linked {
            account: account(ProfileKind::Linked),
        })
    }

    fn linking(uri: Option<&str>) -> SetupState {
        SetupState::Linking {
            device_name: format!("Claude ({COMPUTER})"),
            uri: uri.map(Into::into),
        }
    }

    const LINK: SetupEvent = SetupEvent::AccountChosen {
        choice: AccountChoice::LinkExisting,
    };

    /// Each refreshed link URI must reach the screen as it arrives, so the QR
    /// code a phone scans is never one Signal already expired.
    #[tokio::test]
    async fn every_link_uri_is_shown_before_the_scan_completes() {
        let world = World::new().link(vec![uri("sgnl://a"), uri("sgnl://b"), linked()]);
        let mut h = Harness::new(&world);
        let state = h.send(LINK).await;
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Linked
            }
        );
        assert_eq!(
            h.shown,
            vec![
                linking(None),
                linking(Some("sgnl://a")),
                linking(Some("sgnl://b")),
                state
            ]
        );
    }

    /// The Signal store is encrypted with the key as it is created, so the key
    /// must be saved first, and a setup run again after a crash must open the
    /// store with that same key or the half-made store is unreadable.
    #[tokio::test]
    async fn the_store_key_is_saved_before_the_store_opens_and_reused_on_rerun() {
        let world = World::new()
            .link(vec![Err(anyhow::anyhow!("network down"))])
            .link(vec![linked()]);
        let first = Harness::new(&world).send(LINK).await;
        assert!(
            matches!(first, SetupState::Failed { .. }),
            "a failed link left {first:?}"
        );
        let log = world.log();
        let saved = log.iter().position(|e| e == "save key");
        let opened = log.iter().position(|e| e.starts_with("open "));
        assert!(
            saved.is_some() && saved < opened,
            "store opened before its key was saved: {log:?}"
        );

        Harness::new(&world).send(LINK).await;
        let keys = world.opened_with();
        assert_eq!(keys.len(), 2, "{log:?}");
        assert_eq!(keys.first(), keys.last(), "a rerun opened with a new key");
        assert_eq!(
            log.iter().filter(|e| *e == "save key").count(),
            1,
            "{log:?}"
        );
    }

    /// The whole linked-device path: the access choice is on disk before the
    /// agent starts and before any Claude client can launch it, the link's
    /// store is closed before the agent opens it, and history progress shows.
    #[tokio::test]
    async fn linking_ends_with_access_saved_before_the_agent_or_any_client_starts() {
        let world = World::new()
            .link(vec![uri("sgnl://a"), linked()])
            .import(Ok(vec![(1, Some(2)), (2, Some(2))]));
        let mut h = Harness::new(&world);
        h.send(LINK).await;
        let done = h
            .send(SetupEvent::AccessChosen {
                access: AccessMode::ReadOnly,
            })
            .await;
        assert_eq!(
            done,
            SetupState::Done {
                clients: vec![ClientOutcome {
                    client: ClaudeClient::Code,
                    result: Ok(())
                }]
            }
        );
        let progress: Vec<_> = h
            .shown
            .iter()
            .filter_map(|s| match s {
                SetupState::ImportingHistory {
                    imported, total, ..
                } => Some((*imported, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(progress, [(0, None), (1, Some(2)), (2, Some(2))]);

        let log = world.log();
        let after_link: Vec<_> = log
            .iter()
            .skip_while(|e| !e.starts_with("link as"))
            .skip(1)
            .map(String::as_str)
            .collect();
        assert_eq!(
            after_link,
            [
                "close store",
                "import history",
                "start agent personal, access ReadOnly",
                "claude mcp add -s user signal-personal -- \
                 /tmp/signal-agent mcp, access ReadOnly",
            ]
            .map(|e| e.replace(
                "/tmp/signal-agent",
                &std::env::temp_dir()
                    .join("signal-agent")
                    .display()
                    .to_string()
            )),
            "{log:?}"
        );
    }

    /// Signal Desktop's history is never offered when Desktop is absent and
    /// never read without a yes; with a yes, its progress shows and access is
    /// still saved before the agent starts.
    #[tokio::test]
    async fn signal_desktop_is_offered_only_when_found_and_read_only_on_yes() {
        let access = SetupEvent::AccessChosen {
            access: AccessMode::Full,
        };
        let absent = World::new().link(vec![linked()]);
        let mut h = Harness::new(&absent);
        h.send(LINK).await;
        let done = h.send(access.clone()).await;
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
        assert!(
            !h.shown
                .iter()
                .any(|s| matches!(s, SetupState::OfferDesktopImport { .. })),
            "offered an absent Desktop: {:?}",
            h.shown
        );

        for yes in [false, true] {
            let world = World::new().link(vec![linked()]).with_desktop(vec![10, 20]);
            let mut h = Harness::new(&world);
            h.send(LINK).await;
            let offer = h.send(access.clone()).await;
            assert_eq!(
                offer,
                SetupState::OfferDesktopImport {
                    access: AccessMode::Full
                }
            );
            let done = h
                .send(SetupEvent::DesktopImportChosen { import: yes })
                .await;
            assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
            let log = world.log();
            let read = log.iter().position(|e| e == "import desktop");
            assert_eq!(read.is_some(), yes, "yes={yes}: {log:?}");
            let progress: Vec<_> = h
                .shown
                .iter()
                .filter_map(|s| match s {
                    SetupState::ImportingDesktop { imported, .. } => Some(*imported),
                    _ => None,
                })
                .collect();
            assert_eq!(progress, if yes { vec![0, 10, 20] } else { vec![] });
            if let Some(read) = read {
                let started = log.iter().position(|e| e.starts_with("start agent"));
                assert!(started > Some(read), "agent started first: {log:?}");
            }
        }
    }

    /// When the phone sends no history, setup says why and waits; carrying on
    /// still saves access before the agent starts.
    #[tokio::test]
    async fn missing_history_waits_on_the_user_then_finishes_setup() {
        let world = World::new()
            .link(vec![linked()])
            .no_history(HistoryGap::RelinkRequested);
        let mut h = Harness::new(&world);
        h.send(LINK).await;
        let waiting = h
            .send(SetupEvent::AccessChosen {
                access: AccessMode::ReadOnly,
            })
            .await;
        assert_eq!(
            waiting,
            SetupState::HistoryUnavailable {
                reason: HistoryGap::RelinkRequested,
                access: AccessMode::ReadOnly
            }
        );
        assert!(
            !world.log().iter().any(|e| e.starts_with("start agent")),
            "{:?}",
            world.log()
        );
        let done = h.send(SetupEvent::SkipHistory).await;
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
        assert!(
            world
                .log()
                .contains(&"start agent personal, access ReadOnly".to_owned()),
            "{:?}",
            world.log()
        );
    }

    /// The phone may take up to an hour to send history, or never send it.
    /// Skip must stop waiting at once and finish setup, from the moment the
    /// history screen shows.
    #[tokio::test]
    async fn skip_stops_waiting_for_history_and_finishes_setup() {
        let world = World::new().link(vec![linked()]).stalled_history();
        let mut h = Harness::new(&world);
        h.send(LINK).await;
        let skip = h.driver.history_skip();
        let access = SetupEvent::AccessChosen {
            access: AccessMode::Full,
        };
        let press = async {
            tokio::task::yield_now().await;
            skip.skip();
        };
        let (done, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(h.send(access), press)
        })
        .await
        .expect("skip did not stop the wait");
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
        let log = world.log();
        let stopped = log.iter().position(|e| e == "stop waiting for history");
        let started = log.iter().position(|e| e.starts_with("start agent"));
        assert!(
            stopped.is_some() && stopped < started,
            "the wait outlived the skip: {log:?}"
        );
    }

    const NUMBER: &str = "+15555550100";

    fn primary_registered() -> anyhow::Result<RegistrationStep> {
        Ok(RegistrationStep::Registered {
            account: account(ProfileKind::Primary),
        })
    }

    /// The own-number path: the captcha token goes back with the number, the
    /// code is verified on the same Signal session that sent it, the store is
    /// closed before the agent opens it, and there is no history to import.
    #[tokio::test]
    async fn an_own_number_registers_through_captcha_and_code_without_history() {
        let world = World::new()
            .code(Ok(RegistrationStep::CaptchaRequired))
            .code(Ok(RegistrationStep::CodeSent))
            .code(primary_registered());
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(SetupEvent::NumberEntered {
            number: NUMBER.into(),
        })
        .await;
        h.send(SetupEvent::CaptchaSolved {
            token: "tok".into(),
        })
        .await;
        h.send(SetupEvent::CodeEntered {
            code: "123456".into(),
        })
        .await;
        let done = h
            .send(SetupEvent::AccessChosen {
                access: AccessMode::Full,
            })
            .await;
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");

        let log = world.log();
        let signal: Vec<_> = log
            .iter()
            .filter(|e| !e.starts_with("open ") && *e != "save key")
            .map(String::as_str)
            .take(5)
            .collect();
        assert_eq!(
            signal,
            [
                "request code +15555550100 None voice=false",
                "request code +15555550100 Some(\"tok\") voice=false",
                "verify 123456",
                "close store",
                "start agent personal, access Full",
            ],
            "{log:?}"
        );
        assert_eq!(world.opened_with().len(), 1, "session reopened: {log:?}");
        assert!(
            !log.contains(&"import history".to_owned()),
            "a new number imported history: {log:?}"
        );
    }

    /// A call asked for after the SMS belongs to the same attempt: it reaches
    /// Signal with voice set, on the same session, inside the provisioning
    /// backoff that would refuse a fresh attempt.
    #[tokio::test]
    async fn a_voice_call_after_the_sms_is_not_held_back_by_the_backoff() {
        let world = World::new()
            .stopped_clock()
            .code(Ok(RegistrationStep::CodeSent))
            .code(Ok(RegistrationStep::CodeSent));
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(SetupEvent::NumberEntered {
            number: NUMBER.into(),
        })
        .await;
        let state = h.send(SetupEvent::CodeByVoice).await;
        assert!(matches!(state, SetupState::EnterCode { .. }), "{state:?}");
        let log = world.log();
        let requests: Vec<_> = log
            .iter()
            .filter(|e| e.starts_with("request code"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            requests,
            [
                "request code +15555550100 None voice=false",
                "request code +15555550100 None voice=true",
            ],
            "{log:?}"
        );
        assert_eq!(world.opened_with().len(), 1, "session reopened: {log:?}");
    }

    /// A second call request in one attempt is refused before Signal hears of
    /// it, and starting over from the number meets the provisioning backoff.
    #[tokio::test]
    async fn a_second_voice_call_request_in_one_attempt_is_refused_without_contacting_signal() {
        let world = World::new()
            .stopped_clock()
            .code(Ok(RegistrationStep::CodeSent))
            .code(Ok(RegistrationStep::CodeSent))
            .code(Ok(RegistrationStep::CodeSent));
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(SetupEvent::NumberEntered {
            number: NUMBER.into(),
        })
        .await;
        h.send(SetupEvent::CodeByVoice).await;
        let refused = h.send(SetupEvent::CodeByVoice).await;
        let SetupState::Failed { message, .. } = &refused else {
            panic!("second call request not refused: {refused:?}");
        };
        assert!(message.contains("already requested"), "{message}");
        h.send(SetupEvent::Retry).await;
        let again = h
            .send(SetupEvent::NumberEntered {
                number: NUMBER.into(),
            })
            .await;
        assert!(matches!(again, SetupState::Failed { .. }), "{again:?}");
        let log = world.log();
        let requests: Vec<_> = log
            .iter()
            .filter(|e| e.starts_with("request code"))
            .map(String::as_str)
            .collect();
        assert_eq!(
            requests,
            [
                "request code +15555550100 None voice=false",
                "request code +15555550100 None voice=true",
            ],
            "{log:?}"
        );
    }

    /// A failed access save must stop clients connecting in the same batch,
    /// and Retry must save again before connecting.
    #[tokio::test]
    async fn a_failed_access_save_never_connects_clients() {
        let world = World::new().code(Ok(RegistrationStep::CodeSent));
        let world = world.code(primary_registered());
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(SetupEvent::NumberEntered {
            number: NUMBER.into(),
        })
        .await;
        h.send(SetupEvent::CodeEntered { code: "1".into() }).await;
        // A directory where the access file goes makes the save fail.
        let blocker = world.paths().root.join("access.json");
        fs::create_dir_all(&blocker).expect("blocker");
        let failed = h
            .send(SetupEvent::AccessChosen {
                access: AccessMode::ReadOnly,
            })
            .await;
        assert!(matches!(failed, SetupState::Failed { .. }), "{failed:?}");
        let log = world.log();
        assert!(
            !log.iter()
                .any(|e| e.starts_with("start agent") || e.contains("mcp add")),
            "connected after a failed save: {log:?}"
        );

        fs::remove_dir(&blocker).expect("unblock");
        let done = h.send(SetupEvent::Retry).await;
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
        assert!(
            world
                .log()
                .contains(&"start agent personal, access ReadOnly".to_owned()),
            "{:?}",
            world.log()
        );
    }

    fn assert_failed(state: &SetupState, step: &str) {
        assert!(
            matches!(state, SetupState::Failed { .. }),
            "a failed {step} left {state:?}"
        );
    }

    /// Signal flags accounts that link or register over and over, so a second
    /// link inside the backoff must be refused before the provisioner hears of
    /// it, say why, and go through once the backoff has passed.
    #[tokio::test]
    async fn a_second_link_inside_the_backoff_is_refused_without_contacting_signal() {
        let world = World::new()
            .stopped_clock()
            .link(vec![Err(anyhow::anyhow!("network down"))])
            .link(vec![linked()]);
        let mut h = Harness::new(&world);
        assert_failed(&h.send(LINK).await, "link");
        let refused = h.send(SetupEvent::Retry).await;
        let SetupState::Failed { message, .. } = &refused else {
            panic!("a link inside the backoff left {refused:?}");
        };
        assert!(message.contains("account-safety"), "{message}");
        let links = |log: Vec<String>| log.iter().filter(|e| e.starts_with("link as")).count();
        assert_eq!(links(world.log()), 1, "{:?}", world.log());

        world.clock.advance(5 * 60 * 1_000);
        let state = h.send(SetupEvent::Retry).await;
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Linked
            }
        );
        assert_eq!(links(world.log()), 2, "{:?}", world.log());
    }

    /// A longer provisioning backoff saved under Advanced must hold setup back
    /// too, not just the default five minutes.
    #[tokio::test]
    async fn the_provisioning_backoff_saved_in_settings_is_the_one_applied() {
        let world = World::new()
            .stopped_clock()
            .link(vec![Err(anyhow::anyhow!("network down"))])
            .link(vec![linked()]);
        let paths = world.paths();
        std::fs::create_dir_all(&paths.root).expect("profile dir");
        std::fs::write(
            &paths.settings,
            br#"{"limits": {"provisioning_backoff_minutes": 30}}"#,
        )
        .expect("settings");
        let mut h = Harness::new(&world);
        assert_failed(&h.send(LINK).await, "link");
        world.clock.advance(10 * 60 * 1_000);
        assert_failed(&h.send(SetupEvent::Retry).await, "early retry");
        let links = |log: Vec<String>| log.iter().filter(|e| e.starts_with("link as")).count();
        assert_eq!(links(world.log()), 1, "{:?}", world.log());
        assert!(
            paths.provisioning.exists(),
            "the attempt wasn't recorded at the profile's provisioning path"
        );
    }

    /// Sending the captcha Signal asked for finishes the same attempt, so the
    /// backoff must not refuse it.
    #[tokio::test]
    async fn a_captcha_resubmission_is_not_a_new_provisioning_attempt() {
        let world = World::new()
            .stopped_clock()
            .code(Ok(RegistrationStep::CaptchaRequired))
            .code(Ok(RegistrationStep::CodeSent));
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(SetupEvent::NumberEntered {
            number: NUMBER.into(),
        })
        .await;
        let state = h
            .send(SetupEvent::CaptchaSolved {
                token: "tok".into(),
            })
            .await;
        assert_eq!(
            state,
            SetupState::EnterCode {
                number: NUMBER.into()
            },
            "{:?}",
            world.log()
        );
    }

    /// Every step of the linked path that fails (the link, a link that ends
    /// unscanned, history, starting the agent) shows the failure, and Retry
    /// carries on to done from there.
    #[tokio::test]
    async fn each_failed_linked_step_retries_to_done() {
        let world = World::new()
            .link(vec![Err(anyhow::anyhow!("network down"))])
            .link(vec![uri("sgnl://a")])
            .link(vec![linked()])
            .import(Err(anyhow::anyhow!("archive expired")))
            .fail_once("start agent");
        let mut h = Harness::new(&world);
        assert_failed(&h.send(LINK).await, "link");
        assert_failed(&h.send(SetupEvent::Retry).await, "unscanned link");
        let state = h.send(SetupEvent::Retry).await;
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Linked
            }
        );
        let access = SetupEvent::AccessChosen {
            access: AccessMode::Full,
        };
        assert_failed(&h.send(access).await, "history import");
        assert_failed(&h.send(SetupEvent::Retry).await, "agent start");
        let log = world.log();
        assert!(
            !log.iter().any(|e| e.contains("mcp add")),
            "clients registered while the agent failed to start: {log:?}"
        );
        let done = h.send(SetupEvent::Retry).await;
        assert!(matches!(done, SetupState::Done { .. }), "{done:?}");
    }

    /// Signal rate-limiting a code request must show Signal's own message and
    /// wait, and must not be retried: not by the driver, and not by Retry,
    /// which only goes back to the number screen. Asking again unprompted is
    /// how a number gets flagged.
    #[tokio::test]
    async fn a_rate_limited_code_request_shows_signals_wait_and_is_not_resent() {
        let limited = relay_signal::RateLimited {
            message: "verification session rate limited; next SMS in 3600s".into(),
            retry_after: Some(std::time::Duration::from_secs(3600)),
        };
        let world = World::new().code(Err(limited.into()));
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        let failed = h
            .send(SetupEvent::NumberEntered {
                number: NUMBER.into(),
            })
            .await;
        let SetupState::Failed { message, .. } = &failed else {
            panic!("a rate-limited request left {failed:?}");
        };
        assert!(
            message.contains("next SMS in 3600s") && message.contains("Try again in 3600s"),
            "Signal's wait not shown: {message}"
        );
        assert_eq!(h.send(SetupEvent::Retry).await, SetupState::EnterNumber);
        let requests = world
            .log()
            .into_iter()
            .filter(|e| e.starts_with("request code"))
            .count();
        assert_eq!(requests, 1, "{:?}", world.log());
    }

    /// When Signal rate-limits a code request for longer than our own
    /// backoff, a fresh attempt must wait out Signal's time, refused before
    /// Signal hears of it, and go through once that time has passed.
    #[tokio::test]
    async fn a_rate_limited_code_request_extends_the_backoff_to_signals_wait() {
        let limited = relay_signal::RateLimited {
            message: "slow down".into(),
            retry_after: Some(std::time::Duration::from_secs(7200)),
        };
        let world = World::new()
            .stopped_clock()
            .code(Err(limited.into()))
            .code(Ok(RegistrationStep::CodeSent));
        let mut h = Harness::new(&world);
        let number = SetupEvent::NumberEntered {
            number: NUMBER.into(),
        };
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        h.send(number.clone()).await;
        world.clock.advance(5 * 60 * 1_000 + 1_000);
        h.send(SetupEvent::Retry).await;
        let refused = h.send(number.clone()).await;
        let SetupState::Failed { message, .. } = &refused else {
            panic!("an attempt inside Signal's wait was not refused: {refused:?}");
        };
        assert!(message.contains("try again in 6899s"), "{message}");
        world.clock.advance(6899 * 1_000);
        h.send(SetupEvent::Retry).await;
        let sent = h.send(number).await;
        assert!(
            matches!(sent, SetupState::EnterCode { .. }),
            "{sent:?} {:?}",
            world.log()
        );
        let requests = world
            .log()
            .into_iter()
            .filter(|e| e.starts_with("request code"))
            .count();
        assert_eq!(requests, 2, "{:?}", world.log());
    }

    /// Every step of the own-number path that fails (requesting a code,
    /// verifying it, restarting Claude Desktop) can be retried, and a mistyped
    /// code is retried on the same Signal session rather than a new one.
    #[tokio::test]
    async fn each_failed_registration_step_retries_to_done() {
        let world = World::new()
            .code(Err(anyhow::anyhow!("rate limited")))
            .code(Ok(RegistrationStep::CodeSent))
            .code(Err(anyhow::anyhow!("wrong code")))
            .code(primary_registered())
            .fail_once("restart desktop");
        let mut h = Harness::new(&world);
        h.send(SetupEvent::AccountChosen {
            choice: AccountChoice::OwnNumber,
        })
        .await;
        let number = SetupEvent::NumberEntered {
            number: NUMBER.into(),
        };
        assert_failed(&h.send(number.clone()).await, "code request");
        assert_eq!(h.send(SetupEvent::Retry).await, SetupState::EnterNumber);
        h.send(number).await;
        let code = SetupEvent::CodeEntered {
            code: "111111".into(),
        };
        assert_failed(&h.send(code.clone()).await, "verification");
        assert_eq!(
            h.send(SetupEvent::Retry).await,
            SetupState::EnterCode {
                number: NUMBER.into()
            }
        );
        let state = h.send(code).await;
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Primary
            }
        );
        assert_eq!(
            world.opened_with().len(),
            1,
            "a retry opened a new Signal session: {:?}",
            world.log()
        );
        let done = h
            .send(SetupEvent::AccessChosen {
                access: AccessMode::Full,
            })
            .await;
        assert_failed(&h.send(SetupEvent::RestartDesktop).await, "restart");
        assert_eq!(h.send(SetupEvent::Retry).await, done);
        assert_eq!(h.send(SetupEvent::RestartDesktop).await, done);
    }

    fn any_event() -> impl Strategy<Value = SetupEvent> {
        use SetupEvent as E;
        let access = prop_oneof![Just(AccessMode::Full), Just(AccessMode::ReadOnly)];
        prop_oneof![
            Just(LINK),
            Just(E::AccountChosen {
                choice: AccountChoice::OwnNumber
            }),
            Just(E::NumberEntered {
                number: NUMBER.into()
            }),
            Just(E::CaptchaSolved { token: "t".into() }),
            Just(E::CodeEntered { code: "1".into() }),
            access.prop_map(|access| E::AccessChosen { access }),
            Just(E::RestartDesktop),
            Just(E::Retry),
        ]
    }

    fn any_link() -> impl Strategy<Value = Vec<anyhow::Result<LinkEvent>>> {
        prop_oneof![
            Just(()).prop_map(|_| vec![uri("sgnl://a"), linked()]),
            Just(()).prop_map(|_| vec![uri("sgnl://a")]),
            Just(()).prop_map(|_| vec![Err(anyhow::anyhow!("down"))]),
        ]
    }

    fn any_code() -> impl Strategy<Value = u8> {
        0u8..4
    }

    fn code_step(n: u8) -> anyhow::Result<RegistrationStep> {
        match n {
            0 => Ok(RegistrationStep::CaptchaRequired),
            1 => Ok(RegistrationStep::CodeSent),
            2 => primary_registered(),
            _ => Err(anyhow::anyhow!("rejected")),
        }
    }

    proptest! {
        /// Whatever the user does and whichever step fails, the agent starts and
        /// clients register only with the user's latest access choice already on
        /// disk, and never while setup still holds the Signal store open.
        #[test]
        fn the_agent_and_clients_only_ever_see_the_chosen_access_and_a_closed_store(
            events in prop::collection::vec(any_event(), 0..40),
            links in prop::collection::vec(any_link(), 10),
            codes in prop::collection::vec(any_code(), 20),
            imports_fail in prop::collection::vec(any::<bool>(), 10),
            agent_fails in any::<bool>(),
        ) {
            let mut world = World::new();
            for link in links {
                world = world.link(link);
            }
            for code in codes {
                world = world.code(code_step(code));
            }
            for fail in imports_fail {
                world = world.import(if fail { Err(anyhow::anyhow!("lost")) } else { Ok(vec![(1, None)]) });
            }
            if agent_fails {
                world = world.fail_once("start agent");
            }
            let runtime = tokio::runtime::Builder::new_current_thread().build().expect("runtime");
            let mut h = Harness::new(&world);
            let mut chosen = None;
            for event in events {
                if let (SetupState::ChooseAccess { .. }, SetupEvent::AccessChosen { access }) =
                    (h.driver.state(), &event)
                {
                    chosen = Some(format!("{access:?}"));
                }
                let before = world.log().len();
                runtime.block_on(h.send(event.clone()));
                let log = world.log();
                for (i, entry) in log.iter().enumerate().skip(before) {
                    let Some((_, saved)) = entry.split_once(", access ") else {
                        continue;
                    };
                    prop_assert_eq!(Some(saved), chosen.as_deref(), "{} after {:?}: {:?}", entry, event, log);
                    let opens = log.iter().take(i).filter(|e| e.starts_with("open ")).count();
                    let closes = log.iter().take(i).filter(|e| *e == "close store").count();
                    prop_assert_eq!(opens, closes, "{} with the store open: {:?}", entry, log);
                }
            }
        }
    }
}
