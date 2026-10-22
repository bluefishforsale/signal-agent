//! `signal-agent agent --all-profiles`: the one login item on every OS. It
//! runs each ready profile's agent as its own child process, so one crash
//! can't take the others down, and follows the `Supervisor` policy for when.

use std::{
    collections::BTreeMap,
    future::Future,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::Context;

use futures::future::BoxFuture;
use relay_core::ProfileName;
use relay_setup::{Profiles, claude::Platform};
use relay_tools::{Clock, SystemClock};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
    time::MissedTickBehavior,
};

use crate::{Supervisor, SupervisorAction};

/// How often profiles.json is reread, beside after every child exit.
const RESCAN: Duration = Duration::from_secs(10);

/// How long an agent gets to close its stores after being asked to stop.
#[cfg(unix)]
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Runs every ready profile's agent until the OS asks this process to stop.
pub async fn run_all_profiles() -> anyhow::Result<()> {
    let profiles = Profiles::system()?;
    let exe = std::env::current_exe().context("unable to locate this executable")?;
    #[cfg(windows)]
    if let Err(e) = end_children_with_this_process() {
        tracing::warn!("agents may outlive the supervisor: {e:#}");
    }
    match installed_app(&exe, Platform::current()) {
        Some(app) => drop(tokio::spawn(check_for_updates(app))),
        None => tracing::info!(
            "no setup app beside \"{}\", so no update checks",
            exe.display()
        ),
    }
    supervise(
        move || ready_profiles(&profiles),
        Arc::new(Processes {
            exe,
            profiles: Profiles::system()?,
        }),
        Arc::new(SystemClock),
        crate::stop_requested(),
    )
    .await;
    Ok(())
}

