//! Getting messages from before this device was linked into the store.
//!
//! Link-and-sync (the phone's history transfer at link time) is the primary
//! source. Signal Desktop import is an opt-in extra, never required.

mod desktop_key;
mod link_and_sync;

use std::path::Path;

use anyhow::bail;
use relay_store::MessageStore;

pub use link_and_sync::{ImportSummary, LinkAndSyncKey, import_link_and_sync};

/// How far an import has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImportProgress {
    pub imported: u64,
    pub total: Option<u64>,
}

/// Imports Signal Desktop's history from `signal_dir` into `store`, returning
/// how many messages were new.
pub fn import_signal_desktop(
    signal_dir: &Path,
    _store: &dyn MessageStore,
    _progress: &mut dyn FnMut(ImportProgress),
) -> anyhow::Result<u64> {
    bail!(
        "Signal Desktop import from {} is not implemented yet (lane L7)",
        signal_dir.display()
    )
}
