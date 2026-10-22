//! The real message store: one SQLCipher database per profile, so history at
//! rest is unreadable without the key held in the OS secret store.

use std::{path::Path, str::FromStr};

use anyhow::{Context, bail};
use async_trait::async_trait;
use log::LevelFilter;
use relay_core::prelude::*;
use relay_secrets::StoreKey;
use sqlx::{
    ConnectOptions, FromRow,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions},
};

use crate::{MessageStore, SearchQuery, StoreStats, ensure_author};

/// A profile's encrypted message history.
pub struct SqlCipherStore {
    pool: SqlitePool,
}

impl SqlCipherStore {
    /// Opens or creates the store at `path`, failing if `key` doesn't decrypt it.
    pub async fn open(path: &Path, key: &StoreKey) -> anyhow::Result<Self> {
        let hex: String = key.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        // NOTE: sqlx logs every statement it runs, including the PRAGMA key it
        // sends on connect, and logs slow ones at WARN. Leaving either on writes
        // the database key into the agent log.
        let options = SqliteConnectOptions::from_str("sqlite:")?
            .filename(path)
            .pragma("key", format!("\"x'{hex}'\""))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .log_statements(LevelFilter::Off)
            .log_slow_statements(LevelFilter::Off, std::time::Duration::ZERO);
        let pool = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .with_context(|| {
                format!(
                    "unable to open message store \"{}\": wrong key or not a message store",
                    path.display()
                )
            })?;
        sqlx::migrate!()
            .run(&pool)
            .await
            .with_context(|| format!("unable to migrate message store \"{}\"", path.display()))?;
        Ok(Self { pool })
    }

