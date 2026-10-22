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

/// Replaces `path` with `bytes`, readable only by the current user. Written
/// beside it and renamed over it, so a reader never sees half a file.
fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let Some(dir) = path.parent() else {
        anyhow::bail!("\"{}\" has no parent directory", path.display());
    };
    fs::create_dir_all(dir).with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    let mut suffix = [0u8; 8];
    getrandom::fill(&mut suffix).map_err(|e| anyhow::anyhow!("no OS randomness: {e}"))?;
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(format!(".{}.tmp", hex::encode(suffix)));
    let temp = dir.join(name);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    // %LOCALAPPDATA%, where profiles live on Windows, is already private to the
    // user, and a new file there inherits that.
    let written = options
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written.with_context(|| format!("unable to write \"{}\"", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Token and settings files replace the old copy whole: a crash mid-save
    /// must never leave a truncated file, and the result is the user's alone
    /// even when the old copy was readable by others.
    #[cfg(unix)]
    #[test]
    fn a_private_write_replaces_the_file_whole_and_user_only() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("http_tokens.json");
        fs::write(&path, b"old").expect("old copy");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let old_inode = fs::metadata(&path).expect("stat").ino();
        write_private(&path, b"new").expect("write");
        let meta = fs::metadata(&path).expect("stat");
        assert_eq!(fs::read(&path).expect("read"), b"new");
        assert_eq!(meta.mode() & 0o777, 0o600, "mode {:o}", meta.mode());
        assert_ne!(meta.ino(), old_inode, "rewritten in place, not replaced");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("list")
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "temp files left: {leftovers:?}");
    }
}
