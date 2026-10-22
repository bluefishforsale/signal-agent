//! Where earlier messages come from on a real computer, and the profile's
//! message store they go into, shared by setup and the history commands.

use std::path::{Path, PathBuf};

use anyhow::Context;
use async_trait::async_trait;
use relay_core::{LinkAndSyncKey, prelude::*};
use relay_secrets::SecretStore;
use relay_store::SqlCipherStore;

use crate::driver::{DesktopHistory, HistoryImport};

/// A profile's message store, opened when an import needs it.
pub struct ProfileStore {
    profile: ProfileName,
    path: PathBuf,
    secrets: Box<dyn SecretStore>,
}

impl ProfileStore {
    pub fn new(profile: &ProfileName, paths: &ProfilePaths, secrets: Box<dyn SecretStore>) -> Self {
        Self {
            profile: profile.clone(),
            path: paths.message_store.clone(),
            secrets,
        }
    }

    /// Opens the store with the key setup saved for the profile.
    pub async fn open(&self) -> anyhow::Result<SqlCipherStore> {
        let key = self.secrets.get(&self.profile)?.with_context(|| {
            format!(
                "profile \"{}\" has no saved key; finish its setup first",
                self.profile
            )
        })?;
        SqlCipherStore::open(&self.path, &key).await
    }
}

/// The phone's link-and-sync archive, imported into a profile's store.
pub struct LinkAndSync {
    store: ProfileStore,
}

impl LinkAndSync {
    pub fn new(store: ProfileStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl HistoryImport for LinkAndSync {
    async fn import(
        &self,
        archive: &Path,
        key: &LinkAndSyncKey,
        our_aci: ServiceId,
        progress: &mut (dyn FnMut(u64, Option<u64>) + Send),
    ) -> anyhow::Result<()> {
        let bytes = tokio::fs::read(archive)
            .await
            .with_context(|| format!("unable to read \"{}\"", archive.display()))?;
        let store = self.store.open().await?;
        relay_history::import_link_and_sync(&bytes, key, our_aci, &store, &mut |p| {
            progress(p.imported, p.total)
        })
        .await?;
        Ok(())
    }
}

/// Signal Desktop's data on this computer, imported into a profile's store.
pub struct SignalDesktop {
    dir: Option<PathBuf>,
    store: ProfileStore,
}

impl SignalDesktop {
    /// Desktop's data in `dir` (`None` when this OS has no place for it),
    /// imported into `store`.
    pub fn new(dir: Option<PathBuf>, store: ProfileStore) -> Self {
        Self { dir, store }
    }

    /// Desktop's data where Desktop keeps it on this OS.
    pub fn usual_place(store: ProfileStore) -> Self {
        Self::new(relay_history::signal_desktop_dir(), store)
    }
}

#[async_trait]
impl DesktopHistory for SignalDesktop {
    fn found(&self) -> bool {
        self.dir
            .as_deref()
            .is_some_and(relay_history::is_signal_desktop)
    }

    async fn import(&self, progress: &mut (dyn FnMut(u64) + Send)) -> anyhow::Result<()> {
        let Some(dir) = self.dir.as_deref().filter(|_| self.found()) else {
            anyhow::bail!("Signal Desktop isn't on this computer");
        };
        let store = self.store.open().await?;
        relay_history::import_signal_desktop(dir, &store, &mut |p| progress(p.imported))
            .await
            .with_context(|| format!("importing Signal Desktop from \"{}\"", dir.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use relay_secrets::{MemorySecretStore, StoreKey};

    use super::*;

    /// The message store is encrypted with the key setup saved when it made
    /// the account. Opening it without that key must fail, not create a store
    /// under a fresh key the agent could never read.
    #[tokio::test]
    async fn the_message_store_opens_only_with_the_profiles_saved_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &profile);
        std::fs::create_dir_all(&paths.root).expect("profile dir");
        let store = ProfileStore::new(&profile, &paths, Box::new(MemorySecretStore::default()));
        let Err(e) = store.open().await else {
            panic!("opened a store with no saved key");
        };
        assert!(format!("{e:#}").contains("personal"), "{e:#}");
        assert!(!paths.message_store.exists(), "created a store anyway");

        let secrets = MemorySecretStore::default();
        secrets
            .set(&profile, &StoreKey::generate().expect("key"))
            .expect("save key");
        let store = ProfileStore::new(&profile, &paths, Box::new(secrets));
        store.open().await.expect("open with the saved key");
        assert!(paths.message_store.exists());
    }

    /// The window and the terminal hand the importer the downloaded file as
    /// it is; one that isn't an archive for this key and account must fail
    /// without writing anything.
    #[tokio::test]
    async fn a_file_that_is_not_the_phones_archive_fails_to_import() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &profile);
        std::fs::create_dir_all(&paths.root).expect("profile dir");
        let secrets = MemorySecretStore::default();
        secrets
            .set(&profile, &StoreKey::generate().expect("key"))
            .expect("save key");
        let archive = paths.root.join("history.archive");
        std::fs::write(&archive, b"not an archive").expect("archive");
        let history = LinkAndSync::new(ProfileStore::new(&profile, &paths, Box::new(secrets)));
        let aci = ServiceId::new(uuid::Uuid::from_u128(7));
        let Err(e) = history
            .import(&archive, &LinkAndSyncKey::new([7; 32]), aci, &mut |_, _| {})
            .await
        else {
            panic!("imported a file that isn't an archive");
        };
        assert!(format!("{e:#}").contains("authenticate"), "{e:#}");
    }

    /// No Desktop folder on this OS, or one without Desktop's data, must
    /// never lead to the offer.
    #[test]
    fn desktop_is_found_only_where_its_data_is() {
        let dir = tempfile::tempdir().expect("tempdir");
        let profile = ProfileName::default_profile();
        let paths = ProfilePaths::under(dir.path().to_path_buf(), &profile);
        let desktop = |at: Option<PathBuf>| {
            let store = ProfileStore::new(&profile, &paths, Box::new(MemorySecretStore::default()));
            SignalDesktop::new(at, store).found()
        };
        assert!(!desktop(None));
        assert!(!desktop(Some(dir.path().join("Signal"))));
        let signal = dir.path().join("Signal");
        std::fs::create_dir_all(signal.join("sql")).expect("sql");
        std::fs::write(signal.join("config.json"), "{}").expect("config");
        std::fs::write(signal.join("sql/db.sqlite"), "").expect("db");
        assert!(desktop(Some(signal)));
    }
}
