//! Starting the agents at login: one item per user on every OS (SMAppService
//! on macOS, a logon task on Windows, a systemd user unit on Linux) running
//! `signal-agent agent --all-profiles`, so uninstall has one item to remove.

#[cfg(any(target_os = "macos", test))]
mod launchd;
#[cfg(any(target_os = "linux", test))]
mod systemd;
#[cfg(any(windows, test))]
mod task;

use relay_core::ProfileName;

/// Makes sure `profile`'s agent starts at every login and runs now: the
/// supervisor's login item exists and is running, and any per-profile item
/// from before the supervisor is stopped and removed. The supervisor picks a
/// newly set-up profile up within seconds.
pub fn register(profile: &ProfileName) -> anyhow::Result<()> {
    tracing::info!("ensuring the login item that runs \"{profile}\" among all profiles");
    #[cfg(target_os = "linux")]
    return systemd::register(&agent_executable()?);
    #[cfg(windows)]
    return task::register(&agent_executable()?, &known_profiles()?);
    #[cfg(target_os = "macos")]
    return launchd::register(&data_dir()?);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    anyhow::bail!("no login registration on this OS")
}

/// Stops every profile's agent and removes the login item, including any
/// per-profile item from before the supervisor. Removing one that isn't
/// there succeeds.
pub fn unregister() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return systemd::unregister();
    #[cfg(windows)]
    return task::unregister(&known_profiles()?);
    #[cfg(target_os = "macos")]
    return launchd::unregister(&data_dir()?);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    anyhow::bail!("no login registration on this OS")
}

/// Every profile that may have had its own logon task before the supervisor.
#[cfg(windows)]
fn known_profiles() -> anyhow::Result<Vec<ProfileName>> {
    let mut profiles = relay_setup::Profiles::system()?.list()?;
    let default = ProfileName::default_profile();
    if !profiles.contains(&default) {
        profiles.push(default);
    }
    Ok(profiles)
}

/// The directory that holds every profile's data.
#[cfg(target_os = "macos")]
fn data_dir() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::Context;
    let root = relay_core::ProfilePaths::for_profile(&ProfileName::default_profile())?.root;
    root.parent()
        .map(ToOwned::to_owned)
        .with_context(|| format!("profile directory \"{}\" has no parent", root.display()))
}

/// The `signal-agent` binary: this process if it is the CLI, otherwise the one
/// shipped beside it (the desktop app keeps it next to its own executable).
#[cfg(any(target_os = "linux", windows))]
fn agent_executable() -> anyhow::Result<std::path::PathBuf> {
    use anyhow::{Context, bail};
    let me = std::env::current_exe().context("unable to locate this executable")?;
    let name = format!("signal-agent{}", std::env::consts::EXE_SUFFIX);
    if me.file_name().is_some_and(|n| n == name.as_str()) {
        return Ok(me);
    }
    let sibling = me.with_file_name(&name);
    if !sibling.is_file() {
        bail!("no agent binary at \"{}\"", sibling.display());
    }
    Ok(sibling)
}

/// Runs `program` and fails with its stderr unless it exits successfully.
#[cfg(any(target_os = "linux", windows))]
fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("unable to run {program}"))?;
    if !output.status.success() {
        bail!(
            "`{program} {}` failed ({}): {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
