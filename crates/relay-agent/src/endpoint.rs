//! The agent's local endpoint, reachable only by the user who runs it. Every
//! client (the stdio shim, setup, doctor) connects here, so this is the one
//! place that decides who may talk to the user's Signal account.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Listener, connect};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Listener, connect};

use relay_core::{ProfileName, ProfilePaths};

/// The endpoint name the agent for `profile` listens on and clients connect to.
pub fn name(profile: &ProfileName, paths: &ProfilePaths) -> anyhow::Result<String> {
    #[cfg(windows)]
    let sid = Some(windows::current_user_sid()?);
    #[cfg(not(windows))]
    let sid: Option<String> = None;
    paths.agent_endpoint(profile, sid.as_deref())
}
