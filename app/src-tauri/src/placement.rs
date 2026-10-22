//! macOS runs a downloaded app from a random read-only copy (App
//! Translocation) until it is moved, and Claude's config would then point at a
//! path that vanishes. So setup refuses to start until the app lives in an
//! Applications folder.

use std::{
    fs,
    path::{Path, PathBuf},
    process,
};

use anyhow::{Context, bail};

/// Where the running app is, as far as setup cares.
#[derive(Debug, PartialEq, Eq)]
pub enum Placement {
    /// In /Applications or ~/Applications.
    Installed,
    /// Not inside an `.app` bundle, as in a development build.
    Unbundled,
    /// The `.app` bundle is somewhere it must be moved from.
    Elsewhere { bundle: PathBuf },
}

/// Where the app whose executable is `exe` stands, for a user whose home is
/// `home`.
pub fn placement(exe: &Path, home: &Path) -> Placement {
    let Some(bundle) = exe
        .ancestors()
        .find(|p| p.extension() == Some("app".as_ref()))
    else {
        return Placement::Unbundled;
    };
    let applications = [
        Path::new("/Applications").to_owned(),
        home.join("Applications"),
    ];
    if applications.iter().any(|dir| bundle.starts_with(dir)) {
        return Placement::Installed;
    }
    Placement::Elsewhere {
        bundle: bundle.to_owned(),
    }
}

/// Where to move `bundle`: /Applications when the user can write there,
/// otherwise their own Applications folder.
pub fn destination(bundle: &Path, applications_writable: bool, home: &Path) -> Option<PathBuf> {
    let dir = if applications_writable {
        PathBuf::from("/Applications")
    } else {
        home.join("Applications")
    };
    Some(dir.join(bundle.file_name()?))
}

/// The running app's bundle, when it must move before setup.
pub fn misplaced_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let home = dirs::home_dir()?;
    match placement(&exe, &home) {
        Placement::Elsewhere { bundle } => Some(bundle),
        Placement::Installed | Placement::Unbundled => None,
    }
}

/// Copies `bundle` into an Applications folder and opens the copy. The
/// original stays put: a translocated or disk-image copy can't be removed.
pub fn move_to_applications(bundle: &Path) -> anyhow::Result<()> {
    let home = dirs::home_dir().context("no home folder")?;
    let dest = destination(bundle, writable(Path::new("/Applications")), &home)
        .with_context(|| format!("no app name in \"{}\"", bundle.display()))?;
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    }
    // NOTE: a copy already there is an older install of this same app; ditto
    // would merge into it and leave stale files inside the bundle.
    if dest.exists() {
        fs::remove_dir_all(&dest)
            .with_context(|| format!("unable to replace \"{}\"", dest.display()))?;
    }
    run(process::Command::new("ditto").arg(bundle).arg(&dest))?;
    // ditto keeps the quarantine flag, and a quarantined copy is translocated
    // all over again. Gatekeeper already approved this launch. xattr exits
    // non-zero when the flag is absent, so its status says nothing.
    let _ = process::Command::new("xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(&dest)
        .status();
    run(process::Command::new("open").arg("-n").arg(&dest))
}

fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".mcp-relay-write-check-{}", process::id()));
    fs::File::create(&probe).is_ok() && fs::remove_file(&probe).is_ok()
}

fn run(command: &mut process::Command) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("unable to run {command:?}"))?;
    if !status.success() {
        bail!("{command:?} failed: {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/pat";
    const INNER: &str = "MCP Relay for Signal.app/Contents/MacOS/relay-app";

    fn at(dir: &str) -> Placement {
        placement(&Path::new(dir).join(INNER), Path::new(HOME))
    }

    /// Only the system and per-user Applications folders give Claude's config
    /// a path that survives; a translocated copy, Downloads, or the mounted
    /// disk image all disappear or move.
    #[test]
    fn only_an_applications_folder_counts_as_installed() {
        assert_eq!(at("/Applications"), Placement::Installed);
        assert_eq!(at("/Users/pat/Applications"), Placement::Installed);
        for dir in [
            "/private/var/folders/x1/T/AppTranslocation/5E1B/d",
            "/Users/pat/Downloads",
            "/Volumes/MCP Relay for Signal",
            "/Applications.old",
            "/Users/sam/Applications",
        ] {
            assert_eq!(
                at(dir),
                Placement::Elsewhere {
                    bundle: Path::new(dir).join("MCP Relay for Signal.app")
                },
                "in {dir}"
            );
        }
    }

    /// A development build runs a bare binary; asking to move it would block
    /// every `cargo run`.
    #[test]
    fn a_bare_binary_is_not_asked_to_move() {
        let exe = Path::new("/Users/pat/src/signal-agent/target/debug/relay-app");
        assert_eq!(placement(exe, Path::new(HOME)), Placement::Unbundled);
    }

    /// A standard user can't write /Applications, and setup must not ask for
    /// an admin password, so the move falls back to ~/Applications.
    #[test]
    fn the_move_targets_the_user_applications_folder_when_the_system_one_is_locked() {
        let bundle = Path::new("/Users/pat/Downloads/MCP Relay for Signal.app");
        assert_eq!(
            destination(bundle, true, Path::new(HOME)),
            Some(PathBuf::from("/Applications/MCP Relay for Signal.app"))
        );
        assert_eq!(
            destination(bundle, false, Path::new(HOME)),
            Some(PathBuf::from(
                "/Users/pat/Applications/MCP Relay for Signal.app"
            ))
        );
    }
}
