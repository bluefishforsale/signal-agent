//! One systemd user unit running the supervisor, so every profile's agent runs
//! whenever the user's service manager does.

use std::path::Path;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::bail;

/// The supervisor's unit. Per-profile units from before it were
/// `signal-agent-<profile>.service`.
pub(super) const UNIT_NAME: &str = "signal-agent.service";

#[cfg(target_os = "linux")]
pub(super) fn register(exe: &Path) -> anyhow::Result<()> {
    let dir = unit_dir()?;
    remove_legacy_units(&dir)?;
    let path = dir.join(UNIT_NAME);
    let unit = render_unit(exe)?;
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    std::fs::write(&path, unit)
        .with_context(|| format!("unable to write \"{}\"", path.display()))?;
    super::run("systemctl", &["--user", "daemon-reload"])?;
    super::run("systemctl", &["--user", "enable", "--now", UNIT_NAME])?;
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
pub(super) fn unregister() -> anyhow::Result<()> {
    let dir = unit_dir()?;
    remove_legacy_units(&dir)?;
    let path = dir.join(UNIT_NAME);
    if !path.exists() {
        return Ok(());
    }
    super::run("systemctl", &["--user", "disable", "--now", UNIT_NAME])?;
    std::fs::remove_file(&path)
        .with_context(|| format!("unable to remove \"{}\"", path.display()))?;
    super::run("systemctl", &["--user", "daemon-reload"])
}

/// Stops and removes every per-profile unit, so the supervisor's children
/// are the only agents.
#[cfg(target_os = "linux")]
fn remove_legacy_units(dir: &Path) -> anyhow::Result<()> {
    let legacy = legacy_units(dir)?;
    for name in &legacy {
        super::run("systemctl", &["--user", "disable", "--now", name])?;
        let path = dir.join(name);
        std::fs::remove_file(&path)
            .with_context(|| format!("unable to remove \"{}\"", path.display()))?;
    }
    if !legacy.is_empty() {
        super::run("systemctl", &["--user", "daemon-reload"])?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unit_dir() -> anyhow::Result<std::path::PathBuf> {
    let config = dirs::config_dir().context("no per-user config directory")?;
    Ok(config.join("systemd/user"))
}

/// The per-profile unit files in `dir`, by name.
pub(super) fn legacy_units(dir: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => bail!("unable to list \"{}\": {e}", dir.display()),
    };
    let names = entries.filter_map(|e| e.ok()?.file_name().into_string().ok());
    Ok(names
        .filter(|n| n.starts_with("signal-agent-") && n.ends_with(".service"))
        .collect())
}

/// The unit file that runs `exe agent --all-profiles`.
pub(super) fn render_unit(exe: &Path) -> anyhow::Result<String> {
    let Some(exe) = exe.to_str() else {
        bail!("the agent path {exe:?} is not UTF-8, which a systemd unit can't hold");
    };
    if exe.contains(['"', '\\', '\n', '\r']) {
        bail!("the agent path {exe:?} contains a quote, backslash or newline; move the install");
    }
    let exe = exe.replace('%', "%%").replace('$', "$$");
    Ok(format!(
        "[Unit]\n\
         Description=MCP Relay for Signal agent\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\" agent --all-profiles\n\
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
        let unit = render_unit(Path::new("/home/a%b/$HOME/signal-agent")).expect("unit");
        assert!(
            unit.contains("ExecStart=\"/home/a%%b/$$HOME/signal-agent\""),
            "unescaped ExecStart in:\n{unit}"
        );
        for bad in ["/tmp/a\"b", "/tmp/a\nExecStartPre=/bin/evil", "/tmp/a\\b"] {
            assert!(render_unit(Path::new(bad)).is_err(), "accepted {bad:?}");
        }
    }

    /// The unit must run the supervisor, restart it if it dies, and be
    /// wanted by the user's default target so enabling it survives a reboot.
    #[test]
    fn the_unit_runs_the_supervisor_and_restarts_it() {
        let unit = render_unit(Path::new("/opt/signal agent/signal-agent")).expect("unit");
        assert!(
            unit.contains("ExecStart=\"/opt/signal agent/signal-agent\" agent --all-profiles\n"),
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

    /// Units from before the supervisor ran one profile each; left in place
    /// they'd fight the supervisor's children for each profile's endpoint.
    #[test]
    fn per_profile_units_are_found_for_migration_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        for file in [
            "signal-agent-personal.service",
            "signal-agent-bot.service",
            UNIT_NAME,
            "signal-agent-personal.service.d",
            "other.service",
        ] {
            std::fs::write(dir.path().join(file), "").expect("write");
        }
        let mut found = legacy_units(dir.path()).expect("scan");
        found.sort();
        assert_eq!(
            found,
            ["signal-agent-bot.service", "signal-agent-personal.service"]
        );
        let missing = dir.path().join("nope");
        assert!(legacy_units(&missing).expect("missing dir").is_empty());
    }
}
