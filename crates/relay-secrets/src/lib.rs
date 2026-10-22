//! Where each profile's database key lives, so the encrypted stores are
//! useless to anything that copies the files without the user's OS login.

#[cfg(windows)]
mod dpapi;
#[cfg(unix)]
mod key_file;
#[cfg(target_os = "macos")]
mod keychain;
mod memory;
// Only Linux can gain a keyring after falling back to the file; the unit
// tests run wherever the file store exists.
#[cfg(all(unix, any(target_os = "linux", test)))]
mod migrating;
#[cfg(target_os = "linux")]
mod secret_service;

use std::{fmt, path::PathBuf};

use relay_core::{ProfileName, ProfilePaths};

#[cfg(windows)]
pub use dpapi::DpapiFileStore;
#[cfg(unix)]
pub use key_file::KeyFileStore;
#[cfg(target_os = "macos")]
pub use keychain::KeychainStore;
pub use memory::MemorySecretStore;
#[cfg(target_os = "linux")]
pub use secret_service::SecretServiceStore;

/// The name keys are filed under in OS keyrings.
#[cfg(any(target_os = "macos", target_os = "linux"))]
const SERVICE: &str = "signal-agent";

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
    #[cfg(target_os = "macos")]
    return Ok(Box::new(KeychainStore::new(SERVICE)));
    #[cfg(windows)]
    return Ok(Box::new(DpapiFileStore::default()));
    #[cfg(target_os = "linux")]
    return Ok(match SecretServiceStore::connect(SERVICE) {
        Ok(store) => Box::new(migrating::MigratingStore::new(
            Box::new(store),
            Box::new(KeyFileStore::default()),
        )),
        Err(_) => Box::new(KeyFileStore::default()),
    });
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Ok(Box::new(KeyFileStore::default()));
}

/// Where a file-backed store keeps `profile`'s key: its platform data
/// directory, or under `base` when one is given.
fn key_path(
    base: Option<&PathBuf>,
    profile: &ProfileName,
    file_name: &str,
) -> anyhow::Result<PathBuf> {
    let paths = match base {
        Some(base) => ProfilePaths::under(base.clone(), profile),
        None => ProfilePaths::for_profile(profile)?,
    };
    Ok(paths.root.join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every store this OS can test, each with whatever it needs kept alive.
    fn stores() -> Vec<(
        &'static str,
        Box<dyn SecretStore>,
        Option<tempfile::TempDir>,
    )> {
        let mut out: Vec<(
            &'static str,
            Box<dyn SecretStore>,
            Option<tempfile::TempDir>,
        )> = vec![("memory", Box::new(MemorySecretStore::default()), None)];
        #[cfg(unix)]
        {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = KeyFileStore::under(dir.path().to_owned());
            out.push(("key file", Box::new(store), Some(dir)));
        }
        #[cfg(windows)]
        {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = DpapiFileStore::under(dir.path().to_owned());
            out.push(("dpapi file", Box::new(store), Some(dir)));
        }
        // NOTE: the OS keyring is the developer's own login keychain, so a plain
        // `cargo test` on a laptop must not write to it; CI opts in.
        if std::env::var_os("SIGNAL_AGENT_TEST_OS_KEYRING").is_none() {
            eprintln!("skipping the OS keyring store: set SIGNAL_AGENT_TEST_OS_KEYRING=1");
            return out;
        }
        // CI's Linux runner has no D-Bus session, so this store is skipped
        // there with the reason printed.
        #[cfg(target_os = "linux")]
        match SecretServiceStore::connect("signal-agent-test") {
            Ok(store) => out.push(("secret service", Box::new(store), None)),
            Err(e) => eprintln!("skipping the Secret Service store: {e:#}"),
        }
        #[cfg(target_os = "macos")]
        out.push((
            "keychain",
            Box::new(KeychainStore::new("signal-agent-test")),
            None,
        ));
        out
    }

    /// A unique profile per run, so tests against a real OS keyring never
    /// touch a real profile's key or collide with a parallel run.
    fn test_profile() -> ProfileName {
        let mut bytes = [0u8; 4];
        getrandom::fill(&mut bytes).expect("randomness");
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        ProfileName::new(format!("test-{suffix}")).expect("valid name")
    }

    /// Every store must hand back exactly the key it was given, per profile,
    /// report a missing key as None, and forget a key once deleted, or a
    /// profile's database becomes unreadable.
    #[test]
    fn every_store_round_trips_keys_per_profile() {
        for (name, store, _guard) in stores() {
            let (a, b) = (test_profile(), test_profile());
            let (key_a, key_b) = (StoreKey::from_bytes([1; 32]), StoreKey::from_bytes([2; 32]));
            assert_eq!(store.get(&a).expect("get"), None, "{name}: phantom key");
            store.set(&a, &StoreKey::from_bytes([9; 32])).expect("set");
            store.set(&a, &key_a).expect("overwrite");
            store.set(&b, &key_b).expect("set");
            assert_eq!(
                store.get(&a).expect("get"),
                Some(key_a),
                "{name}: profile a"
            );
            assert_eq!(
                store.get(&b).expect("get"),
                Some(key_b.clone()),
                "{name}: profile b"
            );
            store.delete(&a).expect("delete");
            store.delete(&a).expect("deleting twice is fine");
            assert_eq!(store.get(&a).expect("get"), None, "{name}: deleted key");
            assert_eq!(
                store.get(&b).expect("get"),
                Some(key_b),
                "{name}: other profile"
            );
            store.delete(&b).expect("cleanup");
        }
    }

    #[cfg(target_os = "linux")]
    fn expects_file_fallback() -> bool {
        SecretServiceStore::connect(SERVICE).is_err()
    }

    #[cfg(not(target_os = "linux"))]
    fn expects_file_fallback() -> bool {
        false
    }

    /// Setup warns about key storage only when it really fell back to a
    /// plain file; a native keyring must not trigger the warning.
    #[test]
    fn platform_store_warns_only_for_the_file_fallback() {
        let store = platform_store().expect("platform store");
        let fell_back = expects_file_fallback();
        assert_eq!(
            store.caveat().is_some(),
            fell_back,
            "caveat {:?} on a store that fell back: {fell_back}",
            store.caveat()
        );
    }

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