/// Puts this process in a job that ends every process in it when this one
/// exits, so ending the supervisor's logon task ends its agents too.
// NOTE: Windows has no process group to stop with the task. Without this,
// uninstall ending the task would leave every profile's agent running.
#[cfg(windows)]
fn end_children_with_this_process() -> anyhow::Result<()> {
    use windows_sys::Win32::System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::GetCurrentProcess,
    };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: plain Win32 calls with valid pointers. The job handle is never
    // closed on purpose: closing it is what ends the job, which must happen
    // only when this process exits.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            anyhow::bail!("no job object: {}", std::io::Error::last_os_error());
        }
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 {
            anyhow::bail!("job limits refused: {}", std::io::Error::last_os_error());
        }
        if AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            anyhow::bail!(
                "unable to join the job: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

/// How often the supervisor asks the app to look for an update. The app
/// looks at GitHub at most once a day, so this only bounds how late a notice
/// can be after waking from sleep.
const UPDATE_CHECK_EVERY: Duration = Duration::from_secs(60 * 60);

/// Longest an update check may take before it is given up on.
const UPDATE_CHECK_LIMIT: Duration = Duration::from_secs(2 * 60);

/// Asks the app to look for an update now and every hour, so its notice
/// appears without the window open.
async fn check_for_updates(app: PathBuf) {
    let mut every = tokio::time::interval(UPDATE_CHECK_EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        match tokio::time::timeout(UPDATE_CHECK_LIMIT, update_check(&app).status()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => tracing::warn!("the update check exited ({status})"),
            Ok(Err(e)) => tracing::warn!("unable to run \"{}\": {e}", app.display()),
            Err(_) => tracing::warn!("the update check took over {UPDATE_CHECK_LIMIT:?}"),
        }
    }
}

/// The app's update check, which only ever notifies.
// NOTE: never anything but `--check-for-update`. Installing is the user's
// click in the app window (PRD anti-goal 6); the supervisor only asks.
fn update_check(app: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(app);
    command
        .arg("--check-for-update")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// The setup app installed beside the agent binary `exe`: the bundle's own
/// executable on macOS, `relay-app.exe` beside it on Windows. Linux ships no
/// app, and a dev build has none.
fn installed_app(exe: &Path, platform: Platform) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let app = match platform {
        Platform::MacOs => {
            let plist = std::fs::read_to_string(dir.parent()?.join("Info.plist")).ok()?;
            dir.join(bundle_executable(&plist)?)
        }
        Platform::Windows => dir.join("relay-app.exe"),
        Platform::Linux => return None,
    };
    (app != exe && app.is_file()).then_some(app)
}

/// `CFBundleExecutable` from an Info.plist's XML.
fn bundle_executable(plist: &str) -> Option<&str> {
    let after = plist.split("<key>CFBundleExecutable</key>").nth(1)?;
    let value = after.trim_start().strip_prefix("<string>")?;
    value.split("</string>").next().filter(|v| !v.is_empty())
}

/// Starts one profile's agent.
pub(crate) trait Launcher: Send + Sync {
    fn launch(&self, profile: &ProfileName) -> anyhow::Result<Box<dyn Child>>;
}

/// A running profile agent.
pub(crate) trait Child: Send {
    /// Resolves when the agent exits on its own.
    fn wait(&mut self) -> BoxFuture<'_, ()>;
    /// Asks the agent to stop and resolves once it has.
    fn stop(&mut self) -> BoxFuture<'_, ()>;
}

/// Keeps one agent running per profile `ready` lists until `shutdown`
/// resolves, then stops them all.
pub(crate) async fn supervise(
    ready: impl Fn() -> anyhow::Result<Vec<ProfileName>>,
    launcher: Arc<dyn Launcher>,
    clock: Arc<dyn Clock>,
    shutdown: impl Future<Output = ()>,
) {
    let mut policy = Supervisor::default();
    let mut running: BTreeMap<ProfileName, oneshot::Sender<()>> = BTreeMap::new();
    let mut children = JoinSet::new();
    let (exits_tx, mut exits) = mpsc::unbounded_channel();
    let mut rescan = tokio::time::interval(RESCAN);
    rescan.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = rescan.tick() => {}
            Some(profile) = exits.recv() => {
                running.remove(&profile);
                policy.exited(&profile, clock.now_ms());
            }
        }
        let profiles = match ready() {
            Ok(profiles) => profiles,
            // Keep what runs as it is: an unreadable list says nothing about
            // which profiles were removed.
            Err(e) => {
                tracing::warn!("profiles unchanged this round: {e:#}");
                continue;
            }
        };
        for action in policy.reconcile(&profiles, clock.now_ms()) {
            match action {
                SupervisorAction::Start(profile) => match launcher.launch(&profile) {
                    Ok(child) => {
                        let (stop, stopped) = oneshot::channel();
                        running.insert(profile.clone(), stop);
                        children.spawn(watch(profile, child, stopped, exits_tx.clone()));
                    }
                    Err(e) => {
                        tracing::warn!("unable to start \"{profile}\": {e:#}");
                        policy.exited(&profile, clock.now_ms());
                    }
                },
                SupervisorAction::Stop(profile) => {
                    if let Some(stop) = running.remove(&profile) {
                        let _ = stop.send(());
                    }
                }
            }
        }
        while children.try_join_next().is_some() {}
    }
    drop(running);
    while children.join_next().await.is_some() {}
}

/// Each profile's agent as `<exe> agent --profile <name>`, logging to that
/// profile's agent log.
struct Processes {
    exe: PathBuf,
    profiles: Profiles,
}

impl Launcher for Processes {
    fn launch(&self, profile: &ProfileName) -> anyhow::Result<Box<dyn Child>> {
        let paths = self.profiles.paths(profile);
        std::fs::create_dir_all(&paths.root)
            .with_context(|| format!("unable to create \"{}\"", paths.root.display()))?;
        // NOTE: launchd and the Windows logon task discard the supervisor's
        // own stderr, so an inherited one would lose every agent's errors.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.agent_log)
            .with_context(|| format!("unable to open \"{}\"", paths.agent_log.display()))?;
        let child = tokio::process::Command::new(&self.exe)
            .args(["agent", "--profile", profile.as_ref()])
            .stdin(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("unable to run \"{}\"", self.exe.display()))?;
        Ok(Box::new(Process {
            profile: profile.clone(),
            child,
        }))
    }
}

