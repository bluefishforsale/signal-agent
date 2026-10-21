//! Where each profile's database key lives, so the encrypted stores are
//! useless to anything that copies the files without the user's OS login.

mod memory;

use std::fmt;

use anyhow::bail;
use relay_core::ProfileName;

pub use memory::MemorySecretStore;

/// The 32-byte key both of a profile's SQLCipher databases are encrypted with.
#[derive(Clone, PartialEq, Eq)]
pub struct StoreKey([u8; 32]);

impl StoreKey {
    pub fn generate() -> anyhow::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

// NOTE: never derive Debug or Display here. A key that formats as its bytes ends
// up in a log line or an error message sooner or later.
impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey(<redacted>)")
    }
}

/// An OS secret store holding one key per profile.
pub trait SecretStore: Send + Sync {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>>;
    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()>;
    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()>;
    /// A sentence for setup to show when this store is weaker than the OS
    /// keychain, e.g. the headless Linux key-file fallback.
    fn caveat(&self) -> Option<&'static str> {
        None
    }
}

/// The best secret store this machine offers: Keychain, DPAPI, Secret Service,
/// or the 0600 key file on headless Linux.
pub fn platform_store() -> anyhow::Result<Box<dyn SecretStore>> {
    bail!("platform secret stores are not implemented yet (lane L2)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key must never print its bytes, whatever wraps it.
    #[test]
    fn store_key_debug_never_reveals_bytes() {
        let key = StoreKey::from_bytes([0xab; 32]);
        let printed = format!("{key:?} {:?}", Some(&key));
        assert!(!printed.contains("ab"), "key bytes leaked: {printed}");
    }

    /// Generated keys must differ, or every profile would share one key.
    #[test]
    fn generated_keys_are_distinct() {
        let a = StoreKey::generate().expect("randomness");
        let b = StoreKey::generate().expect("randomness");
        assert_ne!(a, b, "two generated keys were identical");
    }
}
