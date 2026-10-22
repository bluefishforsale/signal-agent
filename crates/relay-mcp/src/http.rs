//! MCP over Streamable HTTP, for clients on other machines. Off unless the
//! user turns it on, and every request must carry a token the user made.

mod proxy;
mod server;
mod settings;
mod tls;
mod tokens;

use std::{fs, path::Path};

use anyhow::Context;
use relay_core::{ProfileName, ProfilePaths};

pub use server::{Listener, serve};
pub use settings::{HttpSettings, TlsFiles};
pub use tls::Tls;
pub use tokens::{Grant, Tokens};

/// Serves the profile's HTTP endpoint through its agent, if the user turned
/// it on; otherwise returns at once.
pub async fn run_http(profile: &ProfileName, paths: &ProfilePaths) -> anyhow::Result<()> {
    let settings = HttpSettings::load(paths)?;
    if !settings.enabled {
        return Ok(());
    }
    let (addr, tls) = settings.listen_on(paths)?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let listener = Listener::bind(addr, tls.as_ref().map(|t| t.config.clone())).await?;
    match &tls {
        Some(tls) => tracing::info!(
            "serving MCP at {scheme}://{addr}/mcp, certificate SHA-256 {}",
            tls.fingerprint
        ),
        None => tracing::info!("serving MCP at {scheme}://{addr}/mcp"),
    }
    let agent = relay_agent::endpoint::name(profile, paths)?;
    serve(listener, Tokens::of(paths), agent).await
}

/// Writes `bytes` to `path` readable only by the current user.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // %LOCALAPPDATA%, where profiles live on Windows, is already private to the user.
    let mut file = options
        .open(path)
        .with_context(|| format!("unable to write \"{}\"", path.display()))?;
    // NOTE: `mode` only applies when the file is created; a pre-existing file
    // keeps whatever permissions it had, so tighten them explicitly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("unable to restrict \"{}\"", path.display()))?;
    }
    file.write_all(bytes)
        .with_context(|| format!("unable to write \"{}\"", path.display()))
}
