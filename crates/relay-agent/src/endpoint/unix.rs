//! Unix socket endpoint: a 0600 socket inside a 0700 directory, and a peer uid
//! check on every connection in case either permission is ever loosened.

use std::{
    fs::{self, DirBuilder, File, OpenOptions, Permissions},
    io::ErrorKind,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    },
    path::Path,
};

use anyhow::{Context, bail};
use tokio::net::{UnixListener, UnixStream};

/// A bound endpoint that accepts only connections from this process's user.
pub struct Listener {
    inner: UnixListener,
    owner: u32,
    // Held for the listener's lifetime; the OS releases it if the agent dies.
    _lock: File,
}

impl Listener {
    pub async fn bind(endpoint: &str) -> anyhow::Result<Self> {
        let path = Path::new(endpoint);
        let dir = path
            .parent()
            .with_context(|| format!("socket path \"{endpoint}\" has no directory"))?;
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
        // An existing directory keeps whatever mode it had; tighten it.
        fs::set_permissions(dir, Permissions::from_mode(0o700))
            .with_context(|| format!("unable to restrict \"{}\"", dir.display()))?;
        let lock = lock_profile(dir)?;
        // Holding the lock means no live agent owns this socket, so whatever file
        // is there was left by a crash.
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("unable to remove stale \"{endpoint}\""));
            }
        }
        let inner = UnixListener::bind(path)
            .with_context(|| format!("unable to listen on \"{endpoint}\""))?;
        fs::set_permissions(path, Permissions::from_mode(0o600))
            .with_context(|| format!("unable to restrict \"{endpoint}\""))?;
        Ok(Self {
            inner,
            owner: current_uid(),
            _lock: lock,
        })
    }

    /// The next connection from this user. Connections from anyone else are
    /// closed and skipped, so a stranger can neither talk nor stop the agent.
    pub async fn accept(&mut self) -> anyhow::Result<UnixStream> {
        loop {
            let (stream, _) = self.inner.accept().await.context("accept failed")?;
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == self.owner => return Ok(stream),
                Ok(cred) => tracing::warn!("refused a connection from uid {}", cred.uid()),
                Err(e) => tracing::warn!("refused a connection with no peer credentials: {e}"),
            }
        }
    }
}

/// Takes the profile's single-agent lock, failing if another agent holds it.
fn lock_profile(dir: &Path) -> anyhow::Result<File> {
    let path = dir.join("agent.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("unable to open \"{}\"", path.display()))?;
    // SAFETY: the descriptor is valid for as long as `file` lives.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if locked != 0 {
        bail!(
            "another agent is already running for this profile (lock \"{}\" is held)",
            path.display()
        );
    }
    Ok(file)
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Connects to the agent listening at `endpoint`.
pub async fn connect(endpoint: &str) -> anyhow::Result<UnixStream> {
    UnixStream::connect(Path::new(endpoint))
        .await
        .with_context(|| format!("unable to connect to \"{endpoint}\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    /// A crashed agent leaves its socket file behind; the next agent must be able
    /// to start anyway, or one crash would need manual cleanup.
    #[tokio::test]
    async fn bind_replaces_a_stale_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp
            .path()
            .join("p")
            .join("agent.sock")
            .display()
            .to_string();
        drop(Listener::bind(&sock).await.expect("first bind"));
        assert!(
            Path::new(&sock).exists(),
            "test needs a leftover socket file"
        );
        let again = Listener::bind(&sock).await;
        assert!(
            again.is_ok(),
            "stale socket blocked startup: {:?}",
            again.err()
        );
    }

    /// When two shims race to start the agent, the second agent must fail to
    /// bind rather than unlink the live socket and split clients between two
    /// processes driving one Signal device.
    #[tokio::test]
    async fn bind_refuses_while_another_agent_is_listening() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp
            .path()
            .join("p")
            .join("agent.sock")
            .display()
            .to_string();
        let _live = Listener::bind(&sock).await.expect("first bind");
        let second = Listener::bind(&sock).await;
        assert!(second.is_err(), "second agent bound over a live one");
        assert!(connect(&sock).await.is_ok(), "live agent lost its socket");
    }

    /// A directory left world-readable by an earlier install or by hand must be
    /// tightened, not trusted.
    #[tokio::test]
    async fn bind_tightens_an_existing_loose_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("profile");
        fs::create_dir(&dir).expect("mkdir");
        fs::set_permissions(&dir, Permissions::from_mode(0o755)).expect("chmod");
        let _listener = Listener::bind(&dir.join("agent.sock").display().to_string())
            .await
            .expect("bind");
        assert_eq!(mode(&dir), 0o700, "loose directory left as is");
    }

    /// Even if the file permissions were loosened, a connection from any other
    /// uid must be dropped before it can speak, and must not stop the listener
    /// from serving the real user. The listener is told it belongs to someone
    /// else, which makes this test's own connection the stranger.
    #[tokio::test]
    async fn connections_from_another_uid_are_dropped_unheard() {
        use tokio::io::AsyncReadExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let sock = tmp
            .path()
            .join("p")
            .join("agent.sock")
            .display()
            .to_string();
        let mut listener = Listener::bind(&sock).await.expect("bind");
        listener.owner = current_uid().wrapping_add(1);
        let accepted = tokio::spawn(async move { listener.accept().await.map(|_| ()) });
        let mut stranger = connect(&sock).await.expect("connect");
        let mut buf = [0u8; 1];
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), stranger.read(&mut buf))
            .await
            .expect("stranger's connection left open");
        assert!(
            matches!(read, Ok(0) | Err(_)),
            "stranger got data: {read:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !accepted.is_finished(),
            "accept handed over or gave up on a stranger"
        );
        accepted.abort();
    }

    /// Other local users must not be able to reach the socket: its directory is
    /// 0700 and the socket itself 0600, created that way by bind.
    #[tokio::test]
    async fn bind_makes_a_user_only_directory_and_socket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("profile");
        let sock = dir.join("agent.sock");
        let _listener = Listener::bind(&sock.display().to_string())
            .await
            .expect("bind");
        assert_eq!(mode(&dir), 0o700, "profile directory mode");
        assert_eq!(mode(&sock), 0o600, "socket mode");
    }
}
