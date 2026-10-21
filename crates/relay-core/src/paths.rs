//! Where each profile's files live on each OS. Every crate resolves paths here,
//! so the agent, the shim, setup and uninstall can never disagree about them.

use std::path::PathBuf;

use anyhow::Context;

use crate::ProfileName;

/// The on-disk locations for one profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfilePaths {
    pub root: PathBuf,
    pub signal_store: PathBuf,
    pub message_store: PathBuf,
    pub agent_log: PathBuf,
}

impl ProfilePaths {
    /// The platform data directory for `profile`: Application Support on macOS,
    /// `%LOCALAPPDATA%` on Windows, `$XDG_DATA_HOME` on Linux.
    pub fn for_profile(profile: &ProfileName) -> anyhow::Result<Self> {
        let base = dirs::data_local_dir().context("no per-user data directory on this system")?;
        Ok(Self::under(base.join("signal-agent"), profile))
    }

    /// The same layout rooted at `base`, for tests and for relocating data.
    pub fn under(base: PathBuf, profile: &ProfileName) -> Self {
        let root = base.join(profile.as_ref());
        Self {
            signal_store: root.join("signal.db"),
            message_store: root.join("messages.db"),
            agent_log: root.join("agent.log"),
            root,
        }
    }

    /// The agent's local endpoint: a Unix socket in the profile directory, or a
    /// per-user named pipe on Windows.
    // NOTE: macOS limits Unix socket paths to 104 bytes. The profile directory
    // under Application Support stays well inside that for 32-character names,
    // but relocating `base` somewhere deep will break binding.
    pub fn agent_endpoint(&self, profile: &ProfileName) -> String {
        if cfg!(windows) {
            let user = std::env::var("USERNAME").unwrap_or_default();
            format!(r"\\.\pipe\signal-agent-{user}-{profile}")
        } else {
            self.root.join("agent.sock").display().to_string()
        }
    }
}
