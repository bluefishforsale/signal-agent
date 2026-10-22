//! Named profiles: which accounts this user has set up, so setup, the CLI and
//! uninstall agree on the list, and each account's files, key, agent and
//! Claude entry stay apart from every other's.

use std::{fs, io, path::PathBuf};

use anyhow::{Context, bail};
use relay_core::{ProfileName, ProfilePaths};
use relay_secrets::SecretStore;
use serde::{Deserialize, Serialize};

use crate::{
    Step,
    claude::{Clients, ServerEntry},
    uninstall_steps,
};

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
    /// The registry in the platform data directory, beside the profiles it lists.
    pub fn system() -> anyhow::Result<Self> {
        let root = ProfilePaths::for_profile(&ProfileName::default_profile())?.root;
        let base = root
            .parent()
            .with_context(|| format!("profile directory \"{}\" has no parent", root.display()))?;
        Ok(Self::under(base.to_path_buf()))
    }

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
    pub fn ensure_default(&self) -> anyhow::Result<ProfileName> {
        let registry = self.load()?;
        if let Some(first) = registry.profiles.first() {
            return Ok(first.clone());
        }
        self.add(registry, ProfileName::default_profile())
    }

    /// Adds the profile `name`, which must be a valid name not already in use.
    pub fn create(&self, name: &str) -> anyhow::Result<ProfileName> {
        let profile = self.available(name)?;
        self.add(self.load()?, profile)
    }

    /// `name` as a profile, if it is valid and not already in use; lists
    /// nothing.
    pub fn available(&self, name: &str) -> anyhow::Result<ProfileName> {
        let profile = ProfileName::new(name)?;
        if self.load()?.profiles.contains(&profile) {
            bail!("a profile called \"{profile}\" already exists");
        }
        Ok(profile)
    }

    /// Lists `profile` if it isn't already.
    pub(crate) fn ensure(&self, profile: &ProfileName) -> anyhow::Result<()> {
        let registry = self.load()?;
        if registry.profiles.contains(profile) {
            return Ok(());
        }
        self.add(registry, profile.clone()).map(drop)
    }

    /// Where `profile`'s files live.
    pub fn paths(&self, profile: &ProfileName) -> ProfilePaths {
        ProfilePaths::under(self.base.clone(), profile)
    }

    /// The cleanup that removes `profile`: its Claude entries, the caller's
    /// `login_item` step, its key, its data when `delete_data`, and finally
    /// its place in the list. `entry` must be this profile's entry.
    pub fn removal_steps<'a>(
        &'a self,
        profile: &'a ProfileName,
        clients: &'a Clients,
        entry: &'a ServerEntry,
        login_item: Step<'a>,
        secrets: &'a dyn SecretStore,
        delete_data: bool,
    ) -> anyhow::Result<Vec<Step<'a>>> {
        if entry.name != profile.mcp_server_name() {
            bail!(
                "Claude entry \"{}\" does not belong to profile \"{profile}\"",
                entry.name
            );
        }
        let key = Step::new("Remove the database key", move || secrets.delete(profile));
        let data = delete_data.then(|| {
            let root = self.paths(profile).root;
            Step::new("Delete messages and account data", move || {
                fs::remove_dir_all(&root)
                    .with_context(|| format!("unable to delete \"{}\"", root.display()))
            })
        });
        let forget = Step::new(format!("Forget profile \"{profile}\""), move || {
            self.forget(profile)
        });
        let mut steps = uninstall_steps(clients, entry, login_item, key, data);
        steps.push(forget);
        Ok(steps)
    }

    fn forget(&self, profile: &ProfileName) -> anyhow::Result<()> {
        let mut registry = self.load()?;
        registry.profiles.retain(|p| p != profile);
        self.save(&registry)
    }

    fn add(&self, mut registry: Registry, profile: ProfileName) -> anyhow::Result<ProfileName> {
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
    use std::{cell::Cell, collections::HashSet, path::Path};

    use relay_secrets::{MemorySecretStore, StoreKey};

    use super::*;
    use crate::{
        claude::{ClaudeClient, Platform, RunOutput, Runner},
        run_steps,
    };

    fn name(s: &str) -> ProfileName {
        ProfileName::new(s).expect("valid profile name")
    }

    /// A first install makes `personal` without mentioning profiles, and the
    /// next launch must find it rather than make another.
    #[test]
    fn the_first_install_creates_personal_and_remembers_it() {
        let base = tempfile::tempdir().expect("temp data dir");
        let created = Profiles::under(base.path().into())
            .ensure_default()
            .expect("default profile");
        assert_eq!(created, name("personal"));

        let reopened = Profiles::under(base.path().into());
        assert_eq!(reopened.list().expect("list"), [name("personal")]);
        assert_eq!(
            reopened.ensure_default().expect("default"),
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
        let profiles = Profiles::under(base.path().into());
        profiles.ensure_default().expect("default profile");

        let bot = profiles.create("bot").expect("create bot");
        assert_eq!(bot, name("bot"));
        for bad in ["bot", "personal", "Work Phone", "../etc", ""] {
            let refused = profiles.create(bad);
            assert!(refused.is_err(), "created {bad:?}: {refused:?}");
        }
        assert_eq!(
            profiles.list().expect("list"),
            [name("personal"), name("bot")]
        );
    }

    /// The app's "Add another account" checks a typed name before setup
    /// starts, but setup lists the profile only once it begins; checking must
    /// not list it, or a name typed and abandoned would show as an account.
    #[test]
    fn checking_a_new_name_refuses_bad_and_taken_names_and_lists_nothing() {
        let base = tempfile::tempdir().expect("temp data dir");
        let profiles = Profiles::under(base.path().into());
        profiles.ensure_default().expect("default profile");

        assert_eq!(profiles.available("bot").expect("bot is free"), name("bot"));
        for bad in ["personal", "Work Phone", "../etc", ""] {
            let refused = profiles.available(bad);
            assert!(refused.is_err(), "accepted {bad:?}: {refused:?}");
        }
        assert_eq!(profiles.list().expect("list"), [name("personal")]);
    }

    /// Two accounts on one machine must never touch each other's files, agent
    /// socket or Claude entry; any overlap would mix two Signal identities in
    /// one store or route one profile's tools to the other.
    #[test]
    fn two_profiles_never_share_paths_endpoints_or_mcp_names() {
        let base = tempfile::tempdir().expect("temp data dir");
        let profiles = Profiles::under(base.path().into());
        let personal = profiles.ensure_default().expect("default profile");
        let bot = profiles.create("bot").expect("create bot");

        let sid = Some("S-1-5-21-1004336348-1177238915-682003330-512");
        let [a, b] = [&personal, &bot].map(|p| {
            let paths = profiles.paths(p);
            let endpoint = paths.agent_endpoint(p, sid).expect("endpoint");
            (paths, endpoint, p.mcp_server_name())
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
        assert_ne!(a.1, b.1, "shared agent endpoint");
        assert_ne!(a.2, b.2, "shared MCP server name");
    }

    /// The agent finds its files through `ProfilePaths::for_profile`, so the
    /// registry must resolve a profile to the same place or setup would
    /// prepare one directory and the agent open another.
    #[test]
    fn the_system_registry_agrees_with_the_agent_on_where_a_profile_lives() {
        let profile = name("bot");
        let registry = Profiles::system().expect("system registry");
        let agent = ProfilePaths::for_profile(&profile).expect("agent paths");
        assert_eq!(registry.paths(&profile), agent);
    }

    struct NoCli;

    impl Runner for NoCli {
        fn run(&self, program: &Path, _: &[String]) -> anyhow::Result<RunOutput> {
            anyhow::bail!("unexpected run of {}", program.display())
        }
    }

    /// Removing one account must take away everything of that profile (its
    /// Claude entry, login item, key, data, and its place in the list) and
    /// nothing of any other profile's.
    #[test]
    fn removing_a_profile_cleans_up_only_that_profile() {
        let base = tempfile::tempdir().expect("temp data dir");
        let desktop = tempfile::tempdir().expect("temp Desktop dir");
        let clients = Clients::new(
            Platform::MacOs,
            Box::new(NoCli),
            None,
            Some(desktop.path().into()),
        );
        let secrets = MemorySecretStore::default();
        let profiles = Profiles::under(base.path().into());
        let personal = profiles.ensure_default().expect("default profile");
        let bot = profiles.create("bot").expect("create bot");
        let entry = |p: &ProfileName| {
            ServerEntry::for_profile(p, &base.path().join("signal-agent"), ["mcp"]).expect("entry")
        };
        for p in [&personal, &bot] {
            clients
                .register(ClaudeClient::Desktop, &entry(p))
                .expect("register");
            secrets
                .set(p, &StoreKey::generate().expect("key"))
                .expect("store key");
            fs::create_dir_all(profiles.paths(p).root).expect("profile dir");
            fs::write(profiles.paths(p).message_store, b"db").expect("store file");
        }

        let login_item_removed = Cell::new(false);
        let bot_entry = entry(&bot);
        let steps = profiles
            .removal_steps(
                &bot,
                &clients,
                &bot_entry,
                Step::new("login item", || {
                    login_item_removed.set(true);
                    Ok(())
                }),
                &secrets,
                true,
            )
            .expect("removal steps");
        let outcomes = run_steps(steps);
        assert!(
            outcomes.iter().all(|o| o.result.is_ok()),
            "a step failed: {outcomes:?}"
        );

        assert!(login_item_removed.get(), "login item step never ran");
        let config = fs::read_to_string(desktop.path().join("claude_desktop_config.json"))
            .expect("Desktop config");
        assert!(
            config.contains("signal-personal") && !config.contains("signal-bot"),
            "{config}"
        );
        assert!(secrets.get(&bot).expect("read").is_none(), "bot key kept");
        assert!(
            secrets.get(&personal).expect("read").is_some(),
            "personal key lost"
        );
        assert!(!profiles.paths(&bot).root.exists(), "bot data kept");
        assert!(
            profiles.paths(&personal).message_store.exists(),
            "personal data lost"
        );
        assert_eq!(profiles.list().expect("list"), [personal]);
    }

    /// Removing `bot` with `personal`'s Claude entry would unregister the
    /// wrong account from Claude, so a mismatched entry is refused up front.
    #[test]
    fn removal_refuses_another_profiles_claude_entry() {
        let base = tempfile::tempdir().expect("temp data dir");
        let secrets = MemorySecretStore::default();
        let profiles = Profiles::under(base.path().into());
        let personal = profiles.ensure_default().expect("default profile");
        let bot = profiles.create("bot").expect("create bot");
        let clients = Clients::new(Platform::MacOs, Box::new(NoCli), None, None);
        let wrong = ServerEntry::for_profile(&personal, &base.path().join("signal-agent"), ["mcp"])
            .expect("entry");

        let refused = profiles.removal_steps(
            &bot,
            &clients,
            &wrong,
            Step::new("login item", || Ok(())),
            &secrets,
            false,
        );
        assert!(refused.is_err(), "accepted another profile's entry");
    }
}
