//! Opt-in import of Signal Desktop's own history, for people who already run
//! Desktop and want more than the phone's link-time transfer gave them. It only
//! reads Desktop's database; it never writes to it.

use std::{collections::HashMap, path::Path};

use anyhow::Context;
use futures::TryStreamExt as _;
use relay_core::prelude::*;
use relay_store::MessageStore;
use serde_json::Value;
use sqlx::{ConnectOptions as _, sqlite::SqliteConnectOptions};

use crate::{ImportProgress, ImportSummary, desktop_key};

/// Imports the messages in Signal Desktop's database under `signal_dir` (its
/// data directory, holding `config.json` and `sql/db.sqlite`) into `store`.
///
/// Reading the database key may ask the user to allow access to the OS
/// keystore. Messages from people without an ACI are skipped and counted.
pub async fn import_signal_desktop(
    signal_dir: &Path,
    store: &dyn MessageStore,
    _progress: &mut (dyn FnMut(ImportProgress) + Send),
) -> anyhow::Result<ImportSummary> {
    let key = desktop_key::database_key(signal_dir)?;
    let db_path = signal_dir.join("sql").join("db.sqlite");
    let mut conn = SqliteConnectOptions::new()
        .filename(&db_path)
        .read_only(true)
        .pragma("key", key.pragma_value())
        .connect()
        .await
        .with_context(|| format!("unable to open \"{}\"", db_path.display()))?;

    let mut conversations = HashMap::new();
    let mut rows = sqlx::query_as::<_, (String, String)>("SELECT id, json FROM conversations")
        .fetch(&mut conn);
    while let Some((id, json)) = rows
        .try_next()
        .await
        .context("reading Signal Desktop's conversations")?
    {
        if let Some(recipient) = conversation(&parse(&json)?) {
            conversations.insert(id, recipient);
        }
    }
    drop(rows);

    let mut summary = ImportSummary::default();
    let mut rows =
        sqlx::query_as::<_, (Option<String>, String)>("SELECT conversationId, json FROM messages")
            .fetch(&mut conn);
    while let Some((conversation_id, json)) = rows
        .try_next()
        .await
        .context("reading Signal Desktop's messages")?
    {
        let conversation = conversation_id.and_then(|id| conversations.get(&id).copied());
        let Some(message) = conversation.and_then(|c| message(c, &parse(&json).ok()?)) else {
            summary.skipped += 1;
            continue;
        };
        store
            .insert(&message)
            .await
            .with_context(|| format!("storing imported message {:?}", message.id))?;
        summary.imported += 1;
    }
    Ok(summary)
}

fn parse(json: &str) -> anyhow::Result<Value> {
    serde_json::from_str(json).context("Signal Desktop row is not JSON")
}

/// The relay-core conversation a Desktop conversation row stands for.
fn conversation(json: &Value) -> Option<Recipient> {
    match json.get("type")?.as_str()? {
        "private" => Some(Recipient::Contact(service_id(json.get("serviceId")?)?)),
        _ => None,
    }
}

