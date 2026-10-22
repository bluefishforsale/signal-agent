//! The macOS login agent, registered through SMAppService from a plist inside
//! the app bundle. A bundle plist is fixed at build time, which is why it runs
//! the supervisor rather than any one profile.

use std::path::Path;

#[cfg(target_os = "macos")]
use anyhow::{Context, anyhow, bail};
#[cfg(target_os = "macos")]
use objc2::rc::Retained;
#[cfg(target_os = "macos")]
use objc2_foundation::NSString;
#[cfg(target_os = "macos")]
use objc2_service_management::{SMAppService, SMAppServiceStatus};

/// The plist's file name in `Contents/Library/LaunchAgents`.
pub(super) const PLIST_NAME: &str = "com.bluefishforsale.signalagent.agent.plist";

/// Written beside the profiles once the job runs the supervisor.
#[cfg(target_os = "macos")]
const MIGRATED_MARKER: &str = "login-item-supervisor";

#[cfg(target_os = "macos")]
pub(super) fn register(data_dir: &Path) -> anyhow::Result<()> {
    let service = service()?;
    let marker = data_dir.join(MIGRATED_MARKER);
    // NOTE: an install from before the supervisor has this same job running
    // `--profile personal`, and launchd keeps the definition it loaded until
    // the job is registered again. Re-register once, not on every call, since
    // each time restarts every agent.
    // SAFETY: SMAppService methods are callable from any thread on macOS 13+,
    // which the app's minimum system version guarantees.
    if unsafe { service.status() } == SMAppServiceStatus::Enabled && !marker.exists() {
        unregister_service(&service)?;
    }
    if unsafe { service.status() } != SMAppServiceStatus::Enabled {
        unsafe { service.registerAndReturnError() }.map_err(|e| {
            anyhow!(
                "SMAppService refused the login agent: {}",
                e.localizedDescription()
            )
        })?;
    }
    match unsafe { service.status() } {
        SMAppServiceStatus::Enabled => {}
        SMAppServiceStatus::RequiresApproval => bail!(
            "the agent needs approval: System Settings > General > Login Items, allow MCP Relay for Signal"
        ),
        other => bail!(
            "the login agent is registered but not enabled (status {})",
            other.0
        ),
    }
    std::fs::create_dir_all(data_dir)
        .and_then(|()| std::fs::write(&marker, b""))
        .with_context(|| format!("unable to write \"{}\"", marker.display()))
}

#[cfg(target_os = "macos")]
pub(super) fn is_registered() -> anyhow::Result<bool> {
    // SAFETY: as in `register`.
    Ok(unsafe { service()?.status() } == SMAppServiceStatus::Enabled)
}

#[cfg(target_os = "macos")]
pub(super) fn unregister(data_dir: &Path) -> anyhow::Result<()> {
    let service = service()?;
    unregister_service(&service)?;
    match std::fs::remove_file(data_dir.join(MIGRATED_MARKER)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).context("unable to remove the login item marker")
        }
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn unregister_service(service: &SMAppService) -> anyhow::Result<()> {
    // SAFETY: as in `register`.
    if unsafe { service.status() } == SMAppServiceStatus::NotRegistered {
        return Ok(());
    }
    unsafe { service.unregisterAndReturnError() }.map_err(|e| {
        anyhow!(
            "SMAppService refused to remove the login agent: {}",
            e.localizedDescription()
        )
    })
}

#[cfg(target_os = "macos")]
fn service() -> anyhow::Result<Retained<SMAppService>> {
    let exe = std::env::current_exe().context("unable to locate this executable")?;
    // NOTE: no launchctl fallback for dev builds. A raw LaunchAgent would
    // outlive the app, escape SMAppService's user approval, and leave a
    // second registration that uninstall doesn't know about.
    if app_bundle(&exe).is_none() {
        bail!(
            "\"{}\" is not inside an app bundle, so it can't register a login agent; \
             run `signal-agent agent --all-profiles` by hand for a dev build",
            exe.display()
        );
    }
    // SAFETY: as in `register`.
    Ok(unsafe { SMAppService::agentServiceWithPlistName(&NSString::from_str(PLIST_NAME)) })
}

/// The `.app` bundle `exe` runs from, if it is the bundle's own executable.
pub(super) fn app_bundle(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let in_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    in_bundle.then_some(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SMAppService finds the plist by PLIST_NAME and launchd runs what it
    /// says, so the shipped file must agree with this code on the name and
    /// the bundled binary, and run the supervisor.
    #[test]
    fn the_shipped_plist_matches_what_registration_expects() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("launchd")
            .join(PLIST_NAME);
        // Windows checkouts may convert line endings; the content is what matters.
        let plist = std::fs::read_to_string(&path)
            .expect("plist next to the crate")
            .replace("\r\n", "\n");
        let label = PLIST_NAME.trim_end_matches(".plist");
        for expected in [
            format!("<key>Label</key>\n\t<string>{label}</string>"),
            "<key>BundleProgram</key>\n\t<string>Contents/MacOS/signal-agent</string>".into(),
            "<string>agent</string>\n\t\t<string>--all-profiles</string>\n\t</array>".into(),
        ] {
            assert!(
                plist.contains(&expected),
                "{} lacks {expected:?}",
                path.display()
            );
        }
    }

    /// Registration only works from inside the app bundle; a dev build must be
    /// recognized as outside it so it fails clearly instead of half-working.
    #[test]
    fn only_executables_in_contents_macos_are_in_a_bundle() {
        let bundled =
            Path::new("/Applications/MCP Relay for Signal.app/Contents/MacOS/signal-agent");
        assert_eq!(
            app_bundle(bundled),
            Some(Path::new("/Applications/MCP Relay for Signal.app"))
        );
        for loose in [
            "/Users/a/signal-agent/target/debug/signal-agent",
            "/Applications/X.app/Contents/Resources/signal-agent",
            "/tmp/Contents/MacOS/signal-agent",
        ] {
            assert_eq!(
                app_bundle(Path::new(loose)),
                None,
                "{loose} taken as bundled"
            );
        }
    }
}
