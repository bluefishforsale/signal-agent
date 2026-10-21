//! Profiles: one Signal account each, with its own store, key, agent and socket.

use std::{fmt, ops::Deref};

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// A profile's name. Lowercase letters, digits and hyphens, 1 to 32 characters,
/// because it becomes part of directory names, socket and pipe names, and the
/// MCP server name `signal-<profile>`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProfileName(String);

impl ProfileName {
    pub fn new(name: impl Into<String>) -> anyhow::Result<Self> {
        let name = name.into();
        let valid_chars = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if name.is_empty() || name.len() > 32 || !valid_chars || name.starts_with('-') {
            bail!(
                "profile name \"{name}\" must be 1 to 32 lowercase letters, digits or hyphens, not starting with a hyphen"
            );
        }
        Ok(Self(name))
    }

    /// The profile a first install creates without asking.
    pub fn default_profile() -> Self {
        Self("personal".to_owned())
    }

    /// The name this profile's MCP server is registered under in Claude clients.
    pub fn mcp_server_name(&self) -> String {
        format!("signal-{}", self.0)
    }
}

impl Deref for ProfileName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ProfileName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ProfileName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ProfileName({})", self.0)
    }
}

impl TryFrom<String> for ProfileName {
    type Error = anyhow::Error;
    fn try_from(s: String) -> anyhow::Result<Self> {
        Self::new(s)
    }
}

impl From<ProfileName> for String {
    fn from(p: ProfileName) -> String {
        p.0
    }
}

/// Whether the profile is a linked device on someone's account or the primary
/// device of its own number. Some Signal operations only work from a primary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Linked,
    Primary,
}

/// What MCP clients may do with this profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    Full,
    ReadOnly,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Any accepted name must be safe in a path component and a pipe name,
        /// since the name is spliced into both.
        #[test]
        fn accepted_profile_names_are_path_and_pipe_safe(s in ".{0,40}") {
            if let Ok(name) = ProfileName::new(s) {
                prop_assert!(!name.contains(['/', '\\', '.', ' ', ':']), "unsafe name accepted: {name:?}");
                prop_assert!(!name.is_empty() && name.len() <= 32);
            }
        }
    }

    /// Deserializing must enforce the same rules as the constructor, or a bad
    /// config file would smuggle an invalid name past validation.
    #[test]
    fn deserializing_a_profile_name_validates_it() {
        let bad: Result<ProfileName, _> = serde_json::from_str("\"../etc\"");
        assert!(
            bad.is_err(),
            "deserialized an invalid profile name: {bad:?}"
        );
    }
}
