//! Which local files a tool may send. An MCP client can be steered by any
//! message it reads, so a send-a-file tool must not be able to reach outside
//! the folders the user chose, or one prompt injection mails out an SSH key.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

/// The folders the user lets tools send files from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendRoots(Vec<PathBuf>);

impl SendRoots {
    /// Fails when a root doesn't exist, so a typo in settings is reported
    /// rather than quietly allowing nothing.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<Self> {
        roots
            .into_iter()
            .map(|root| {
                root.canonicalize()
                    .with_context(|| format!("send folder \"{}\" is unreadable", root.display()))
            })
            .collect::<anyhow::Result<_>>()
            .map(Self)
    }

    /// `path` as a file a backend may upload, or why not.
    // NOTE: canonicalize before comparing. A symlink inside a root that points
    // outside it, or a `..` component, must be judged by where it lands.
    pub fn admit(&self, path: &Path) -> anyhow::Result<SendableFile> {
        let real = path
            .canonicalize()
            .with_context(|| format!("\"{}\" is unreadable", path.display()))?;
        if !real.is_file() {
            bail!("\"{}\" is not a file", path.display());
        }
        if !self.0.iter().any(|root| real.starts_with(root)) {
            bail!(
                "\"{}\" is outside the folders files may be sent from; add its folder under Advanced",
                path.display()
            );
        }
        Ok(SendableFile(real))
    }
}

/// A regular file inside a send root. Only `SendRoots::admit` makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SendableFile(PathBuf);

impl SendableFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> (tempfile::TempDir, SendRoots) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("outbox")).expect("outbox");
        std::fs::write(dir.path().join("outbox/cat.png"), b"png").expect("file");
        std::fs::write(dir.path().join("secret"), b"key").expect("file");
        let roots = SendRoots::new([dir.path().join("outbox")]).expect("roots");
        (dir, roots)
    }

    /// A file inside a root is sendable; the escape hatches a path offers
    /// (`..`, a file next to the root, the root itself) are not.
    #[test]
    fn only_files_inside_a_send_root_are_admitted() {
        let (dir, roots) = tree();
        let inside = dir.path().join("outbox/cat.png");
        assert!(roots.admit(&inside).is_ok(), "refused a file in the root");
        for outside in [
            dir.path().join("secret"),
            dir.path().join("outbox/../secret"),
            dir.path().join("outbox"),
            dir.path().join("outbox/missing.png"),
        ] {
            let admitted = roots.admit(&outside);
            assert!(admitted.is_err(), "admitted {outside:?}: {admitted:?}");
        }
    }

    /// A symlink inside a root must be judged by its target, or linking the
    /// key into the outbox would send it.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_a_send_root_is_refused() {
        let (dir, roots) = tree();
        let link = dir.path().join("outbox/link");
        std::os::unix::fs::symlink(dir.path().join("secret"), &link).expect("symlink");
        let admitted = roots.admit(&link);
        assert!(admitted.is_err(), "admitted a symlink out: {admitted:?}");
    }

    /// No roots configured means no file is sendable.
    #[test]
    fn with_no_send_roots_nothing_is_admitted() {
        let (dir, _) = tree();
        let admitted = SendRoots::default().admit(&dir.path().join("outbox/cat.png"));
        assert!(admitted.is_err(), "admitted with no roots: {admitted:?}");
    }
}