struct Process {
    profile: ProfileName,
    child: tokio::process::Child,
}

impl Child for Process {
    fn wait(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            match self.child.wait().await {
                Ok(status) => tracing::warn!("the \"{}\" agent exited ({status})", self.profile),
                Err(e) => tracing::warn!("lost track of the \"{}\" agent: {e}", self.profile),
            }
        })
    }

    fn stop(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // NOTE: ask first. SIGTERM lets the agent close its stores, which
            // a kill skips. Windows has no SIGTERM for a console-less child,
            // so there it is killed; SQLite recovers from that on next open.
            #[cfg(unix)]
            if let Some(pid) = self.child.id().and_then(|id| i32::try_from(id).ok()) {
                // SAFETY: `pid` is our own child and not yet reaped, since
                // `id()` returns None once it has been.
                unsafe { libc::kill(pid, libc::SIGTERM) };
                if tokio::time::timeout(STOP_GRACE, self.child.wait())
                    .await
                    .is_ok()
                {
                    return;
                }
            }
            if let Err(e) = self.child.kill().await {
                tracing::warn!("unable to stop the \"{}\" agent: {e}", self.profile);
            }
        })
    }
}

/// The profiles whose setup finished and isn't running again, in the order
/// they were created.
fn ready_profiles(profiles: &Profiles) -> anyhow::Result<Vec<ProfileName>> {
    let mut ready = profiles.list()?;
    ready.retain(|p| {
        let paths = profiles.paths(p);
        paths.load_access().is_ok() && !relay_setup::setup_running(&paths)
    });
    Ok(ready)
}

