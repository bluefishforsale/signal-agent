//! The certificate the endpoint serves off loopback: the user's own, or one
//! made here and kept, whose fingerprint the user pins in their client.

use std::{path::Path, sync::Arc};

use anyhow::{Context, bail};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use sha2::{Digest, Sha256};

/// A certificate to serve and the SHA-256 fingerprint a client can pin.
#[derive(Clone, Debug)]
pub struct Tls {
    pub config: Arc<ServerConfig>,
    pub fingerprint: String,
}

impl Tls {
    /// Serves the user's own certificate chain and private key, both PEM.
    pub fn from_files(cert: &Path, key: &Path) -> anyhow::Result<Self> {
        let chain = CertificateDer::pem_file_iter(cert)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .with_context(|| format!("unable to read certificates from \"{}\"", cert.display()))?;
        let Some(leaf) = chain.first() else {
            bail!("\"{}\" holds no certificate", cert.display());
        };
        let fingerprint = fingerprint(leaf);
        let key = PrivateKeyDer::from_pem_file(key)
            .with_context(|| format!("unable to read a private key from \"{}\"", key.display()))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .with_context(|| format!("\"{}\" does not match its key", cert.display()))?;
        Ok(Self {
            config: Arc::new(config),
            fingerprint,
        })
    }

    /// Serves the certificate made for this endpoint in `dir`, valid for
    /// `names`, making it on first use and keeping it after.
    pub fn self_signed(dir: &Path, names: &[String]) -> anyhow::Result<Self> {
        let (cert, key) = (dir.join("http-cert.pem"), dir.join("http-key.pem"));
        if !cert.exists() {
            let made = rcgen::generate_simple_self_signed(names.to_vec())
                .context("unable to make a self-signed certificate")?;
            // The key first: a certificate on disk always has its key beside it.
            super::write_private(&key, made.signing_key.serialize_pem().as_bytes())?;
            super::write_private(&cert, made.cert.pem().as_bytes())?;
        }
        Self::from_files(&cert, &key)
    }
}

/// SHA-256 of the certificate, as colon-separated hex pairs the way browsers
/// and `openssl x509 -fingerprint -sha256` show it.
fn fingerprint(cert: &CertificateDer) -> String {
    Sha256::digest(cert)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use rustls::pki_types::{CertificateDer, pem::PemObject};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::http::{Listener, Tokens, serve};

    /// A client pins the fingerprint it was shown, so the made certificate
    /// must survive a restart, and the endpoint must serve exactly it.
    #[tokio::test]
    async fn a_self_signed_certificate_is_kept_and_served_under_its_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let names = vec!["localhost".to_owned()];
        let first = Tls::self_signed(dir.path(), &names).expect("make");
        let again = Tls::self_signed(dir.path(), &names).expect("reload");
        assert_eq!(first.fingerprint, again.fingerprint, "remade on restart");

        let pem = std::fs::read(dir.path().join("http-cert.pem")).expect("cert file");
        let der = CertificateDer::from_pem_slice(&pem).expect("pem");
        let expected = hex::encode_upper(Sha256::digest(&der))
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(first.fingerprint, expected);

        let listener = Listener::bind(([127, 0, 0, 1], 0).into(), Some(again.config))
            .await
            .expect("bind");
        let addr: SocketAddr = listener.local_addr().expect("addr");
        let tokens = Tokens::at(&dir.path().join("http_tokens.json"));
        tokio::spawn(serve(listener, tokens, "no-agent".into()));
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .no_proxy()
            .tls_certs_only([reqwest::Certificate::from_pem(&pem).expect("cert")])
            .resolve("localhost", addr)
            .build()
            .expect("client");
        let response = client
            .post(format!("https://localhost:{}/mcp", addr.port()))
            .send()
            .await
            .expect("TLS request with the pinned certificate");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    }
}
