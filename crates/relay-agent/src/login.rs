//! Starting the agent at login: SMAppService on macOS, a logon task on
//! Windows, a systemd user unit on Linux.

use anyhow::bail;
use relay_core::ProfileName;

pub fn register(profile: &ProfileName) -> anyhow::Result<()> {
    bail!("login registration for \"{profile}\" is not implemented yet (lane L4)")
}

pub fn unregister(profile: &ProfileName) -> anyhow::Result<()> {
    bail!("login unregistration for \"{profile}\" is not implemented yet (lane L4)")
}
