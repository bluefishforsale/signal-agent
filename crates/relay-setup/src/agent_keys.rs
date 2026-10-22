//! Store keys reached through the bundled `signal-agent`, so it is the only
//! program that ever creates or reads an item in the OS secret store. On macOS
//! a Keychain item trusts the program that created it; one made by the setup
//! app would make the login-started agent prompt, or fail with nobody there.

use std::path::PathBuf;

use anyhow::{Context, bail};
use relay_core::ProfileName;
use relay_secrets::{SecretStore, StoreKey};

use crate::claude::{Runner, SystemRunner};

/// What `signal-agent key` does with one profile's key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Prints the key, or nothing when there is none.
    Get,
    /// Prints the key, creating and saving one first if there is none.
    Ensure,
    Delete,
}

impl KeyAction {
    fn word(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Ensure => "ensure",
            Self::Delete => "delete",
        }
    }

    /// The `signal-agent` arguments that run this action for `profile`.
    pub fn args(self, profile: &ProfileName) -> Vec<String> {
        ["key", self.word(), "--profile", profile.as_ref()]
            .map(String::from)
            .into()
    }
}

/// Runs `action` for `profile` against `store`, returning what
/// `signal-agent key` prints.
pub fn serve_key(
    action: KeyAction,
    profile: &ProfileName,
    store: &dyn SecretStore,
) -> anyhow::Result<String> {
    let key = match action {
        KeyAction::Get => store.get(profile)?,
        KeyAction::Ensure => Some(store.ensure(profile)?),
        KeyAction::Delete => {
            store.delete(profile)?;
            None
        }
    };
    Ok(key
        .map(|k| format!("{}\n", hex(k.as_bytes())))
        .unwrap_or_default())
}

/// A profile's key kept by the `signal-agent` at `agent`, asked for over a
/// pipe that nothing logs.
pub struct AgentKeys {
    agent: PathBuf,
    runner: Box<dyn Runner>,
}

impl AgentKeys {
    pub fn new(agent: PathBuf) -> Self {
        Self::with_runner(agent, Box::new(SystemRunner))
    }

    pub(crate) fn with_runner(agent: PathBuf, runner: Box<dyn Runner>) -> Self {
        Self { agent, runner }
    }

    fn run(&self, action: KeyAction, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        let out = self.runner.run(&self.agent, &action.args(profile))?;
        if !out.success {
            bail!(
                "signal-agent could not {} the key for {profile}: {}",
                action.word(),
                out.stderr.trim()
            );
        }
        let printed = out.stdout.trim();
        if printed.is_empty() {
            return Ok(None);
        }
        let bytes = unhex(printed)
            .with_context(|| format!("signal-agent printed no usable key for {profile}"))?;
        Ok(Some(StoreKey::from_bytes(bytes)))
    }
}

impl SecretStore for AgentKeys {
    fn get(&self, profile: &ProfileName) -> anyhow::Result<Option<StoreKey>> {
        self.run(KeyAction::Get, profile)
    }

    // NOTE: never pass a key to signal-agent. Arguments show in every user's
    // process list, and a key the agent makes itself needs no passing.
    fn set(&self, profile: &ProfileName, _: &StoreKey) -> anyhow::Result<()> {
        bail!("signal-agent makes {profile}'s key itself; it is never handed one")
    }

    fn delete(&self, profile: &ProfileName) -> anyhow::Result<()> {
        self.run(KeyAction::Delete, profile).map(drop)
    }

    fn ensure(&self, profile: &ProfileName) -> anyhow::Result<StoreKey> {
        self.run(KeyAction::Ensure, profile)?
            .with_context(|| format!("signal-agent printed no key for {profile}"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 bytes from 64 hex digits; anything else is None, so a damaged reply
/// can never become a different key.
fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 32];
    for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use relay_secrets::MemorySecretStore;

    use super::*;
    use crate::claude::RunOutput;

    impl KeyAction {
        fn parse(args: &[String]) -> Option<(Self, ProfileName)> {
            let [key, word, flag, profile] = args else {
                return None;
            };
            if key != "key" || flag != "--profile" {
                return None;
            }
            let action = [Self::Get, Self::Ensure, Self::Delete]
                .into_iter()
                .find(|a| a.word() == word)?;
            Some((action, ProfileName::new(profile.as_str()).ok()?))
        }
    }

    /// `signal-agent key ...` run in-process against one shared store.
    struct Sidecar(MemorySecretStore);

    impl Runner for Sidecar {
        fn run(&self, program: &Path, args: &[String]) -> anyhow::Result<RunOutput> {
            assert_eq!(program, Path::new("/app/signal-agent"));
            let Some((action, profile)) = KeyAction::parse(args) else {
                anyhow::bail!("not a key command: {args:?}");
            };
            Ok(match serve_key(action, &profile, &self.0) {
                Ok(stdout) => RunOutput {
                    success: true,
                    stdout,
                    stderr: String::new(),
                },
                Err(e) => RunOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: format!("{e:#}"),
                },
            })
        }
    }

    fn profile() -> ProfileName {
        ProfileName::new("personal").expect("valid name")
    }

    /// Setup's key must be the one the agent's own store holds: created there
    /// once, handed back unchanged, and gone after uninstall deletes it.
    #[test]
    fn the_key_setup_gets_is_the_one_the_agent_saved() {
        let keys = AgentKeys::with_runner(
            "/app/signal-agent".into(),
            Box::new(Sidecar(MemorySecretStore::default())),
        );
        assert_eq!(keys.get(&profile()).expect("get"), None, "phantom key");
        let made = keys.ensure(&profile()).expect("ensure");
        assert_eq!(keys.get(&profile()).expect("get"), Some(made.clone()));
        assert_eq!(keys.ensure(&profile()).expect("ensure again"), made);
        keys.delete(&profile()).expect("delete");
        assert_eq!(keys.get(&profile()).expect("get"), None, "deleted key");
    }

    /// A reply that is whatever text the test gives.
    struct Replies(String);

    impl Runner for Replies {
        fn run(&self, _: &Path, _: &[String]) -> anyhow::Result<RunOutput> {
            Ok(RunOutput {
                success: true,
                stdout: self.0.clone(),
                stderr: String::new(),
            })
        }
    }

    /// A damaged or truncated reply must be refused, never read as some other
    /// key that would leave the profile's databases unreadable.
    #[test]
    fn a_reply_that_is_not_exactly_one_key_is_refused() {
        let good = "ab".repeat(32);
        let replies = [
            good[..62].to_owned(),
            format!("+{}", &good[1..]),
            format!("{good}00"),
            "zz".repeat(32),
        ];
        for reply in replies {
            let keys = AgentKeys::with_runner(
                "/app/signal-agent".into(),
                Box::new(Replies(reply.clone())),
            );
            assert!(keys.ensure(&profile()).is_err(), "accepted {reply:?}");
        }
    }
}
