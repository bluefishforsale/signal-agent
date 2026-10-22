//! The HTTP endpoint's Advanced settings. Kept in their own file so turning
//! the endpoint on or off never touches the settings the agent depends on.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use anyhow::Context;
use relay_core::ProfilePaths;
use serde::{Deserialize, Serialize};

use super::Tls;

/// How, and whether, a profile serves MCP over HTTP.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpSettings {
    pub enabled: bool,
    pub port: u16,
    /// Listen on every interface instead of loopback only. Always with TLS.
    pub lan: bool,
    /// The user's own certificate; without one, LAN gets a self-signed one.
    pub tls: Option<TlsFiles>,
    /// Host names or addresses a self-signed certificate is valid for, besides
    /// localhost: whatever clients on the LAN dial.
    pub tls_names: Vec<String>,
}

/// A PEM certificate chain and its private key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 8765,
            lan: false,
            tls: None,
            tls_names: vec![],
        }
    }
}

impl HttpSettings {
    /// The profile's saved HTTP settings, or the defaults (off) when none.
    pub fn load(paths: &ProfilePaths) -> anyhow::Result<Self> {
        let path = file(paths);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("unreadable HTTP settings \"{}\"", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("unable to read \"{}\"", path.display())),
        }
    }

    pub fn save(&self, paths: &ProfilePaths) -> anyhow::Result<()> {
        super::write_private(&file(paths), &serde_json::to_vec_pretty(self)?)
    }

    /// Where to listen and the certificate to serve there, if any. LAN always
    /// gets one; loopback only when the user supplied their own.
    pub fn listen_on(&self, paths: &ProfilePaths) -> anyhow::Result<(SocketAddr, Option<Tls>)> {
        let ip = if self.lan {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        };
        let tls = match (&self.tls, self.lan) {
            (Some(files), _) => Some(Tls::from_files(&files.cert, &files.key)?),
            (None, true) => {
                let mut names = vec!["localhost".to_owned()];
                names.extend(self.tls_names.iter().cloned());
                Some(Tls::self_signed(&paths.root, &names)?)
            }
            (None, false) => None,
        };
        Ok(((ip, self.port).into(), tls))
    }
}

fn file(paths: &ProfilePaths) -> PathBuf {
    paths.root.join("http.json")
}

#[cfg(test)]
mod tests {
    use relay_core::ProfileName;

    use super::*;

    fn paths(dir: &tempfile::TempDir) -> ProfilePaths {
        ProfilePaths::under(dir.path().to_path_buf(), &ProfileName::default_profile())
    }

    /// PRD goal 9: a profile nobody configured serves nothing over HTTP, and
    /// turning it on without opting into LAN stays on loopback in plain HTTP.
    #[test]
    fn the_endpoint_is_off_by_default_and_loopback_when_turned_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let settings = HttpSettings::load(&paths).expect("load");
        assert!(!settings.enabled, "on by default");
        let on = HttpSettings {
            enabled: true,
            ..settings
        };
        on.save(&paths).expect("save");
        let (addr, tls) = HttpSettings::load(&paths)
            .expect("reload")
            .listen_on(&paths)
            .expect("plan");
        assert!(addr.ip().is_loopback(), "listens on {addr}");
        assert!(tls.is_none(), "loopback made a certificate");
    }

    /// Opting into LAN must never mean plain HTTP: with no certificate of the
    /// user's own, one is made and its fingerprint is there to show.
    #[test]
    fn opting_into_lan_listens_everywhere_with_a_self_signed_certificate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let settings = HttpSettings {
            enabled: true,
            lan: true,
            ..HttpSettings::default()
        };
        let (addr, tls) = settings.listen_on(&paths).expect("plan");
        assert!(addr.ip().is_unspecified(), "LAN listens on {addr}");
        let Some(tls) = tls else {
            panic!("LAN without TLS")
        };
        assert_eq!(tls.fingerprint.len(), 95, "{}", tls.fingerprint);
    }
}
