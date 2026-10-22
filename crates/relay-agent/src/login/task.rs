//! A per-user logon task in Task Scheduler. Registered from XML because only
//! a trigger scoped to the user's own SID can be created without admin rights.

use std::path::Path;

#[cfg(windows)]
use anyhow::Context;
use anyhow::bail;
#[cfg(windows)]
use relay_core::ProfileName;

/// The supervisor's task. Per-profile tasks from before it were
/// `signal-agent-<profile>`.
#[cfg(windows)]
const TASK_NAME: &str = "signal-agent";

#[cfg(windows)]
pub(super) fn register(exe: &Path, profiles: &[ProfileName]) -> anyhow::Result<()> {
    remove_legacy_tasks(profiles)?;
    let sid = crate::endpoint::current_user_sid()?;
    let xml = render_task(exe, &sid)?;
    let file = std::env::temp_dir().join("signal-agent-task.xml");
    std::fs::write(&file, utf16_file(&xml))
        .with_context(|| format!("unable to write \"{}\"", file.display()))?;
    let file_arg = file.display().to_string();
    let created = super::run(
        "schtasks",
        &["/Create", "/TN", TASK_NAME, "/XML", &file_arg, "/F"],
    );
    // The file is only input to schtasks; a leftover copy is harmless.
    let _ = std::fs::remove_file(&file);
    created?;
    super::run("schtasks", &["/Run", "/TN", TASK_NAME])
}

#[cfg(windows)]
pub(super) fn unregister(profiles: &[ProfileName]) -> anyhow::Result<()> {
    remove_legacy_tasks(profiles)?;
    remove_task(TASK_NAME)
}

/// Stops and removes each of `profiles`' own tasks, so the supervisor's
/// children are the only agents.
#[cfg(windows)]
fn remove_legacy_tasks(profiles: &[ProfileName]) -> anyhow::Result<()> {
    profiles
        .iter()
        .try_for_each(|p| remove_task(&format!("signal-agent-{p}")))
}

/// Ends and deletes task `name`. Removing one that isn't there succeeds.
#[cfg(windows)]
fn remove_task(name: &str) -> anyhow::Result<()> {
    if super::run("schtasks", &["/Query", "/TN", name]).is_err() {
        return Ok(());
    }
    // Not running is fine; the delete below is what must succeed.
    let _ = super::run("schtasks", &["/End", "/TN", name]);
    super::run("schtasks", &["/Delete", "/TN", name, "/F"])
}

/// Task Scheduler XML that starts `exe agent --all-profiles` when the user
/// with `sid` logs on.
pub(super) fn render_task(exe: &Path, sid: &str) -> anyhow::Result<String> {
    let Some(exe) = exe.to_str() else {
        bail!("the agent path {exe:?} is not valid Unicode");
    };
    let exe = xml_escape(exe);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>MCP Relay for Signal agent</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Hidden>true</Hidden>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>agent --all-profiles</Arguments></Exec></Actions>
</Task>
"#
    ))
}

/// `text` as UTF-16LE with a byte order mark.
fn utf16_file(text: &str) -> Vec<u8> {
    std::iter::once(0xFEFF)
        .chain(text.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1-2-3-1001";

    /// Task Scheduler's defaults would refuse to start the agent on battery,
    /// stop it when unplugged, and kill it after three days; capture must
    /// survive all three. An `&` in the install path must not break the XML.
    #[test]
    fn the_task_keeps_running_on_battery_and_forever_with_any_path() {
        let xml = render_task(Path::new(r"C:\Tom & Jerry\signal-agent.exe"), SID).expect("xml");
        for setting in [
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
        ] {
            assert!(xml.contains(setting), "missing {setting} in:\n{xml}");
        }
        assert!(
            xml.contains(r"C:\Tom &amp; Jerry\"),
            "unescaped path in:\n{xml}"
        );
    }

    /// schtasks reads the file as UTF-16 when it starts with a byte order
    /// mark, matching the XML's own encoding declaration.
    #[test]
    fn task_files_are_utf16_with_a_byte_order_mark() {
        let bytes = utf16_file("<Task>é</Task>");
        assert_eq!(bytes.get(..2), Some(&[0xFF, 0xFE][..]), "no BOM: {bytes:?}");
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(
            String::from_utf16(&units).expect("utf-16"),
            "<Task>é</Task>"
        );
    }

    /// The trigger and principal must both name this user's SID, or creating
    /// the task needs admin rights (a trigger for any user) or runs it as
    /// someone else.
    #[test]
    fn the_task_starts_the_supervisor_at_this_users_logon() {
        let xml = render_task(Path::new(r"C:\Users\a\signal-agent.exe"), SID).expect("xml");
        assert!(
            xml.contains(&format!(
                "<LogonTrigger><Enabled>true</Enabled><UserId>{SID}</UserId></LogonTrigger>"
            )),
            "trigger not scoped to the user:\n{xml}"
        );
        assert!(
            xml.contains(&format!("<UserId>{SID}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel>")),
            "principal wrong:\n{xml}"
        );
        assert!(
            xml.contains(r"<Command>C:\Users\a\signal-agent.exe</Command><Arguments>agent --all-profiles</Arguments>"),
            "wrong action:\n{xml}"
        );
    }
}
