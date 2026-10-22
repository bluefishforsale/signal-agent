//! macOS: the key as a generic password in the user's login Keychain, which
//! the OS unlocks with the user's login.

use anyhow::{Context, bail};
use relay_core::ProfileName;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

use crate::{SecretStore, StoreKey};

const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// One Keychain item per profile, under one service name.
pub struct KeychainStore {
    service: String,
}

impl KeychainStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

impl SecretStore for KeychainStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        let bytes = match get_generic_password(&self.service, profile) {
            Ok(bytes) => bytes,
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("unable to read the Keychain key for {profile}"));
            }
        };
        let Ok(bytes) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            bail!(
                "Keychain key for {profile} holds {} bytes, expected 32",
                bytes.len()
            );
        };
        Ok(Some(StoreKey::from_bytes(bytes)))
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        set_generic_password(&self.service, profile, key.as_bytes())
            .with_context(|| format!("unable to save the Keychain key for {profile}"))
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        match delete_generic_password(&self.service, profile) {
            Err(e) if e.code() != ERR_SEC_ITEM_NOT_FOUND => {
                Err(e).with_context(|| format!("unable to delete the Keychain key for {profile}"))
            }
            _ => Ok(()),
        }
    }
}
