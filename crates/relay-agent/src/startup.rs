//! Work the agent does once at startup to repair what the user may have
//! undone since setup.

use std::path::Path;

use relay_core::{ProfileName, ProfilePaths};
use relay_setup::{
    ClientOutcome,
    claude::{Clients, ServerEntry},
};

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
    let setup = ProfilePaths::for_profile(profile).and_then(|paths| Ok((entry?, paths)));
    let (entry, paths) = match setup {
        Ok(setup) => setup,
        Err(e) => return tracing::warn!("skipped Claude re-registration: {e:#}"),
    };
    for outcome in reregister_if_set_up(&paths, &Clients::system(), &entry) {
        match outcome.result {
            Ok(()) => tracing::info!("{} has \"{}\"", outcome.client, entry.name),
            Err(e) => tracing::warn!("{} re-registration failed: {e}", outcome.client),
        }
    }
}

/// Puts back `entry` in every client that lacks it, but only while the
/// profile is set up; a profile whose access choice is gone is being (or has
/// been) uninstalled, and re-adding its entries would undo that.
fn reregister_if_set_up(
    paths: &ProfilePaths,
    clients: &Clients,
    entry: &ServerEntry,
) -> Vec<ClientOutcome> {
    if let Err(e) = paths.load_access() {
        tracing::info!(
            "not re-adding \"{}\": the profile isn't set up ({e:#})",
            entry.name
        );
        return vec![];
    }
    clients.reregister_missing(entry)
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

    struct NoCli;

    impl relay_setup::claude::Runner for NoCli {
        fn run(
            &self,
            program: &Path,
            _: &[String],
        ) -> anyhow::Result<relay_setup::claude::RunOutput> {
            anyhow::bail!("unexpected run of {}", program.display())
        }
    }

    /// Uninstall removes the Claude entries, but a Claude client's health
    /// check can start an agent mid-uninstall, and that agent's startup used
    /// to put the entries straight back. Once uninstall has begun, an agent
    /// starting for the profile must leave them removed.
    #[test]
    fn an_agent_started_mid_uninstall_does_not_put_claude_entries_back() {
        use relay_core::{AccessMode, ProfilePaths};
        use relay_setup::{Footprint, Machine, Roots, claude::Platform};

        let home = tempfile::tempdir().expect("temp home");
        let support = home.path().join("Library/Application Support");
        let footprint = Footprint::new(
            Platform::MacOs,
            Roots {
                home: home.path().to_owned(),
                data_local: support.clone(),
                config: support.clone(),
            },
        );
        let desktop = footprint.desktop_dir().expect("Desktop dir");
        std::fs::create_dir_all(&desktop).expect("Desktop dir");
        let clients = Clients::new(
            Platform::MacOs,
            Box::new(NoCli),
            None,
            Some(desktop.clone()),
        );
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(footprint.data_dir(), &profile);
        let exe = home.path().join("signal-agent");
        let entry = server_entry(&profile, &exe).expect("entry");
        paths.save_access(AccessMode::Full).expect("setup finished");
        assert!(
            reregister_if_set_up(&paths, &clients, &entry)
                .iter()
                .all(|o| o.result.is_ok()),
            "a set-up profile must be registered"
        );

        let secrets = relay_secrets::MemorySecretStore::default();
        let machine = Machine {
            clients: &clients,
            secrets: &secrets,
            agent: &exe,
            remove_login_item: Box::new(|| Ok(())),
            // The health check's agent starts while uninstall stops agents,
            // after the entries are already gone.
            stop_agents: Box::new(|| {
                reregister_if_set_up(&paths, &clients, &entry);
                Ok(())
            }),
        };
        let steps =
            relay_setup::removal_steps(&footprint, std::slice::from_ref(&profile), &machine, false);
        let outcomes = relay_setup::run_steps(steps);
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:#?}");
        let config = std::fs::read_to_string(desktop.join("claude_desktop_config.json"))
            .expect("Desktop config");
        assert!(!config.contains(&entry.name), "entry came back: {config}");
    }
}
