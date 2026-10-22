//! Starting the agent at login: SMAppService on macOS, a logon task on
//! Windows, a systemd user unit on Linux.

#[cfg(any(target_os = "macos", test))]
mod launchd;
#[cfg(any(target_os = "linux", test))]
mod systemd;
#[cfg(any(windows, test))]
mod task;

use relay_core::ProfileName;

/// Makes the agent for `profile` start at every login, and starts it now.
pub fn register(profile: &ProfileName) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return systemd::register(profile, &agent_executable()?);
    #[cfg(windows)]
    return task::register(profile, &agent_executable()?);
    #[cfg(target_os = "macos")]
    return launchd::register(profile);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    anyhow::bail!("no login registration for \"{profile}\" on this OS")
}

/// Stops the agent for `profile` and removes its login registration. Removing
/// one that isn't there succeeds.
pub fn unregister(profile: &ProfileName) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    return systemd::unregister(profile);
    #[cfg(windows)]
    return task::unregister(profile);
    #[cfg(target_os = "macos")]
    return launchd::unregister(profile);
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    anyhow::bail!("no login registration for \"{profile}\" on this OS")
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
