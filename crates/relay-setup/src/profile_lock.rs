//! Who may open a profile's stores right now. Two processes driving one
//! Signal store corrupt its sessions, so setup run again on a set-up profile
//! must get the running agent to let go first, and keep every other agent
//! from starting until it is done. OS file locks do both, and the OS drops
//! them when their process dies, so a crash never leaves a profile stuck.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
};

use anyhow::Context;
use relay_core::ProfilePaths;

/// Held while setup runs on a profile: its agent lets go of the stores, and
/// nothing starts one.
pub struct SetupLock {
    _held: File,
}

impl SetupLock {
    /// The lock, or None while another setup holds it.
    pub fn try_acquire(paths: &ProfilePaths) -> anyhow::Result<Option<Self>> {
        Ok(try_lock(&setup_file(paths))?.map(|_held| Self { _held }))
    }
}

/// Held while a process has a profile's Signal and message stores open.
pub struct StoreLock {
    _held: File,
}

impl StoreLock {
    /// The lock, or None while another process has the stores open.
    pub fn try_acquire(paths: &ProfilePaths) -> anyhow::Result<Option<Self>> {
        Ok(try_lock(&paths.root.join("store.lock"))?.map(|_held| Self { _held }))
    }
}

/// Whether setup is running on this profile right now.
pub fn setup_running(paths: &ProfilePaths) -> bool {
    let file = match File::open(setup_file(paths)) {
        Ok(file) => file,
        Err(_) => return false,
    };
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}

fn setup_file(paths: &ProfilePaths) -> PathBuf {
    paths.root.join("setup.lock")
}

/// An exclusive lock on the file at `path`, created with its folder if
/// missing, or None while someone else holds it.
fn try_lock(path: &Path) -> anyhow::Result<Option<File>> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .with_context(|| format!("unable to create \"{}\"", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("unable to open \"{}\"", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("unable to lock \"{}\"", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use relay_core::{ProfileName, ProfilePaths};

    use super::*;

    fn paths(dir: &tempfile::TempDir) -> ProfilePaths {
        ProfilePaths::under(dir.path().to_owned(), &ProfileName::default_profile())
    }

    /// Whether `check` holds within a second. A lock let go of can stay held
    /// for an instant longer: a process a parallel test is spawning shares
    /// its descriptor until it execs.
    fn soon(check: impl Fn() -> bool) -> bool {
        (0..100).any(|_| {
            check() || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                false
            }
        })
    }

    /// Agents and the shim must see setup running for exactly as long as
    /// setup holds its lock: a setup that quit or crashed must not keep the
    /// profile's agent off forever.
    #[test]
    fn setup_counts_as_running_only_while_its_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        assert!(!setup_running(&paths), "running before setup started");
        assert!(!paths.root.exists(), "the check created the profile folder");
        let held = SetupLock::try_acquire(&paths).expect("lock").expect("free");
        assert!(setup_running(&paths), "setup's lock went unseen");
        drop(held);
        assert!(
            soon(|| !setup_running(&paths)),
            "a dropped lock still counts"
        );
    }

    /// The agent and setup must never have one profile's stores open at
    /// once: whoever holds the store lock shuts the other out until it lets go.
    #[test]
    fn one_holder_of_a_profiles_stores_at_a_time() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let first = StoreLock::try_acquire(&paths).expect("lock").expect("free");
        assert!(
            StoreLock::try_acquire(&paths).expect("lock").is_none(),
            "two holders at once"
        );
        drop(first);
        assert!(
            soon(|| StoreLock::try_acquire(&paths).expect("lock").is_some()),
            "still held after its holder let go"
        );
    }
}
