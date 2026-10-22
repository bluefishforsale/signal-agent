//! A systemd user unit per profile, so the agent runs whenever the user's
//! service manager does.

use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::bail;
use relay_core::ProfileName;

#[cfg(target_os = "linux")]
pub(super) fn register(profile: &ProfileName, exe: &Path) -> anyhow::Result<()> {
    let path = unit_path(profile)?;
    let unit = render_unit(exe, profile)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    }
    std::fs::write(&path, unit)
        .with_context(|| format!("unable to write \"{}\"", path.display()))?;
    super::run("systemctl", &["--user", "daemon-reload"])?;
    super::run(
        "systemctl",
        &["--user", "enable", "--now", &unit_name(profile)],
    )?;
    // NOTE: without lingering, the user's service manager (and the agent with
    // it) stops when the last session ends, so a headless box reached over SSH
    // would stop capturing at logout. Polkit lets users set their own linger
    // by default; where it doesn't, capture still works while logged in.
    if let Err(e) = super::run("loginctl", &["enable-linger"]) {
        tracing::warn!("the agent will stop at logout: {e:#}");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn unregister(profile: &ProfileName) -> anyhow::Result<()> {
    let path = unit_path(profile)?;
    if !path.exists() {
        return Ok(());
    }
    super::run(
        "systemctl",
        &["--user", "disable", "--now", &unit_name(profile)],
    )?;
    std::fs::remove_file(&path)
        .with_context(|| format!("unable to remove \"{}\"", path.display()))?;
    super::run("systemctl", &["--user", "daemon-reload"])
}

#[cfg(target_os = "linux")]
fn unit_path(profile: &ProfileName) -> anyhow::Result<std::path::PathBuf> {
    let config = dirs::config_dir().context("no per-user config directory")?;
    Ok(config.join("systemd/user").join(unit_name(profile)))
}

#[cfg(target_os = "linux")]
fn unit_name(profile: &ProfileName) -> String {
    format!("signal-agent-{profile}.service")
}

/// The unit file that runs `exe agent --profile <profile>`.
pub(super) fn render_unit(exe: &Path, profile: &ProfileName) -> anyhow::Result<String> {
    let Some(exe) = exe.to_str() else {
        bail!("the agent path {exe:?} is not UTF-8, which a systemd unit can't hold");
    };
    if exe.contains(['"', '\\', '\n', '\r']) {
        bail!("the agent path {exe:?} contains a quote, backslash or newline; move the install");
    }
    let exe = exe.replace('%', "%%").replace('$', "$$");
    Ok(format!(
        "[Unit]\n\
         Description=MCP Relay for Signal agent ({profile})\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\" agent --profile {profile}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// systemd expands `%` specifiers and `$` variables inside ExecStart, so an
    /// install path containing either must be escaped or the unit would run a
    /// different program; characters that can't be escaped must be refused.
    #[test]
    fn exec_paths_are_escaped_or_refused() {
        let profile = ProfileName::default_profile();
        let unit = render_unit(Path::new("/home/a%b/$HOME/signal-agent"), &profile).expect("unit");
        assert!(
            unit.contains("ExecStart=\"/home/a%%b/$$HOME/signal-agent\""),
            "unescaped ExecStart in:\n{unit}"
        );
        for bad in ["/tmp/a\"b", "/tmp/a\nExecStartPre=/bin/evil", "/tmp/a\\b"] {
            assert!(
                render_unit(Path::new(bad), &profile).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    /// The unit must run this profile's agent, restart it if it dies, and be
    /// wanted by the user's default target so enabling it survives a reboot.
    #[test]
    fn the_unit_runs_this_profiles_agent_and_restarts_it() {
        let profile = ProfileName::new("work").expect("name");
        let unit =
            render_unit(Path::new("/opt/signal agent/signal-agent"), &profile).expect("unit");
        assert!(
            unit.contains("ExecStart=\"/opt/signal agent/signal-agent\" agent --profile work\n"),
            "wrong ExecStart in:\n{unit}"
        );
        assert!(
            unit.contains("Restart=on-failure\n"),
            "no restart in:\n{unit}"
        );
        assert!(
            unit.contains("WantedBy=default.target\n"),
            "not enable-able:\n{unit}"
        );
    }
}