/// A Desktop message row as a relay-core message, if it's one a person sent.
fn message(conversation: Recipient, json: &Value) -> Option<Message> {
    let sent_at_ms = json.get("sent_at")?.as_u64()?;
    let sender = match json.get("type")?.as_str()? {
        "incoming" => service_id(json.get("sourceServiceId")?)?,
        _ => return None,
    };
    Some(Message {
        id: MessageId {
            author: sender,
            sent_at_ms,
        },
        conversation,
        sender,
        from_me: false,
        received_at_ms: json
            .get("received_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(sent_at_ms),
        body: json.get("body").and_then(Value::as_str).map(str::to_owned),
        attachments: vec![],
        quote: None,
        edited: false,
        deleted: false,
    })
}

fn service_id(value: &Value) -> Option<ServiceId> {
    ServiceId::parse(value.as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_store::MemoryStore;
    use serde_json::json;
    use sqlx::{Connection as _, Executor as _};

    const DB_KEY: &str = "3a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac03a0aaac0";
    const OUR_ACI: &str = "00000000-0000-0000-0000-0000000000aa";
    const ALICE_ACI: &str = "00000000-0000-0000-0000-0000000000a1";

    /// A Signal Desktop data directory with a plaintext config key and a
    /// SQLCipher database holding just the columns the importer reads.
    struct Desktop {
        dir: tempfile::TempDir,
        conn: sqlx::SqliteConnection,
    }

    impl Desktop {
        async fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::write(
                dir.path().join("config.json"),
                json!({ "key": DB_KEY }).to_string(),
            )
            .expect("config");
            std::fs::create_dir(dir.path().join("sql")).expect("sql dir");
            let mut conn = SqliteConnectOptions::new()
                .filename(dir.path().join("sql/db.sqlite"))
                .create_if_missing(true)
                .pragma("key", format!("\"x'{DB_KEY}'\""))
                .connect()
                .await
                .expect("create db");
            // NOTE: Desktop's real schema indexes JSON with ->>, which needs
            // SQLite 3.38+; an older SQLite can't even open the file. The
            // generated column keeps that requirement in the test.
            conn.execute(
                "CREATE TABLE items (id TEXT PRIMARY KEY, json TEXT);
                 CREATE TABLE conversations (id TEXT PRIMARY KEY, json TEXT);
                 CREATE TABLE messages (
                     id TEXT PRIMARY KEY,
                     json TEXT,
                     conversationId TEXT,
                     isStory INTEGER GENERATED ALWAYS AS (json ->> '$.type' IS 'story')
                 );",
            )
            .await
            .expect("schema");
            let mut desktop = Self { dir, conn };
            desktop
                .item(
                    "uuid_id",
                    json!({ "id": "uuid_id", "value": format!("{OUR_ACI}.1") }),
                )
                .await;
            desktop
        }

        async fn item(&mut self, id: &str, json: serde_json::Value) {
            sqlx::query("INSERT INTO items (id, json) VALUES (?, ?)")
                .bind(id)
                .bind(json.to_string())
                .execute(&mut self.conn)
                .await
                .expect("insert");
        }

        async fn conversation(&mut self, id: &str, json: serde_json::Value) {
            sqlx::query("INSERT INTO conversations (id, json) VALUES (?, ?)")
                .bind(id)
                .bind(json.to_string())
                .execute(&mut self.conn)
                .await
                .expect("insert");
        }

        async fn message(&mut self, id: &str, conversation_id: &str, json: serde_json::Value) {
            sqlx::query("INSERT INTO messages (id, json, conversationId) VALUES (?, ?, ?)")
                .bind(id)
                .bind(json.to_string())
                .bind(conversation_id)
                .execute(&mut self.conn)
                .await
                .expect("insert message");
        }

        async fn import(self) -> (MemoryStore, ImportSummary) {
            self.conn.close().await.expect("close");
            let store = MemoryStore::default();
            let summary = import_signal_desktop(self.dir.path(), &store, &mut |_| {})
                .await
                .expect("import");
            (store, summary)
        }
    }

    fn aci(s: &str) -> ServiceId {
        ServiceId::parse(s).expect("aci")
    }

    async fn all_messages(store: &MemoryStore) -> Vec<Message> {
        let mut out = vec![];
        for c in store.conversations().await.expect("conversations") {
            out.extend(
                store
                    .history(c.recipient, None, u32::MAX)
                    .await
                    .expect("history"),
            );
        }
        out
    }

    /// The basic promise: a text in Desktop's encrypted database comes out in
    /// the sender's conversation with its timestamps, through the raw hex key.
    #[tokio::test]
    async fn an_incoming_text_lands_in_the_senders_conversation() {
        let mut desktop = Desktop::new().await;
        desktop
            .conversation(
                "c-alice",
                json!({ "type": "private", "serviceId": ALICE_ACI }),
            )
            .await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({
                    "type": "incoming",
                    "sent_at": 1_000,
                    "received_at_ms": 1_005,
                    "sourceServiceId": ALICE_ACI,
                    "body": "from desktop",
                }),
            )
            .await;
        let (store, summary) = desktop.import().await;
        assert_eq!(summary.imported, 1, "{summary:?}");
        let expected = Message {
            id: MessageId {
                author: aci(ALICE_ACI),
                sent_at_ms: 1_000,
            },
            conversation: Recipient::Contact(aci(ALICE_ACI)),
            sender: aci(ALICE_ACI),
            from_me: false,
            received_at_ms: 1_005,
            body: Some("from desktop".to_owned()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        };
        assert_eq!(all_messages(&store).await, vec![expected]);
    }
}
