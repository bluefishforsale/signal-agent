//! MCP over Streamable HTTP, for clients on other machines. Off unless the
//! user turns it on, and every request must carry a token the user made.

mod proxy;
mod server;
mod tls;
mod tokens;

use std::{fs, path::Path};

use anyhow::Context;

pub use server::{Listener, serve};
pub use tls::Tls;
pub use tokens::{Grant, Tokens};

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
