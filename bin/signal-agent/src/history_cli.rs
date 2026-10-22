//! `signal-agent history`: bringing in earlier messages by hand, after setup.
//! Deliberately not MCP tools: reading Signal Desktop's database asks the OS
//! for its key, and only the person at the computer should set that off.

use std::{io::Write, path::PathBuf};

use anyhow::{Context, bail};
use clap::Subcommand;
use relay_core::prelude::*;
use relay_setup::{
    driver::DesktopHistory,
    history::{ProfileStore, SignalDesktop},
};
use relay_signal::{PresageSignal, SignalBackend};

#[derive(Subcommand)]
pub enum HistoryCmd {
    /// Import Signal Desktop's messages into this profile.
    ImportDesktop {
        /// Signal Desktop's data folder, when it isn't in the usual place.
        #[arg(long)]
        from: Option<PathBuf>,
    },
}

pub async fn run(
    cmd: HistoryCmd,
    profile: &ProfileName,
    out: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    match cmd {
        HistoryCmd::ImportDesktop { from } => {
            let paths = ProfilePaths::for_profile(profile)?;
            let store = ProfileStore::new(profile, &paths, relay_secrets::platform_store()?);
            let desktop = match from {
                Some(dir) => SignalDesktop::new(Some(dir), store),
                None => SignalDesktop::usual_place(store),
            };
            import_desktop(&desktop, linked_aci(profile, &paths).await?, out).await
        }
    }
}

/// The profile's own account, read from its Signal store without contacting
/// Signal.
// NOTE: the agent may have this store open. Only the registration is read and
// the store is closed at once; nothing here writes to it.
async fn linked_aci(profile: &ProfileName, paths: &ProfilePaths) -> anyhow::Result<ServiceId> {
    let key = relay_secrets::platform_store()?
        .get(profile)?
        .with_context(|| format!("profile \"{profile}\" has no saved key; set it up first"))?;
    let signal = PresageSignal::open(&paths.signal_store, key.as_bytes()).await?;
    Ok(signal.account().await?.service_id)
}

async fn import_desktop(
    desktop: &dyn DesktopHistory,
    our_aci: ServiceId,
    out: &mut (impl Write + Send),
) -> anyhow::Result<()> {
    if !desktop.found() {
        bail!(
            "Signal Desktop's data isn't in its usual place; if it's somewhere else, \
             pass its folder with --from"
        );
    }
    writeln!(
        out,
        "Importing from Signal Desktop. Your computer may ask to let this read Signal \
         Desktop's key; allow it, or the import can't open the database."
    )?;
    let mut imported = 0;
    desktop.import(our_aci, &mut |n| imported = n).await?;
    writeln!(out, "Imported {imported} messages from Signal Desktop.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    /// Signal Desktop as a fake: whether it's there, and what importing it
    /// reports.
    struct Desktop {
        found: bool,
        progress: Vec<u64>,
        imported_as: Mutex<Option<ServiceId>>,
    }

    #[async_trait]
    impl DesktopHistory for Desktop {
        fn found(&self) -> bool {
            self.found
        }
        async fn import(
            &self,
            our_aci: ServiceId,
            progress: &mut (dyn FnMut(u64) + Send),
        ) -> anyhow::Result<()> {
            *self.imported_as.lock().expect("imported") = Some(our_aci);
            for n in &self.progress {
                progress(*n);
            }
            Ok(())
        }
    }

    fn aci() -> ServiceId {
        ServiceId::new(uuid::Uuid::from_u128(7))
    }

    /// No Desktop where we looked must stop before anything is read, and say
    /// how to point at a Desktop kept somewhere else.
    #[tokio::test]
    async fn import_desktop_without_desktop_reads_nothing_and_suggests_from() {
        let desktop = Desktop {
            found: false,
            progress: vec![],
            imported_as: Mutex::new(None),
        };
        let mut out = Vec::new();
        let Err(e) = import_desktop(&desktop, aci(), &mut out).await else {
            panic!("imported from nowhere");
        };
        assert!(format!("{e:#}").contains("--from"), "{e:#}");
        assert_eq!(*desktop.imported_as.lock().expect("imported"), None);
    }

    /// A found Desktop is imported as the profile's own account, so a Desktop
    /// signed in to someone else is refused, and the user is told how many
    /// messages came in, since nothing else shows it.
    #[tokio::test]
    async fn import_desktop_reports_how_many_messages_came_in() {
        let desktop = Desktop {
            found: true,
            progress: vec![100, 200, 250],
            imported_as: Mutex::new(None),
        };
        let mut out = Vec::new();
        import_desktop(&desktop, aci(), &mut out)
            .await
            .expect("import");
        assert_eq!(*desktop.imported_as.lock().expect("imported"), Some(aci()));
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.ends_with("Imported 250 messages from Signal Desktop.\n"),
            "{out:?}"
        );
    }
}