/// Waits for `child` to exit, or stops it when `stop` fires or is dropped,
/// then reports `profile` on `exits`.
async fn watch(
    profile: ProfileName,
    mut child: Box<dyn Child>,
    mut stop: oneshot::Receiver<()>,
    exits: mpsc::UnboundedSender<ProfileName>,
) {
    let stopping = tokio::select! {
        () = child.wait() => false,
        _ = &mut stop => true,
    };
    if stopping {
        child.stop().await;
    }
    // The supervisor may be gone already, during shutdown.
    let _ = exits.send(profile);
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    use super::*;

    fn name(s: &str) -> ProfileName {
        ProfileName::new(s).expect("valid profile name")
    }

    /// Whether `check` holds within a second. A lock let go of can stay held
    /// for an instant longer: a process a parallel test is spawning shares
    /// its descriptor until it execs.
    fn soon(check: impl Fn() -> bool) -> bool {
        (0..100).any(|_| {
            check() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        })
    }

    /// Milliseconds of tokio's clock, which a paused test runtime advances.
    struct TokioClock(tokio::time::Instant);

    impl Clock for TokioClock {
        fn now_ms(&self) -> u64 {
            self.0.elapsed().as_millis() as u64
        }
    }

    /// What happened to the fake children, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        Launched(ProfileName),
        Stopped(ProfileName),
    }

    /// Children that run until stopped, or exit as soon as they start when
    /// `crash` is set. Records each launch and stop with tokio's time.
    struct FakeLauncher {
        crash: bool,
        start: tokio::time::Instant,
        events: Mutex<Vec<(Duration, Event)>>,
    }

    impl FakeLauncher {
        fn new(crash: bool) -> Arc<Self> {
            Arc::new(Self {
                crash,
                start: tokio::time::Instant::now(),
                events: Mutex::default(),
            })
        }

        fn record(&self, event: Event) {
            let at = self.start.elapsed();
            self.events.lock().expect("events").push((at, event));
        }

        fn events(&self) -> Vec<Event> {
            let events = self.events.lock().expect("events");
            events.iter().map(|(_, e)| e.clone()).collect()
        }

        fn launch_times(&self) -> Vec<Duration> {
            let events = self.events.lock().expect("events");
            let launches = events
                .iter()
                .filter(|(_, e)| matches!(e, Event::Launched(_)));
            launches.map(|(at, _)| *at).collect()
        }
    }

    impl Launcher for Arc<FakeLauncher> {
        fn launch(&self, profile: &ProfileName) -> anyhow::Result<Box<dyn Child>> {
            self.record(Event::Launched(profile.clone()));
            Ok(Box::new(FakeChild {
                profile: profile.clone(),
                launcher: self.clone(),
            }))
        }
    }

    struct FakeChild {
        profile: ProfileName,
        launcher: Arc<FakeLauncher>,
    }

    impl Child for FakeChild {
        fn wait(&mut self) -> BoxFuture<'_, ()> {
            if self.launcher.crash {
                return Box::pin(async {});
            }
            Box::pin(std::future::pending())
        }
        fn stop(&mut self) -> BoxFuture<'_, ()> {
            self.launcher.record(Event::Stopped(self.profile.clone()));
            Box::pin(async {})
        }
    }

    /// A profile still in setup has its Signal store open in the setup
    /// process, so only profiles whose access choice was saved may start.
    #[test]
    fn only_profiles_that_finished_setup_are_ready() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profiles = Profiles::under(tmp.path().to_owned());
        let personal = profiles.ensure_default().expect("default");
        let bot = profiles.create("bot").expect("bot");
        profiles
            .paths(&bot)
            .save_access(relay_core::AccessMode::Full)
            .expect("save");
        assert_eq!(ready_profiles(&profiles).expect("ready"), [bot]);
        profiles
            .paths(&personal)
            .save_access(relay_core::AccessMode::ReadOnly)
            .expect("save");
        assert_eq!(ready_profiles(&profiles).expect("ready").len(), 2);
    }

    /// Setup run again on a set-up profile needs its stores to itself, so
    /// while setup holds the profile the supervisor must not count it ready,
    /// even with its old access choice still saved, and must again after.
    #[test]
    fn a_profile_with_setup_running_is_not_ready_until_setup_lets_go() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let profiles = Profiles::under(tmp.path().to_owned());
        let personal = profiles.ensure_default().expect("default");
        let paths = profiles.paths(&personal);
        paths
            .save_access(relay_core::AccessMode::Full)
            .expect("save");
        let setup = relay_setup::SetupLock::try_acquire(&paths)
            .expect("lock")
            .expect("free");
        assert_eq!(ready_profiles(&profiles).expect("ready"), []);
        drop(setup);
        assert!(
            soon(|| ready_profiles(&profiles).expect("ready") == [personal.clone()]),
            "still not ready after setup let go"
        );
    }

    /// launchd and the Windows logon task throw the supervisor's stderr away,
    /// so an agent that fails at start (no key, no account) must still leave
    /// its reason in its profile's log.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_supervised_agents_errors_land_in_its_profile_log() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let exe = tmp.path().join("signal-agent");
        std::fs::write(&exe, "#!/bin/sh\necho \"failed for $3\" >&2\n").expect("script");
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let profiles = Profiles::under(tmp.path().join("data"));
        let processes = Processes { exe, profiles };
        let mut child = processes.launch(&name("personal")).expect("launch");
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("child exited");
        let log = processes.profiles.paths(&name("personal")).agent_log;
        let logged = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            logged.contains("failed for personal"),
            "the agent's stderr is not in \"{}\": {logged:?}",
            log.display()
        );
    }

    /// Stopping a real agent process must end it promptly, by SIGTERM so it
    /// can close its stores, rather than leave it holding the profile.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_child_process_ends_it() {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .expect("spawn sleep");
        let mut process = Process {
            profile: name("personal"),
            child,
        };
        tokio::time::timeout(Duration::from_secs(5), process.stop())
            .await
            .expect("stop hung");
        let status = process.child.try_wait().expect("status");
        assert!(
            status.is_some_and(|s| !s.success()),
            "child still running after stop: {status:?}"
        );
    }

    /// A child that keeps crashing is restarted, but each wait is longer
    /// than the last, so a crash loop can't reconnect to Signal every few
    /// seconds.
    #[tokio::test(start_paused = true)]
    async fn a_crashing_child_is_restarted_with_growing_waits() {
        let fake = FakeLauncher::new(true);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let running = tokio::spawn(supervise(
            || Ok(vec![name("personal")]),
            Arc::new(fake.clone()),
            Arc::new(TokioClock(tokio::time::Instant::now())),
            async {
                let _ = stop_rx.await;
            },
        ));
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
        let _ = stop_tx.send(());
        running.await.expect("supervisor");
        let launches = fake.launch_times();
        let waits: Vec<_> = launches.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            waits.len() >= 4,
            "a crashing agent wasn't restarted: {launches:?}"
        );
        assert!(
            waits.first().is_some_and(|w| *w >= Duration::from_secs(5)),
            "restarted without waiting: {waits:?}"
        );
        assert!(
            waits.windows(2).all(|w| w[1] >= w[0]),
            "the wait shrank during a crash loop: {waits:?}"
        );
        assert!(
            waits
                .iter()
                .all(|w| *w <= Duration::from_secs(5 * 60) + RESCAN),
            "waited past the five-minute cap: {waits:?}"
        );
    }

    /// Every ready profile gets one child, a running one is never started
    /// twice however many rescans pass, and a profile that drops out of the
    /// list has its child stopped.
    #[tokio::test(start_paused = true)]
    async fn each_ready_profile_gets_one_child_and_a_removed_one_is_stopped() {
        let ready = Arc::new(Mutex::new(vec![name("personal"), name("bot")]));
        let fake = FakeLauncher::new(false);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let listed = ready.clone();
        let running = tokio::spawn(supervise(
            move || Ok(listed.lock().expect("ready").clone()),
            Arc::new(fake.clone()),
            Arc::new(TokioClock(tokio::time::Instant::now())),
            async {
                let _ = stop_rx.await;
            },
        ));
        tokio::time::sleep(RESCAN * 3).await;
        ready.lock().expect("ready").retain(|p| *p != name("bot"));
        tokio::time::sleep(RESCAN * 3).await;
        let _ = stop_tx.send(());
        running.await.expect("supervisor");
        assert_eq!(
            fake.events(),
            [
                Event::Launched(name("personal")),
                Event::Launched(name("bot")),
                Event::Stopped(name("bot")),
                Event::Stopped(name("personal")),
            ]
        );
    }

    /// The supervisor asks the app installed beside it to check for updates:
    /// the bundle's real executable on macOS, relay-app.exe on Windows, and
    /// nothing on Linux or in a dev build, where there is no app.
    #[test]
    fn the_update_check_runs_the_app_installed_beside_the_agent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let macos = tmp.path().join("MCP Relay for Signal.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).expect("bundle");
        std::fs::write(
            macos.parent().expect("Contents").join("Info.plist"),
            "<dict>\n\t<key>CFBundleExecutable</key>\n\t<string>relay-app</string>\n</dict>",
        )
        .expect("plist");
        let agent = macos.join("signal-agent");
        assert_eq!(installed_app(&agent, Platform::MacOs), None, "no app yet");
        std::fs::write(macos.join("relay-app"), b"").expect("app");
        assert_eq!(
            installed_app(&agent, Platform::MacOs),
            Some(macos.join("relay-app"))
        );

        let windows = tmp.path().join("win");
        std::fs::create_dir_all(&windows).expect("dir");
        std::fs::write(windows.join("relay-app.exe"), b"").expect("app");
        let agent = windows.join("signal-agent.exe");
        assert_eq!(
            installed_app(&agent, Platform::Windows),
            Some(windows.join("relay-app.exe"))
        );
        assert_eq!(installed_app(&agent, Platform::Linux), None);
    }

    /// Whatever schedules it, the supervisor's update check may only ask
    /// the app to look and notify; installing waits for the user's click.
    #[test]
    fn the_supervisors_update_check_only_ever_notifies() {
        let command = update_check(Path::new("/Applications/X.app/Contents/MacOS/relay-app"));
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, ["--check-for-update"]);
    }
}
