//! The headless Linux fallback: the key in a file only the user can read,
//! for machines with no Secret Service to hold it.

use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

use anyhow::{Context, bail};
use relay_core::ProfileName;

use crate::{SecretStore, StoreKey, key_path};

/// Each profile's key in `store.key`, mode 0600, in its profile directory.
/// The default keeps keys in each profile's platform data directory.
#[derive(Default)]
pub struct KeyFileStore {
    base: Option<PathBuf>,
}

impl KeyFileStore {
    /// Keys under `base` instead, laid out as `ProfilePaths::under` does.
    pub fn under(base: PathBuf) -> Self {
        Self { base: Some(base) }
    }

    fn path(&self, profile: &ProfileName) -> anyhow::Result<PathBuf> {
        key_path(self.base.as_ref(), profile, "store.key")
    }
}

impl SecretStore for KeyFileStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        let path = self.path(profile)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("unable to read \"{}\"", path.display()));
            }
        };
        let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            bail!(
                "key file \"{}\" holds {} bytes, expected 32",
                path.display(),
                bytes.len()
            );
        };
        Ok(Some(StoreKey::from_bytes(bytes)))
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        let path = self.path(profile)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("unable to write \"{}\"", path.display()))?;
        // NOTE: `mode` only applies when the file is created; a pre-existing
        // file keeps whatever permissions it had, so tighten them explicitly.
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("unable to restrict \"{}\"", path.display()))?;
        file.write_all(key.as_bytes())
            .with_context(|| format!("unable to write \"{}\"", path.display()))?;
        Ok(())
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        let path = self.path(profile)?;
        match fs::remove_file(&path) {
            Err(e) if e.kind() != ErrorKind::NotFound => {
                Err(e).with_context(|| format!("unable to delete \"{}\"", path.display()))
            }
            _ => Ok(()),
        }
    }

    fn caveat(&self) -> Option<&'static str> {
        Some(
            "No system keyring was found, so the key that encrypts your Signal history \
             is saved in a file only your user account can read; anyone who can log in \
             as you or read your disk can decrypt it.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> ProfileName {
        ProfileName::new("work").expect("valid name")
    }

    /// The key file must be readable by its owner only, even when it replaces
    /// a file that was world-readable.
    #[test]
    fn key_file_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = KeyFileStore::under(dir.path().to_owned());
        let path = store.path(&profile()).expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, b"old").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        store
            .set(&profile(), &StoreKey::from_bytes([5; 32]))
            .expect("set");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "key file mode {mode:o}");
    }

    /// A truncated or foreign file must be an error, not a wrong key that
    /// silently fails to open the database later.
    #[test]
    fn a_key_file_of_the_wrong_length_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = KeyFileStore::under(dir.path().to_owned());
        let path = store.path(&profile()).expect("path");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, [1u8; 31]).expect("write");
        assert!(store.get(&profile()).is_err(), "accepted a 31-byte key");
    }
}
