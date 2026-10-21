//! presage behind `SignalBackend` and `Provisioner`. presage's futures aren't
//! `Send` and its receive loop needs a `LocalSet`, so every call runs on one
//! dedicated thread and the trait methods only pass work to it.

use std::{future::Future, path::Path, pin::Pin, thread};

use anyhow::Context;
use presage_store_sqlite::{OnNewIdentity, SqliteConnectOptions, SqliteStore};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// The thread presage runs on. Dropping every handle stops it, and with it
/// every task still running there.
#[derive(Clone)]
struct Worker {
    jobs: mpsc::UnboundedSender<Job>,
}

impl Worker {
    fn spawn() -> anyhow::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("unable to start the Signal worker runtime")?;
        let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
        thread::Builder::new()
            .name("signal".into())
            .spawn(move || {
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, async move {
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn_local(job());
                    }
                });
            })
            .context("unable to start the Signal worker thread")?;
        Ok(Self { jobs })
    }

    /// Runs `job` on the worker and waits for its result.
    async fn run<F, Fut, T>(&self, job: F) -> anyhow::Result<T>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
        T: Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move || {
                Box::pin(async move {
                    // The caller gave up waiting; nobody wants the result.
                    let _ = tx.send(job().await);
                })
            }))
            .ok()
            .context("the Signal worker has stopped")?;
        rx.await.context("the Signal worker dropped a job")
    }
}

/// Opens (creating if needed) presage's store at `path`, encrypted with `key`.
// NOTE: the key goes in as a raw SQLCipher key (x'..'), not a passphrase. It's
// already 32 random bytes, so PBKDF2 would only slow every open; but the form
// is part of the file format, and changing it locks out every existing store.
async fn open_store(path: &Path, key: &[u8; 32]) -> anyhow::Result<SqliteStore> {
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .pragma("key", format!("\"x'{hex}'\""));
    SqliteStore::open_with_options(options, OnNewIdentity::Trust)
        .await
        .with_context(|| format!("unable to open Signal store \"{}\"", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("signal.db");
        (dir, path)
    }

    /// The store must be unreadable without its key. If the SQLCipher build
    /// ever falls back to plain SQLite the key pragma is silently ignored, and
    /// this is the test that notices.
    #[tokio::test]
    async fn store_refuses_to_open_with_the_wrong_key() {
        let (_dir, path) = temp_store();
        let worker = Worker::spawn().expect("worker");
        let open_path = path.clone();
        let store = worker
            .run(move || async move { open_store(&open_path, &[1; 32]).await })
            .await
            .expect("worker ran")
            .expect("first open creates the store");
        worker
            .run(move || async move { store.close().await })
            .await
            .expect("worker ran");
        let wrong = worker
            .run(move || async move { open_store(&path, &[2; 32]).await })
            .await
            .expect("worker ran");
        assert!(
            wrong.is_err(),
            "opened an encrypted store with the wrong key"
        );
    }
}
