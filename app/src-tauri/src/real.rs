//! relay-setup's driver behind the window, against Signal's production
//! service and this computer's Claude apps.
//!
//! NOTE: this talks to real Signal. Agents never run it into linking or
//! registration (the owner's account could be banned); development runs use
//! `--features mock-driver`.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use async_trait::async_trait;
use relay_core::prelude::*;
use relay_secrets::StoreKey;
use relay_setup::{
    claude::{Clients, ServerEntry},
    driver::{Driver, HistoryImport, HistorySkip, LoginItem, OpenProvisioner, Services},
    history::{ProfileStore, SignalDesktop},
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

/// Setup of the default profile, starting at `start`.
pub fn setup(start: SetupState) -> anyhow::Result<Box<dyn Setup>> {
    let profile = ProfileName::default_profile();
    let server = ServerEntry::for_profile(&profile, &agent()?, ["mcp", "--profile", &profile])?;
    let paths = ProfilePaths::for_profile(&profile)?;
    let store = ProfileStore::new(&profile, &paths, relay_secrets::platform_store()?);
    let services = Services {
        signal: Box::new(Presage),
        secrets: relay_secrets::platform_store()?,
        history: Box::new(NoHistoryYet),
        desktop: Box::new(SignalDesktop::usual_place(store)),
        login: Box::new(AgentAtLogin),
        clients: Clients::system(),
    };
    Ok(Box::new(Driver::new(
        start, profile, paths, server, services,
    )))
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

// Integration seam: link-and-sync import (lane L7) needs the archive and key
// from linking, which the Provisioner contract doesn't carry yet (issue #34).
struct NoHistoryYet;

#[async_trait]
impl HistoryImport for NoHistoryYet {
    async fn import(
        &self,
        _: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> anyhow::Result<Option<HistoryGap>> {
        Ok(None)
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
    use std::{sync::mpsc, time::Duration};

    use futures::stream::BoxStream;
    use relay_signal::{Account, LinkEvent, RegistrationStep};

    use super::*;
    use crate::{session::Session, view::View};

    struct Phone;

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
    }

    #[async_trait]
    impl OpenProvisioner for Phone {
        async fn open(&self, _: &Path, _: &StoreKey) -> anyhow::Result<Box<dyn Provisioner>> {
            Ok(Box::new(Phone))
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

    /// A phone that never answers the history long-poll.
    struct PhoneNeverSends;

    #[async_trait]
    impl HistoryImport for PhoneNeverSends {
        async fn import(
            &self,
            _: &mut (dyn FnMut(u64, Option<u64>) + Send),
        ) -> anyhow::Result<Option<HistoryGap>> {
            futures::future::pending().await
        }
    }

    fn faked(dir: &Path) -> Box<dyn Setup> {
        faked_with(dir, Box::new(NoHistoryYet))
    }

    fn faked_with(dir: &Path, history: Box<dyn HistoryImport>) -> Box<dyn Setup> {
        let profile = ProfileName::default_profile();
        let server = ServerEntry::for_profile(&profile, &dir.join("signal-agent"), ["mcp"])
            .expect("server entry");
        let services = Services {
            signal: Box::new(Phone),
            secrets: Box::new(relay_secrets::MemorySecretStore::default()),
            history,
            desktop: Box::new(SignalDesktop::new(
                None,
                ProfileStore::new(
                    &profile,
                    &ProfilePaths::under(dir.to_path_buf(), &profile),
                    Box::new(relay_secrets::MemorySecretStore::default()),
                ),
            )),
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
            faked_with(dir.path(), Box::new(PhoneNeverSends)),
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
