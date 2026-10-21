//! A secret store that forgets everything on exit, for tests.

use std::{collections::HashMap, sync::Mutex};

use relay_core::ProfileName;

use crate::{SecretStore, StoreKey};

#[derive(Default)]
pub struct MemorySecretStore {
    keys: Mutex<HashMap<ProfileName, StoreKey>>,
}

impl SecretStore for MemorySecretStore {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        Ok(self.keys.lock().expect("secret map").get(profile).cloned())
    }

    fn set(&self, profile: &ProfileName, key: &StoreKey) -> anyhow::Result<()> {
        self.keys
            .lock()
            .expect("secret map")
            .insert(profile.clone(), key.clone());
        Ok(())
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        self.keys.lock().expect("secret map").remove(profile);
        Ok(())
    }
}
