//! relay-setup's driver behind the window, against Signal's production
//! service and this computer's Claude apps.
//!
//! NOTE: this talks to real Signal. Agents never run it into linking or
//! registration (the owner's account could be banned); development runs use
//! `--features mock-driver`.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_secrets::StoreKey;
use relay_setup::{
    AgentKeys, SetupLock, StoreLock,
    claude::{Clients, ServerEntry},
    driver::{Driver, HistorySkip, LoginItem, OpenProvisioner, Services},
    history::{LinkAndSync, ProfileStore, SignalDesktop},
    prelude::*,
};
use relay_signal::{PresageProvisioner, Provisioner, Servers};

use crate::session::{Setup, Show};

#[async_trait]
impl Setup for Driver {
    async fn dispatch(&mut self, event: SetupEvent, show: Show<'_>) {
        Driver::dispatch(self, event, show).await;
    }

    fn state(&self) -> &SetupState {
        Driver::state(self)
    }

    fn history_skip(&self) -> Option<HistorySkip> {
        Some(Driver::history_skip(self))
    }
}

/// Setup of `profile`, starting at `start`.
pub fn setup(start: SetupState, profile: ProfileName) -> anyhow::Result<Box<dyn Setup>> {
    let agent = agent()?;
    let server = ServerEntry::for_profile(&profile, &agent, ["mcp", "--profile", &profile])?;
    let paths = ProfilePaths::for_profile(&profile)?;
    // NOTE: every key read goes through the bundled signal-agent, never the
    // app itself, so the Keychain item keeps trusting the program the login
    // item runs (#112).
    let store = || ProfileStore::new(&profile, &paths, Box::new(AgentKeys::new(agent.clone())));
    let services = Services {
        signal: Box::new(Presage),
        secrets: Box::new(AgentKeys::new(agent.clone())),
        history: Box::new(LinkAndSync::new(store())),
        desktop: Box::new(SignalDesktop::usual_place(store())),
        login: Box::new(AgentAtLogin),
        clients: Clients::system(),
    };
    Ok(Box::new(Driver::new(
        start, profile, paths, server, services,
    )))
}

/// Removes `profile` from this computer, keeping its messages and key unless
/// `delete_data`.
pub fn remove(profile: &ProfileName, delete_data: bool) -> anyhow::Result<Vec<StepOutcome>> {
    let agent = agent()?;
    let profiles = Profiles::system()?;
    let clients = Clients::system();
    let secrets = AgentKeys::new(agent.clone());
    let entry = ServerEntry::for_profile(profile, &agent, ["mcp", "--profile", profile])?;
    let paths = profiles.paths(profile);
    let stop = Step::new(format!("Stop the agent for \"{profile}\""), || {
        agent_stopped(profile, &paths, AGENT_STOP_WAIT)
    });
    let steps = profiles.removal_steps(profile, &clients, &entry, stop, &secrets, delete_data)?;
    Ok(run_steps(steps))
}

/// How long an agent gets to stop: it checks for setup's lock every two
/// seconds, then closes its stores.
const AGENT_STOP_WAIT: Duration = Duration::from_secs(30);