    async fn set_read(&self, targets: &[MessageId], read: bool) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for target in targets {
            sqlx::query("UPDATE messages SET read = ? WHERE author = ? AND sent_at_ms = ?")
                .bind(read)
                .bind(target.author.to_string())
                .bind(to_i64(target.sent_at_ms)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// A `SELECT` of every `MessageRow` column from `messages m`, followed by
/// `$rest`. A macro rather than `format!` because sqlx only accepts static SQL.
macro_rules! select_messages {
    ($rest:literal) => {
        concat!(
            "SELECT m.author, m.sent_at_ms, m.conversation, m.sender, m.from_me, ",
            "m.received_at_ms, m.body, m.attachments, m.quote_author, m.quote_sent_at_ms, ",
            "m.edited, m.deleted ",
            $rest
        )
    };
}

/// A `messages` row as stored; converted to a `Message` at the boundary so a
/// corrupt row fails loudly instead of producing a half-valid message.
#[derive(FromRow)]
struct MessageRow {
    author: String,
    sent_at_ms: i64,
    conversation: String,
    sender: String,
    from_me: bool,
    received_at_ms: i64,
    body: Option<String>,
    attachments: String,
    quote_author: Option<String>,
    quote_sent_at_ms: Option<i64>,
    edited: bool,
    deleted: bool,
}

impl TryFrom<MessageRow> for Message {
    type Error = anyhow::Error;

    fn try_from(row: MessageRow) -> anyhow::Result<Self> {
        let quote = match (row.quote_author, row.quote_sent_at_ms) {
            (Some(author), Some(sent_at_ms)) => Some(MessageId {
                author: ServiceId::parse(&author)?,
                sent_at_ms: to_u64(sent_at_ms)?,
            }),
            _ => None,
        };
        Ok(Message {
            id: MessageId {
                author: ServiceId::parse(&row.author)?,
                sent_at_ms: to_u64(row.sent_at_ms)?,
            },
            conversation: parse_recipient(&row.conversation)?,
            sender: ServiceId::parse(&row.sender)?,
            from_me: row.from_me,
            received_at_ms: to_u64(row.received_at_ms)?,
            body: row.body,
            attachments: serde_json::from_str(&row.attachments)
                .with_context(|| format!("corrupt attachments column {:?}", row.attachments))?,
            quote,
            edited: row.edited,
            deleted: row.deleted,
        })
    }
}

fn to_u64(n: i64) -> anyhow::Result<u64> {
    u64::try_from(n).with_context(|| format!("negative timestamp {n} in message store"))
}

fn to_i64(n: u64) -> anyhow::Result<i64> {
    i64::try_from(n).with_context(|| format!("timestamp {n} too large for the message store"))
}

fn recipient_key(recipient: Recipient) -> String {
    match recipient {
        Recipient::Contact(id) => format!("contact:{id}"),
        Recipient::Group(id) => format!("group:{id}"),
    }
}

fn parse_recipient(key: &str) -> anyhow::Result<Recipient> {
    if let Some(id) = key.strip_prefix("contact:") {
        return Ok(Recipient::Contact(ServiceId::parse(id)?));
    }
    let Some(id) = key.strip_prefix("group:") else {
        bail!("corrupt conversation column \"{key}\"");
    };
    Ok(Recipient::Group(GroupId::parse(id)?))
}

fn ensure_found(rows_affected: u64, target: MessageId) -> anyhow::Result<()> {
    if rows_affected == 0 {
        bail!("no stored message {target:?}");
    }
    Ok(())
}

/// Free text as an FTS5 query: each word must appear, as a word or a word
/// prefix. Only letters and digits reach FTS, so user input can't inject
/// query syntax.
fn fts_query(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{word}\"*"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn to_messages(rows: Vec<MessageRow>) -> anyhow::Result<Vec<Message>> {
    rows.into_iter().map(Message::try_from).collect()
}

#[async_trait]
impl MessageStore for SqlCipherStore {
    async fn insert(&self, message: &Message) -> anyhow::Result<bool> {
        let inserted = sqlx::query(
            "INSERT INTO messages (author, sent_at_ms, conversation, sender, from_me, \
             received_at_ms, body, attachments, quote_author, quote_sent_at_ms, edited, deleted) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (author, sent_at_ms) DO NOTHING",
        )
        .bind(message.id.author.to_string())
        .bind(to_i64(message.id.sent_at_ms)?)
        .bind(recipient_key(message.conversation))
        .bind(message.sender.to_string())
        .bind(message.from_me)
        .bind(to_i64(message.received_at_ms)?)
        .bind(message.body.as_deref())
        .bind(serde_json::to_string(&message.attachments)?)
        .bind(message.quote.map(|q| q.author.to_string()))
        .bind(message.quote.map(|q| to_i64(q.sent_at_ms)).transpose()?)
        .bind(message.edited)
        .bind(message.deleted)
        .execute(&self.pool)
        .await?;
        Ok(inserted.rows_affected() == 1)
    }

    async fn apply_edit(
        &self,
        target: MessageId,
        editor: ServiceId,
        body: &str,
    ) -> anyhow::Result<()> {
        ensure_author(target, editor, "edit")?;
        let updated = sqlx::query(
            "UPDATE messages SET body = ?, edited = 1 WHERE author = ? AND sent_at_ms = ?",
        )
        .bind(body)
        .bind(target.author.to_string())
        .bind(to_i64(target.sent_at_ms)?)
        .execute(&self.pool)
        .await?;
        ensure_found(updated.rows_affected(), target)
    }

    async fn mark_deleted(&self, target: MessageId, deleter: ServiceId) -> anyhow::Result<()> {
        ensure_author(target, deleter, "delete")?;
        let updated =
            sqlx::query("UPDATE messages SET deleted = 1 WHERE author = ? AND sent_at_ms = ?")
                .bind(target.author.to_string())
                .bind(to_i64(target.sent_at_ms)?)
                .execute(&self.pool)
                .await?;
        ensure_found(updated.rows_affected(), target)
    }

    async fn mark_read(&self, targets: &[MessageId]) -> anyhow::Result<()> {
        self.set_read(targets, true).await
    }

    async fn mark_unread(&self, targets: &[MessageId]) -> anyhow::Result<()> {
        self.set_read(targets, false).await
    }

    async fn set_title(&self, conversation: Recipient, title: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO titles (conversation, title) VALUES (?, ?) \
             ON CONFLICT (conversation) DO UPDATE SET title = excluded.title",
        )
        .bind(recipient_key(conversation))
        .bind(title)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: MessageId) -> anyhow::Result<Option<Message>> {
        let row: Option<MessageRow> = sqlx::query_as(select_messages!(
            "FROM messages m WHERE m.author = ? AND m.sent_at_ms = ?"
        ))
        .bind(id.author.to_string())
        .bind(to_i64(id.sent_at_ms)?)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Message::try_from).transpose()
    }

    async fn conversations(&self) -> anyhow::Result<Vec<Conversation>> {
        let rows: Vec<(String, Option<String>, i64, i64)> = sqlx::query_as(
            "SELECT m.conversation, t.title, max(m.sent_at_ms), \
             sum(m.from_me = 0 AND m.read = 0) \
             FROM messages m LEFT JOIN titles t ON t.conversation = m.conversation \
             GROUP BY m.conversation ORDER BY max(m.sent_at_ms) DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(conversation, title, last, unread)| {
                Ok(Conversation {
                    recipient: parse_recipient(&conversation)?,
                    title: title.unwrap_or_default(),
                    last_message_at_ms: Some(to_u64(last)?),
                    unread: u32::try_from(unread)
                        .with_context(|| format!("unread count {unread} out of range"))?,
                })
            })
            .collect()
    }

    async fn history(
        &self,
        conversation: Recipient,
        before_ms: Option<u64>,
        limit: u32,
    ) -> anyhow::Result<Vec<Message>> {
        let rows = sqlx::query_as(select_messages!(
            "FROM messages m WHERE m.conversation = ? AND m.sent_at_ms < ? ORDER BY m.sent_at_ms DESC LIMIT ?"
        ))
        .bind(recipient_key(conversation))
        .bind(before_ms.map(to_i64).transpose()?.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        to_messages(rows)
    }

    async fn search(&self, query: &SearchQuery) -> anyhow::Result<Vec<Message>> {
        let words = fts_query(&query.text);
        // Text with no words in it is a filter-only listing, newest first; FTS5
        // rejects an empty MATCH. Both bind the same parameters, so ?1 is checked
        // rather than left unused.
        let sql = if words.is_empty() {
            select_messages!(
                "FROM messages m WHERE ?1 = '' AND m.body IS NOT NULL AND m.deleted = 0 \
                 AND (?2 IS NULL OR m.sender = ?2) \
                 AND (?3 IS NULL OR m.conversation = ?3) \
                 AND m.sent_at_ms >= ?4 AND m.sent_at_ms < ?5 \
                 ORDER BY m.sent_at_ms DESC LIMIT ?6 OFFSET ?7"
            )
        } else {
            select_messages!(
                "FROM messages_fts JOIN messages m ON m.id = messages_fts.rowid \
                 WHERE messages_fts MATCH ?1 AND m.deleted = 0 \
                 AND (?2 IS NULL OR m.sender = ?2) \
                 AND (?3 IS NULL OR m.conversation = ?3) \
                 AND m.sent_at_ms >= ?4 AND m.sent_at_ms < ?5 \
                 ORDER BY rank, m.sent_at_ms DESC LIMIT ?6 OFFSET ?7"
            )
        };
        let rows = sqlx::query_as(sql)
            .bind(words)
            .bind(query.sender.map(|s| s.to_string()))
            .bind(query.conversation.map(recipient_key))
            .bind(query.since_ms.map(to_i64).transpose()?.unwrap_or(0))
            .bind(query.until_ms.map(to_i64).transpose()?.unwrap_or(i64::MAX))
            .bind(query.limit)
            .bind(query.offset)
            .fetch_all(&self.pool)
            .await?;
        to_messages(rows)
    }

    async fn unread(&self) -> anyhow::Result<Vec<Message>> {
        let rows = sqlx::query_as(select_messages!(
            "FROM messages m WHERE m.from_me = 0 AND m.read = 0 ORDER BY m.sent_at_ms"
        ))
        .fetch_all(&self.pool)
        .await?;
        to_messages(rows)
    }

    async fn prune(&self, older_than_ms: u64) -> anyhow::Result<u64> {
        let deleted = sqlx::query("DELETE FROM messages WHERE sent_at_ms < ?")
            .bind(to_i64(older_than_ms)?)
            .execute(&self.pool)
            .await?;
        Ok(deleted.rows_affected())
    }

    async fn export(
        &self,
        conversation: Option<Recipient>,
        since_ms: Option<u64>,
    ) -> anyhow::Result<Vec<Message>> {
        let rows = sqlx::query_as(select_messages!(
            "FROM messages m WHERE (?1 IS NULL OR m.conversation = ?1) \
             AND m.sent_at_ms >= ?2 ORDER BY m.sent_at_ms"
        ))
        .bind(conversation.map(recipient_key))
        .bind(since_ms.map(to_i64).transpose()?.unwrap_or(0))
        .fetch_all(&self.pool)
        .await?;
        to_messages(rows)
    }

    // NOTE: dropping the pool without this leaves sqlx worker threads closing
    // connections while the process tears down SQLCipher's crypto state, which
    // segfaults in sqlite3FreeCodecArg on exit.
    async fn close(&self) -> anyhow::Result<()> {
        self.pool.close().await;
        Ok(())
    }

    async fn stats(&self) -> anyhow::Result<StoreStats> {
        let (messages, conversations, unread, oldest, newest): (
            i64,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT count(*), count(DISTINCT conversation), \
             coalesce(sum(from_me = 0 AND read = 0), 0), min(sent_at_ms), max(sent_at_ms) \
             FROM messages",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StoreStats {
            messages: to_u64(messages)?,
            conversations: to_u64(conversations)?,
            unread: to_u64(unread)?,
            oldest_ms: oldest.map(to_u64).transpose()?,
            newest_ms: newest.map(to_u64).transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A copied store file must be useless without its key: opening it with
    /// any other key has to fail instead of returning an empty history.
    #[tokio::test]
    async fn opening_with_the_wrong_key_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("messages.db");
        let store = SqlCipherStore::open(&path, &StoreKey::from_bytes([1; 32]))
            .await
            .expect("create");
        store.close().await.expect("close");
        let reopened = SqlCipherStore::open(&path, &StoreKey::from_bytes([2; 32])).await;
        assert!(reopened.is_err(), "opened with the wrong key");
    }

    fn sid(n: u128) -> ServiceId {
        ServiceId::new(uuid::Uuid::from_u128(n))
    }

    fn msg(sent_at_ms: u64, body: &str) -> Message {
        Message {
            id: MessageId {
                author: sid(1),
                sent_at_ms,
            },
            conversation: Recipient::Contact(sid(1)),
            sender: sid(1),
            from_me: false,
            received_at_ms: sent_at_ms,
            body: Some(body.to_owned()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        }
    }

    async fn open_temp() -> (SqlCipherStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SqlCipherStore::open(
            &dir.path().join("messages.db"),
            &StoreKey::from_bytes([3; 32]),
        )
        .await
        .expect("open");
        (store, dir)
    }

    /// Search must rank by relevance, not recency: a message about the term
    /// beats a newer one that mentions it in passing.
    #[tokio::test]
    async fn search_ranks_best_match_first() {
        let (store, _dir) = open_temp().await;
        store
            .insert(&msg(1, "rehearsal rehearsal schedule"))
            .await
            .expect("insert");
        store
            .insert(&msg(
                2,
                "long note about groceries, cats, the vet, the car and one rehearsal",
            ))
            .await
            .expect("insert");
        let hits = store
            .search(&SearchQuery {
                text: "rehearsal".into(),
                limit: 10,
                ..Default::default()
            })
            .await
            .expect("search");
        let times: Vec<_> = hits.iter().map(|m| m.id.sent_at_ms).collect();
        assert_eq!(times, vec![1, 2], "ranked by recency, not relevance");
        store.close().await.expect("close");
    }

    /// Nothing written to disk, including the WAL and the FTS index, may
    /// contain a message body in the clear.
    #[tokio::test]
    async fn no_plaintext_bodies_reach_the_disk() {
        let (store, dir) = open_temp().await;
        let body = "zanzibar-heron-quartz";
        store.insert(&msg(1, body)).await.expect("insert");
        let hits = store
            .search(&SearchQuery {
                text: body.into(),
                limit: 1,
                ..Default::default()
            })
            .await
            .expect("search");
        assert_eq!(hits.len(), 1, "body not indexed: {hits:?}");
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir.path()).expect("list store dir") {
            let path = entry.expect("dir entry").path();
            let bytes = std::fs::read(&path).expect("read store file");
            files.push(path.display().to_string());
            let leaked = bytes.windows(body.len()).any(|w| w == body.as_bytes());
            assert!(!leaked, "plaintext body found in {}", path.display());
            assert!(
                !bytes.starts_with(b"SQLite format 3"),
                "unencrypted SQLite header in {}",
                path.display()
            );
        }
        assert!(
            files.iter().any(|f| f.ends_with("-wal")),
            "expected a WAL file to check too: {files:?}"
        );
        store.close().await.expect("close");
    }

    /// Search depends on FTS5 being compiled into the bundled SQLCipher.
    #[tokio::test]
    async fn fts5_is_compiled_in() {
        let (store, _dir) = open_temp().await;
        let (used,): (i64,) = sqlx::query_as("SELECT sqlite_compileoption_used('ENABLE_FTS5')")
            .fetch_one(&store.pool)
            .await
            .expect("query");
        assert_eq!(used, 1, "SQLCipher built without FTS5");
        store.close().await.expect("close");
    }

    /// History, unread and get-by-id run on every tool call that touches
    /// messages, so they must use an index rather than scan every message the
    /// profile ever received.
    #[tokio::test]
    async fn history_and_unread_use_indices() {
        let (store, _dir) = open_temp().await;
        for sql in [
            "EXPLAIN QUERY PLAN SELECT * FROM messages \
             WHERE conversation = 'x' AND sent_at_ms < 5 ORDER BY sent_at_ms DESC",
            "EXPLAIN QUERY PLAN SELECT * FROM messages \
             WHERE from_me = 0 AND read = 0 ORDER BY sent_at_ms",
            "EXPLAIN QUERY PLAN SELECT * FROM messages WHERE author = 'a' AND sent_at_ms = 1",
        ] {
            let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(sql)
                .fetch_all(&store.pool)
                .await
                .expect("explain");
            let details: Vec<_> = plan.into_iter().map(|row| row.3).collect();
            assert!(
                details.iter().all(|d| d != "SCAN messages"),
                "full scan for {sql}: {details:?}"
            );
        }
        store.close().await.expect("close");
    }

    /// The cipher backend must be the one docs/slices.md declares: pure-Rust
    /// crypto, except on Windows, where the fork's build.rs lets the
    /// vendored-OpenSSL feature win over custom crypto. Either backend writes
    /// the standard SQLCipher format, so stores stay portable across OSes.
    #[tokio::test]
    async fn sqlcipher_uses_the_declared_crypto_backend() {
        let (store, _dir) = open_temp().await;
        let (provider,): (String,) = sqlx::query_as("PRAGMA cipher_provider")
            .fetch_one(&store.pool)
            .await
            .expect("query");
        let arm_linux = cfg!(all(
            target_os = "linux",
            any(target_arch = "aarch64", target_arch = "arm")
        ));
        let expected = if cfg!(windows) || arm_linux {
            "openssl"
        } else {
            "rustcrypto"
        };
        assert_eq!(provider.to_lowercase(), expected, "cipher provider");
        store.close().await.expect("close");
    }
}
