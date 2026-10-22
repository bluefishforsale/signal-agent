//! Work the agent does once at startup to repair what the user may have
//! undone since setup.

use std::path::Path;

use relay_core::ProfileName;
use relay_setup::claude::{Clients, ServerEntry};

/// Puts back any Claude client registrations for `profile` that went missing
/// (a Claude reinstall, a hand-edited config). Failures are logged, never
/// returned: a missing Claude entry must not stop capture.
// NOTE: `run` calls this only after the endpoint is bound. `claude mcp get`
// health-checks by launching our shim, and a shim that found nothing listening
// would spawn a second agent.
pub(crate) fn reregister_claude_entries(profile: &ProfileName) {
    let entry = std::env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|exe| server_entry(profile, &exe));
    let entry = match entry {
        Ok(entry) => entry,
        Err(e) => return tracing::warn!("skipped Claude re-registration: {e:#}"),
    };
    for outcome in Clients::system().reregister_missing(&entry) {
        match outcome.result {
            Ok(()) => tracing::info!("{} has \"{}\"", outcome.client, entry.name),
            Err(e) => tracing::warn!("{} re-registration failed: {e}", outcome.client),
        }
    }
}

/// The entry Claude clients use to launch this profile's stdio shim.
fn server_entry(profile: &ProfileName, exe: &Path) -> anyhow::Result<ServerEntry> {
    ServerEntry::for_profile(profile, exe, ["mcp", "--profile", profile])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Claude must launch the shim for this profile by absolute path; uninstall
    /// recognizes our entries by that path, and Desktop has no shell PATH.
    #[test]
    fn the_entry_launches_this_profiles_shim_by_absolute_path() {
        let profile = ProfileName::new("work").expect("name");
        let exe = std::env::temp_dir().join("signal-agent");
        let entry = server_entry(&profile, &exe).expect("entry");
        assert_eq!(entry.name, "signal-work");
        assert_eq!(entry.command, exe.display().to_string());
        assert_eq!(entry.args, ["mcp", "--profile", "work"]);
    }
}
