//! Moves a key out of the file fallback once a better store appears. A
//! headless Linux box that later gains a Secret Service would otherwise look
//! there, find nothing, and leave the profile's history unreadable.

use relay_core::ProfileName;

use crate::{SecretStore, StoreKey};

/// Reads `primary` first, then `fallback`, moving any key found in the
/// fallback into the primary.
pub(crate) struct MigratingStore {
    primary: Box<dyn SecretStore>,
    fallback: Box<dyn SecretStore>,
}

impl MigratingStore {
    pub(crate) fn new(primary: Box<dyn SecretStore>, fallback: Box<dyn SecretStore>) -> Self {
        Self { primary, fallback }
    }

    /// Copies `key` into the primary and removes the fallback copy only once
    /// the primary reads it back intact.
    fn migrate(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        self.primary.set(profile, key)?;
        if self.primary.get(profile)?.as_ref() != Some(key) {
            anyhow::bail!("the key for {profile} did not read back from the new store");
        }
        self.fallback.delete(profile)
    }
}

impl SecretStore for MigratingStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        if let Some(key) = self.primary.get(profile)? {
            return Ok(Some(key));
        }
        let Some(key) = self.fallback.get(profile)? else {
            return Ok(None);
        };
        // NOTE: a failed migration must still return the key; the file stays
        // and the move is retried on the next read.
        let _ = self.migrate(profile, &key);
        Ok(Some(key))
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        self.migrate(profile, key)
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        self.primary.delete(profile)?;
        self.fallback.delete(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KeyFileStore, MemorySecretStore};

    fn profile() -> ProfileName {
        ProfileName::new("personal").expect("valid name")
    }

    /// A keyring that is reachable but refuses writes, e.g. a locked
    /// collection whose unlock prompt was dismissed.
    struct ReadOnlyKeyring;

    impl SecretStore for ReadOnlyKeyring {
        fn get(&self, _: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
            Ok(None)
        }
        fn set(&self, _: &ProfileName, _: &StoreKey) -> anyhow::Result<()> {
            anyhow::bail!("collection is locked")
        }
        fn delete(&self, _: &ProfileName) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A profile keyed by the file fallback still opens after a Secret
    /// Service appears, and the key ends up in the keyring, not the file.
    #[test]
    fn a_profile_keyed_by_the_file_fallback_still_opens_after_a_secret_service_appears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = StoreKey::from_bytes([4; 32]);
        KeyFileStore::under(dir.path().to_owned())
            .set(&profile(), &key)
            .expect("set file key");
        let store = MigratingStore::new(
            Box::new(MemorySecretStore::default()),
            Box::new(KeyFileStore::under(dir.path().to_owned())),
        );
        assert_eq!(store.get(&profile()).expect("get"), Some(key.clone()));
        let file = KeyFileStore::under(dir.path().to_owned());
        assert_eq!(file.get(&profile()).expect("get"), None, "file key kept");
        assert_eq!(store.primary.get(&profile()).expect("get"), Some(key));
    }

    /// If the keyring won't take the key, the file copy must survive and the
    /// key still be returned, or a failed move would lose the history.
    #[test]
    fn a_failed_move_into_the_keyring_keeps_the_file_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key = StoreKey::from_bytes([6; 32]);
        let file = KeyFileStore::under(dir.path().to_owned());
        file.set(&profile(), &key).expect("set file key");
        let store = MigratingStore::new(
            Box::new(ReadOnlyKeyring),
            Box::new(KeyFileStore::under(dir.path().to_owned())),
        );
        assert_eq!(store.get(&profile()).expect("get"), Some(key.clone()));
        assert_eq!(
            file.get(&profile()).expect("get"),
            Some(key),
            "file key lost"
        );
    }
}
