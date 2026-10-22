//! Getting messages from before this device was linked into the store.
//!
//! Link-and-sync (the phone's history transfer at link time) is the primary
//! source. Signal Desktop import is an opt-in extra, never required.

mod desktop;
mod desktop_key;
mod link_and_sync;

pub use desktop::{import_signal_desktop, is_signal_desktop, signal_desktop_dir};
pub use link_and_sync::import_link_and_sync;
pub use relay_core::LinkAndSyncKey;

/// How far an import has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportProgress {
    pub imported: u64,
    pub total: Option<u64>,
}

/// What an import wrote and what it had to leave out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub imported: u64,
    pub skipped: u64,
    pub already_stored: u64,
}

/// Given and family name joined, or None if both are empty.
fn full_name(parts: [&str; 2]) -> Option<String> {
    let parts: Vec<_> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}
