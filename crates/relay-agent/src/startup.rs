//! Work the agent does once at startup, before serving, to repair what the
//! user may have undone since setup.

use relay_core::ProfileName;

/// Puts back any Claude client registrations for `profile` that went missing
/// (a Claude reinstall, a hand-edited config).
// Seam for lane L5: relay-setup will expose the re-registration; call it here
// and log its failures, since a missing Claude entry must not stop capture.
pub(crate) fn reregister_claude_entries(profile: &ProfileName) {
    tracing::debug!("Claude re-registration for \"{profile}\" is not wired yet (lane L5)");
}
