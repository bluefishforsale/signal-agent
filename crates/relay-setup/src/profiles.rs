//! Named profiles: which accounts this user has set up, so setup, the CLI and
//! uninstall agree on the list, and each account's files, key, agent and
//! Claude entry stay apart from every other's.

use std::{fs, io, path::PathBuf};

use anyhow::{Context, bail};
use relay_core::{ProfileName, ProfilePaths};
use relay_secrets::{SecretStore, StoreKey};
use serde::{Deserialize, Serialize};

/// The registry file's contents.
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    profiles: Vec<ProfileName>,
}

/// The profiles under one data directory, remembered in a small registry file.
pub struct Profiles {
    base: PathBuf,
}

impl Profiles {
    /// The same registry rooted at `base`, for tests and for relocating data.
    pub fn under(base: PathBuf) -> Self {
        Self { base }
    }

    /// Every profile, in the order they were created.
    pub fn list(&self) -> anyhow::Result<Vec<ProfileName>> {
        Ok(self.load()?.profiles)
    }

    /// The profile a first install uses, created without asking if there are
    /// none yet.
    pub fn ensure_default(&self, secrets: &dyn SecretStore) -> anyhow::Result<ProfileName> {
        let registry = self.load()?;
        if let Some(first) = registry.profiles.first() {
            return Ok(first.clone());
        }
        self.add(registry, ProfileName::default_profile(), secrets)
    }

    /// Adds the profile `name`, which must be a valid name not already in use.
    pub fn create(&self, name: &str, secrets: &dyn SecretStore) -> anyhow::Result<ProfileName> {
        let profile = ProfileName::new(name)?;
        let registry = self.load()?;
        if registry.profiles.contains(&profile) {
            bail!("a profile called \"{profile}\" already exists");
        }
        self.add(registry, profile, secrets)
    }

    /// Where `profile`'s files live.
    pub fn paths(&self, profile: &ProfileName) -> ProfilePaths {
        ProfilePaths::under(self.base.clone(), profile)
    }

    // NOTE: a key already in the secret store is kept, not replaced. It may be
    // the only way to open data a previous install of this profile left
    // behind, and overwriting it would make that data unreadable for good.
    fn add(
        &self,
        mut registry: Registry,
        profile: ProfileName,
        secrets: &dyn SecretStore,
    ) -> anyhow::Result<ProfileName> {
        if secrets.get(&profile)?.is_none() {
            secrets.set(&profile, &StoreKey::generate()?)?;
        }
        registry.profiles.push(profile.clone());
        self.save(&registry)?;
        Ok(profile)
    }

    fn file(&self) -> PathBuf {
        self.base.join("profiles.json")
    }

    fn load(&self) -> anyhow::Result<Registry> {
        let file = self.file();
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("unable to read \"{}\"", file.display()));
            }
        };
        serde_json::from_str(&text)
            .with_context(|| format!("\"{}\" is not a valid profile list", file.display()))
    }

    // NOTE: written to a temporary file and renamed into place, so a crash
    // mid-write leaves the old list rather than an empty one that would make
    // the next launch create `personal` over an existing setup.
    fn save(&self, registry: &Registry) -> anyhow::Result<()> {
        fs::create_dir_all(&self.base)
            .with_context(|| format!("unable to create \"{}\"", self.base.display()))?;
        let file = self.file();
        let partial = file.with_extension("json.partial");
        let text = serde_json::to_string_pretty(registry).context("unable to encode profiles")?;
        fs::write(&partial, text)
            .with_context(|| format!("unable to write \"{}\"", partial.display()))?;
        fs::rename(&partial, &file)
            .with_context(|| format!("unable to replace \"{}\"", file.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use relay_secrets::MemorySecretStore;

    use super::*;

    fn name(s: &str) -> ProfileName {
        ProfileName::new(s).expect("valid profile name")
    }

    /// A first install makes `personal` without mentioning profiles, and the
    /// next launch must find it rather than make another.
    #[test]
    fn the_first_install_creates_personal_and_remembers_it() {
        let base = tempfile::tempdir().expect("temp data dir");
        let secrets = MemorySecretStore::default();
        let created = Profiles::under(base.path().into())
            .ensure_default(&secrets)
            .expect("default profile");
        assert_eq!(created, name("personal"));

        let reopened = Profiles::under(base.path().into());
        assert_eq!(reopened.list().expect("list"), [name("personal")]);
        assert_eq!(
            reopened.ensure_default(&secrets).expect("default"),
            name("personal")
        );
        assert_eq!(reopened.list().expect("list"), [name("personal")]);
    }

    /// "Add another account" takes a typed name, so it must be validated
    /// before it reaches a path, and a second profile with the same name would
    /// share the first one's store, key and agent.
    #[test]
    fn creating_a_profile_refuses_invalid_and_duplicate_names() {
        let base = tempfile::tempdir().expect("temp data dir");
        let secrets = MemorySecretStore::default();
        let profiles = Profiles::under(base.path().into());
        profiles.ensure_default(&secrets).expect("default profile");

        let bot = profiles.create("bot", &secrets).expect("create bot");
        assert_eq!(bot, name("bot"));
        for bad in ["bot", "personal", "Work Phone", "../etc", ""] {
            let refused = profiles.create(bad, &secrets);
            assert!(refused.is_err(), "created {bad:?}: {refused:?}");
        }
        assert_eq!(
            profiles.list().expect("list"),
            [name("personal"), name("bot")]
        );
    }

    /// Two accounts on one machine must never touch each other's files, key,
    /// agent socket or Claude entry; any overlap would mix two Signal
    /// identities in one store or route one profile's tools to the other.
    #[test]
    fn two_profiles_never_share_paths_keys_endpoints_or_mcp_names() {
        let base = tempfile::tempdir().expect("temp data dir");
        let secrets = MemorySecretStore::default();
        let profiles = Profiles::under(base.path().into());
        let personal = profiles.ensure_default(&secrets).expect("default profile");
        let bot = profiles.create("bot", &secrets).expect("create bot");

        let sid = Some("S-1-5-21-1004336348-1177238915-682003330-512");
        let [a, b] = [&personal, &bot].map(|p| {
            let paths = profiles.paths(p);
            let key = secrets.get(p).expect("read key").expect("key created");
            let endpoint = paths.agent_endpoint(p, sid).expect("endpoint");
            (paths, key, endpoint, p.mcp_server_name())
        });
        let files = |p: &ProfilePaths| {
            HashSet::from([
                p.root.clone(),
                p.signal_store.clone(),
                p.message_store.clone(),
                p.agent_log.clone(),
            ])
        };
        assert!(
            files(&a.0).is_disjoint(&files(&b.0)),
            "shared paths: {:?} {:?}",
            a.0,
            b.0
        );
        assert!(!a.0.root.starts_with(&b.0.root) && !b.0.root.starts_with(&a.0.root));
        assert_ne!(a.1, b.1, "both profiles got the same store key");
        assert_ne!(a.2, b.2, "shared agent endpoint");
        assert_ne!(a.3, b.3, "shared MCP server name");
    }
}
