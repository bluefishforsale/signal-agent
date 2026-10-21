//! Work the agent does once at startup, before serving, to repair what the
//! user may have undone since setup.

use relay_core::ProfileName;

/// Puts back any Claude client registrations for `profile` that went missing
/// (a Claude reinstall, a hand-edited config).
// Seam for integration once relay-setup's PR #38 lands: build
// `ServerEntry::for_profile(profile, &current_exe, ["mcp", "--profile", profile])`
// with the absolute path of the running binary, call
// `relay_setup::claude::Clients::system().reregister_missing(&entry)`, and log
// each `ClientOutcome`. Never fail startup on it.
// NOTE: `run` calls this only after the endpoint is bound. `claude mcp get`
// health-checks by launching our shim, and a shim that found nothing listening
// would spawn a second agent.
pub(crate) fn reregister_claude_entries(profile: &ProfileName) {
    tracing::debug!("Claude re-registration for \"{profile}\" is not wired yet (relay-setup #38)");
}
