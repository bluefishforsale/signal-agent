//! `signal-agent history`: bringing in earlier messages by hand, after setup.
//! Deliberately not MCP tools: reading Signal Desktop's database asks the OS
//! for its key, and only the person at the computer should set that off.

use std::{io::Write, path::PathBuf};

use anyhow::bail;
use clap::Subcommand;
use relay_core::prelude::*;
use relay_setup::{
    driver::DesktopHistory,
    history::{ProfileStore, SignalDesktop},
};

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
            import_desktop(&desktop, out).await
        }
    }
}

async fn import_desktop(
    desktop: &dyn DesktopHistory,
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
    desktop.import(&mut |n| imported = n).await?;
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
        imported: Mutex<bool>,
    }

    #[async_trait]
    impl DesktopHistory for Desktop {
        fn found(&self) -> bool {
            self.found
        }
        async fn import(&self, progress: &mut (dyn FnMut(u64) + Send)) -> anyhow::Result<()> {
            *self.imported.lock().expect("imported") = true;
            for n in &self.progress {
                progress(*n);
            }
            Ok(())
        }
    }

    /// No Desktop where we looked must stop before anything is read, and say
    /// how to point at a Desktop kept somewhere else.
    #[tokio::test]
    async fn import_desktop_without_desktop_reads_nothing_and_suggests_from() {
        let desktop = Desktop {
            found: false,
            progress: vec![],
            imported: Mutex::new(false),
        };
        let mut out = Vec::new();
        let Err(e) = import_desktop(&desktop, &mut out).await else {
            panic!("imported from nowhere");
        };
        assert!(format!("{e:#}").contains("--from"), "{e:#}");
        assert!(!*desktop.imported.lock().expect("imported"));
    }

    /// A found Desktop is imported, and the user is told how many messages
    /// came in, since nothing else shows it.
    #[tokio::test]
    async fn import_desktop_reports_how_many_messages_came_in() {
        let desktop = Desktop {
            found: true,
            progress: vec![100, 200, 250],
            imported: Mutex::new(false),
        };
        let mut out = Vec::new();
        import_desktop(&desktop, &mut out).await.expect("import");
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.ends_with("Imported 250 messages from Signal Desktop.\n"),
            "{out:?}"
        );
    }
}
