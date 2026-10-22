//! Linux desktops: the key in the Secret Service (GNOME Keyring, KWallet),
//! which the session unlocks with the user's login.

use std::collections::HashMap;

use anyhow::{Context, bail};
use relay_core::ProfileName;
use secret_service::{EncryptionType, blocking::SecretService};

use crate::{SecretStore, StoreKey};

/// One item per profile in the default collection, found by attributes.
pub struct SecretServiceStore {
    application: String,
}

impl SecretServiceStore {
    /// A store tagged with `application`, if this session has a Secret
    /// Service with a default collection; the error says why not.
    pub fn connect(application: impl Into<String>) -> anyhow::Result<Self> {
        let ss = SecretService::connect(EncryptionType::Dh)
            .context("no Secret Service on the session bus")?;
        ss.get_default_collection()
            .context("the Secret Service has no default collection")?;
        Ok(Self {
            application: application.into(),
        })
    }

    fn attributes<'a>(&'a self, profile: &'a ProfileName) -> HashMap<&'a str, &'a str> {
        HashMap::from([
            ("application", self.application.as_str()),
            ("profile", profile.as_ref()),
        ])
    }
}

impl SecretStore for SecretServiceStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let collection = ss.get_default_collection()?;
        collection
            .ensure_unlocked()
            .context("the Secret Service collection is locked")?;
        let items = collection.search_items(self.attributes(profile))?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        item.ensure_unlocked()?;
        let bytes = item
            .get_secret()
            .with_context(|| format!("unable to read the Secret Service key for {profile}"))?;
        let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            bail!(
                "Secret Service key for {profile} holds {} bytes, expected 32",
                bytes.len()
            );
        };
        Ok(Some(StoreKey::from_bytes(bytes)))
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let collection = ss.get_default_collection()?;
        collection
            .ensure_unlocked()
            .context("the Secret Service collection is locked")?;
        collection
            .create_item(
                &format!("signal-agent store key ({profile})"),
                self.attributes(profile),
                key.as_bytes(),
                true,
                "application/octet-stream",
            )
            .with_context(|| format!("unable to save the Secret Service key for {profile}"))?;
        Ok(())
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        let ss = SecretService::connect(EncryptionType::Dh)?;
        let collection = ss.get_default_collection()?;
        collection
            .ensure_unlocked()
            .context("the Secret Service collection is locked")?;
        for item in collection.search_items(self.attributes(profile))? {
            item.delete().with_context(|| {
                format!("unable to delete the Secret Service key for {profile}")
            })?;
        }
        Ok(())
    }
}