/// Waits up to `limit` for `profile`'s agent to let go of its stores,
/// holding setup's lock meanwhile so the agent stops and nothing starts one.
// NOTE: both locks are let go of before the profile folder is deleted, since
// Windows can't delete a folder with an open file; the access choice is gone
// by then, so nothing starts the agent again.
fn agent_stopped(
    profile: &ProfileName,
    paths: &ProfilePaths,
    limit: Duration,
) -> anyhow::Result<()> {
    let Some(_setup) = SetupLock::try_acquire(paths)? else {
        bail!("setup is running on \"{profile}\"; finish or close it first");
    };
    let deadline = Instant::now() + limit;
    while StoreLock::try_acquire(paths)?.is_none() {
        if Instant::now() >= deadline {
            bail!(
                "the agent for \"{profile}\" is still running after {} seconds",
                limit.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// The access choice `profile`'s last setup saved, if it finished.
pub fn saved_access(profile: &ProfileName) -> Option<AccessMode> {
    let paths = ProfilePaths::for_profile(profile).ok()?;
    paths.load_access().ok()
}

/// The `signal-agent` binary shipped beside the app, which Claude launches.
pub fn agent() -> anyhow::Result<PathBuf> {
    let me = std::env::current_exe().context("unable to locate this app")?;
    let agent = me.with_file_name(format!("signal-agent{}", std::env::consts::EXE_SUFFIX));
    if !agent.is_file() {
        bail!("no signal-agent beside the app at \"{}\"", agent.display());
    }
    Ok(agent)
}

struct Presage;

#[async_trait]
impl OpenProvisioner for Presage {
    async fn open(&self, path: &Path, key: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
        let provisioner = PresageProvisioner::open(path, key.as_bytes(), Servers::Production)
            .await
            .with_context(|| format!("unable to open the Signal store \"{}\"", path.display()))?;
        Ok(Box::new(provisioner))
    }
}

struct AgentAtLogin;

impl LoginItem for AgentAtLogin {
    fn register(&self, profile: &ProfileName) -> anyhow::Result<()> {
        relay_agent::login::register(profile)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use futures::stream::BoxStream;
    use relay_signal::{Account, HistoryTransfer, LinkEvent, RegistrationStep};

    use super::*;
    use crate::{session::Session, view::View};

    /// A phone that links at once, and then either offers no history or
    /// never answers the history long-poll.
    #[derive(Clone, Copy)]
    struct Phone {
        stalls_history: bool,
    }

    #[async_trait]
    impl Provisioner for Phone {
        fn link(&self, device_name: String) -> BoxStream<'static, anyhow::Result<LinkEvent>> {
            let linked = LinkEvent::Linked {
                account: Account {
                    service_id: ServiceId::new(uuid::Uuid::from_u128(1)),
                    phone_number: None,
                    kind: ProfileKind::Linked,
                    device_name,
                },
            };
            let uri = LinkEvent::Uri {
                uri: "sgnl://linkdevice?uuid=1".into(),
            };
            Box::pin(futures::stream::iter([Ok(uri), Ok(linked)]))
        }

        async fn request_code(
            &self,
            _: &str,
            _: Option<&str>,
            _: bool,
        ) -> anyhow::Result<RegistrationStep> {
            bail!("not scripted")
        }

        async fn verify_code(&self, _: &str) -> anyhow::Result<RegistrationStep> {
            bail!("not scripted")
        }

        async fn transfer_history(&self, _: &Path) -> anyhow::Result<HistoryTransfer> {
            if self.stalls_history {
                futures::future::pending::<()>().await;
            }
            Ok(HistoryTransfer::NotOffered)
        }
    }

    #[async_trait]
    impl OpenProvisioner for Phone {
        async fn open(&self, _: &Path, _: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
            Ok(Box::new(*self))
        }
    }

    struct NoLogin;

    impl LoginItem for NoLogin {
        fn register(&self, _: &ProfileName) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct NoClaude;

    impl relay_setup::claude::Runner for NoClaude {
        fn run(&self, _: &Path, _: &[String]) -> anyhow::Result<relay_setup::claude::RunOutput> {
            bail!("no Claude here")
        }
    }

    fn faked(dir: &Path) -> Box<dyn Setup> {
        faked_with(
            dir,
            Phone {
                stalls_history: false,
            },
        )
    }

    fn faked_with(dir: &Path, phone: Phone) -> Box<dyn Setup> {
        let profile = ProfileName::default_profile();
        let server = ServerEntry::for_profile(&profile, &dir.join("signal-agent"), ["mcp"])
            .expect("server entry");
        let store = || {
            ProfileStore::new(
                &profile,
                &ProfilePaths::under(dir.to_path_buf(), &profile),
                Box::new(relay_secrets::MemorySecretStore::default()),
            )
        };
        let services = Services {
            signal: Box::new(phone),
            secrets: Box::new(relay_secrets::MemorySecretStore::default()),
            history: Box::new(LinkAndSync::new(store())),
            desktop: Box::new(SignalDesktop::new(None, store())),
            login: Box::new(NoLogin),
            clients: Clients::new(
                relay_setup::claude::Platform::current(),
                Box::new(NoClaude),
                None,
                None,
            ),
        };
        let paths = ProfilePaths::under(dir.to_path_buf(), &profile);
        Box::new(Driver::new(
            SetupState::start("Mac"),
            profile,
            paths,
            server,
            services,
        ))
    }

    /// Removing an account deletes its files only after its agent lets go of
    /// them. Any agent, even one the stdio shim started outside the
    /// supervisor, lets go when it sees setup's lock, so the wait must hold
    /// that lock and return only once the store lock is free.
    #[test]
    fn waiting_for_an_agent_holds_the_setup_lock_until_the_agent_lets_go() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::new("bot").expect("name");
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &profile);
        let stores = relay_setup::StoreLock::try_acquire(&paths)
            .expect("lock")
            .expect("free");
        let agent_paths = paths.clone();
        let agent = std::thread::spawn(move || {
            while !relay_setup::setup_running(&agent_paths) {
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(stores);
        });

        let waited = agent_stopped(&profile, &paths, Duration::from_secs(5));
        assert!(waited.is_ok(), "{waited:?}");
        agent.join().expect("stand-in agent");
    }

    /// An agent that never lets go must fail the wait, so removal stops
    /// before deleting anything under it.
    #[test]
    fn waiting_for_an_agent_that_never_lets_go_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::new("bot").expect("name");
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &profile);
        let _stores = relay_setup::StoreLock::try_acquire(&paths)
            .expect("lock")
            .expect("free");

        let waited = agent_stopped(&profile, &paths, Duration::from_millis(300));
        assert!(waited.is_err(), "returned while the agent held the stores");
    }

    /// The window drives relay-setup's own driver: a scan on the phone must
    /// show the QR from Signal's URI, then the access question, through the
    /// same session the page talks to.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_real_driver_shows_the_qr_then_asks_for_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel();
        let (session, run) = Session::new(faked(dir.path()), move |view: &View| {
            tx.send(view.clone()).unwrap();
        });
        tokio::spawn(run);
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        let mut screens = Vec::new();
        while let Ok(view) = rx.recv_timeout(Duration::from_secs(10)) {
            let asked = matches!(view, View::ChooseAccess { .. });
            screens.push(view);
            if asked {
                break;
            }
        }
        assert!(
            screens
                .iter()
                .any(|v| matches!(v, View::Linking { qr: Some(_), .. })),
            "no QR shown: {screens:?}"
        );
        assert!(
            matches!(
                screens.last(),
                Some(View::ChooseAccess {
                    imports_history: true
                })
            ),
            "{screens:?}"
        );
    }

    /// Skip in the window must reach an import that is waiting on the phone,
    /// which holds the driver, rather than queue behind it for up to an hour.
    #[tokio::test(flavor = "multi_thread")]
    async fn skip_in_the_window_stops_a_history_import_waiting_on_the_phone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, rx) = mpsc::channel();
        let (session, run) = Session::new(
            faked_with(
                dir.path(),
                Phone {
                    stalls_history: true,
                },
            ),
            move |view: &View| {
                tx.send(view.clone()).unwrap();
            },
        );
        tokio::spawn(run);
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        session.send(SetupEvent::AccessChosen {
            access: AccessMode::Full,
        });
        let next = || rx.recv_timeout(Duration::from_secs(10));
        while let Ok(view) = next() {
            if matches!(view, View::ImportingHistory { .. }) {
                break;
            }
        }
        session.send(SetupEvent::SkipHistory);
        let mut screens = Vec::new();
        while let Ok(view) = next() {
            let done = matches!(view, View::Done { .. });
            screens.push(view);
            if done {
                break;
            }
        }
        assert!(
            matches!(screens.last(), Some(View::Done { .. })),
            "skip never reached the import: {screens:?}"
        );
    }
}
