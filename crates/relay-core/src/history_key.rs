//! The key a phone hands a newly linked device for its history archive. It
//! crosses from the Signal seam to the importer, so it lives here, and it is
//! the one secret that type must keep out of logs and memory dumps.

use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

/// The 32-byte key the phone sends in the provisioning message when it offers
/// to transfer history. Single-use; never persist it.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct LinkAndSyncKey([u8; 32]);

impl LinkAndSyncKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for LinkAndSyncKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkAndSyncKey(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key must never reach a log through Debug, where a tracing call
    /// that formats an outcome holding it would otherwise print it.
    #[test]
    fn debug_never_shows_the_key() {
        let shown = format!("{:?}", LinkAndSyncKey::new([0xab; 32]));
        assert!(!shown.contains("ab") && !shown.contains("171"), "{shown}");
    }

    /// The key must be wiped when dropped, so it doesn't linger in freed
    /// memory after the import.
    #[test]
    fn the_key_is_zeroized_on_drop() {
        fn zeroized_on_drop<T: ZeroizeOnDrop>() {}
        zeroized_on_drop::<LinkAndSyncKey>();
    }
}
