//! The bearer tokens remote clients present. Only a hash of each is kept, so
//! the file leaking doesn't hand anyone access, and the user can name and
//! revoke each client separately.

use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail};
use relay_core::AccessMode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The tokens saved for one profile's HTTP endpoint.
#[derive(Clone, Debug)]
pub struct Tokens {
    path: PathBuf,
}

/// Who a presented token belongs to and what it may do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub name: String,
    pub scope: AccessMode,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    name: String,
    scope: AccessMode,
    sha256: String,
}

impl Tokens {
    pub fn at(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
        }
    }

    /// Makes a token for the client `name` and returns it. This is the only
    /// time the token exists outside the client that holds it.
    pub fn create(&self, name: &str, scope: AccessMode) -> anyhow::Result<String> {
        let mut saved = self.load()?;
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| anyhow!("no OS randomness: {e}"))?;
        let token = hex::encode(bytes);
        saved.push(Saved {
            name: name.to_owned(),
            scope,
            sha256: hex::encode(Sha256::digest(token.as_bytes())),
        });
        self.save(&saved)?;
        Ok(token)
    }

    /// Every live token's client and scope.
    pub fn list(&self) -> anyhow::Result<Vec<Grant>> {
        Ok(self
            .load()?
            .into_iter()
            .map(|t| Grant {
                name: t.name,
                scope: t.scope,
            })
            .collect())
    }

    /// Deletes the token made for `name`, so that client is refused from its
    /// next request on.
    pub fn revoke(&self, name: &str) -> anyhow::Result<()> {
        let mut saved = self.load()?;
        let before = saved.len();
        saved.retain(|t| t.name != name);
        if saved.len() == before {
            bail!("no token is named \"{name}\"");
        }
        self.save(&saved)
    }

    /// The grant `presented` carries, if it is a live token.
    pub(crate) fn verify(&self, presented: &str) -> anyhow::Result<Option<Grant>> {
        let digest = Sha256::digest(presented.as_bytes());
        let found = self.load()?.into_iter().find(|t| {
            hex::decode(&t.sha256).is_ok_and(|hash| bool::from(hash.ct_eq(digest.as_slice())))
        });
        Ok(found.map(|t| Grant {
            name: t.name,
            scope: t.scope,
        }))
    }

    fn load(&self) -> anyhow::Result<Vec<Saved>> {
        let path = &self.path;
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("unreadable token file \"{}\"", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(vec![]),
            Err(e) => Err(e).with_context(|| format!("unable to read \"{}\"", path.display())),
        }
    }

    fn save(&self, saved: &[Saved]) -> anyhow::Result<()> {
        let json = serde_json::to_vec_pretty(saved)?;
        super::write_private(&self.path, &json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(dir: &tempfile::TempDir) -> Tokens {
        Tokens::at(&dir.path().join("http_tokens.json"))
    }

    /// A new token must authenticate as the client it was made for, with the
    /// scope it was given, while the saved file holds no trace of the token.
    #[test]
    fn a_created_token_verifies_with_its_scope_and_is_never_stored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tokens = tokens(&dir);
        let secret = tokens
            .create("laptop", AccessMode::ReadOnly)
            .expect("create");
        let grant = tokens.verify(&secret).expect("verify");
        assert_eq!(
            grant,
            Some(Grant {
                name: "laptop".into(),
                scope: AccessMode::ReadOnly
            })
        );
        let saved = std::fs::read_to_string(dir.path().join("http_tokens.json")).expect("read");
        assert!(
            !saved.contains(&secret),
            "token stored in the clear: {saved}"
        );
        assert_eq!(tokens.verify("not-a-token").expect("verify"), None);
    }

    /// Revoking a client must lock it out at once without touching any other
    /// client's token, and the list must show what's left.
    #[test]
    fn a_revoked_token_stops_verifying_and_others_keep_working() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tokens = tokens(&dir);
        let laptop = tokens.create("laptop", AccessMode::Full).expect("create");
        let phone = tokens
            .create("phone", AccessMode::ReadOnly)
            .expect("create");
        tokens.revoke("laptop").expect("revoke");
        assert_eq!(tokens.verify(&laptop).expect("verify"), None, "revoked");
        assert!(tokens.verify(&phone).expect("verify").is_some(), "other");
        let listed = tokens.list().expect("list");
        assert_eq!(
            listed,
            vec![Grant {
                name: "phone".into(),
                scope: AccessMode::ReadOnly
            }]
        );
        assert!(tokens.revoke("laptop").is_err(), "revoked twice silently");
    }
}
