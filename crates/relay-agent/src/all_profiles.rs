//! `signal-agent agent --all-profiles`: the one login item on every OS. It
//! runs each ready profile's agent as its own child process, so one crash
//! can't take the others down, and follows the `Supervisor` policy for when.

use std::{
    collections::BTreeMap, future::Future, path::PathBuf, process::Stdio, sync::Arc, time::Duration,
};

use anyhow::Context;

use futures::future::BoxFuture;
use relay_core::ProfileName;
use relay_setup::Profiles;
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
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Runs every ready profile's agent until the OS asks this process to stop.
pub async fn run_all_profiles() -> anyhow::Result<()> {
    let profiles = Profiles::system()?;
    let exe = std::env::current_exe().context("unable to locate this executable")?;
    #[cfg(windows)]
    if let Err(e) = end_children_with_this_process() {
        tracing::warn!("agents may outlive the supervisor: {e:#}");
    }
    supervise(
        move || ready_profiles(&profiles),
        Arc::new(Processes { exe }),
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

/// Each profile's agent as `<exe> agent --profile <name>`.
struct Processes {
    exe: PathBuf,
}

impl Launcher for Processes {
    fn launch(&self, profile: &ProfileName) -> anyhow::Result<Box<dyn Child>> {
        let child = tokio::process::Command::new(&self.exe)
            .args(["agent", "--profile", profile.as_ref()])
            .stdin(Stdio::null())
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

/// The profiles whose setup finished, in the order they were created.
fn ready_profiles(profiles: &Profiles) -> anyhow::Result<Vec<ProfileName>> {
    let mut ready = profiles.list()?;
    ready.retain(|p| profiles.paths(p).load_access().is_ok());
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
}
