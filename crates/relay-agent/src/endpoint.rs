//! The agent's local endpoint, reachable only by the user who runs it. Every
//! client (the stdio shim, setup, doctor) connects here, so this is the one
//! place that decides who may talk to the user's Signal account.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Listener, connect};
