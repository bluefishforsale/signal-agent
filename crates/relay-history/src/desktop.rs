//! Opt-in import of Signal Desktop's own history, for people who already run
//! Desktop and want more than the phone's link-time transfer gave them. It only
//! reads Desktop's database; it never writes to it.

use std::{collections::HashMap, path::Path};

use anyhow::Context;
use base64::Engine as _;
use futures::TryStreamExt as _;
use relay_core::prelude::*;
use relay_store::MessageStore;
use serde_json::Value;
use sqlx::{ConnectOptions as _, sqlite::SqliteConnectOptions};

use crate::{ImportProgress, ImportSummary, desktop_key, full_name};

/// Imports the messages in Signal Desktop's database under `signal_dir` (its
/// data directory, holding `config.json` and `sql/db.sqlite`) into `store`.
///
/// Reading the database key may ask the user to allow access to the OS
/// keystore. Messages from people without an ACI are skipped and counted.
pub async fn import_signal_desktop(
    signal_dir: &Path,
    store: &dyn MessageStore,
    progress: &mut (dyn FnMut(ImportProgress) + Send),
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

    let (our_aci,) = sqlx::query_as::<_, (String,)>("SELECT json FROM items WHERE id = 'uuid_id'")
        .fetch_optional(&mut conn)
        .await
        .context("reading Signal Desktop's account")?
        .context("Signal Desktop isn't linked to an account")?;
    let our_aci = parse(&our_aci)?
        .get("value")
        .and_then(Value::as_str)
        .and_then(|v| v.split('.').next())
        .and_then(|aci| ServiceId::parse(aci).ok())
        .context("Signal Desktop's account has no ACI")?;

    let mut conversations = HashMap::new();
    let mut titles = Vec::new();
    let mut rows = sqlx::query_as::<_, (String, String)>("SELECT id, json FROM conversations")
        .fetch(&mut conn);
    while let Some((id, json)) = rows
        .try_next()
        .await
        .context("reading Signal Desktop's conversations")?
    {
        let json = parse(&json)?;
        let Some(recipient) = conversation(&json) else {
            continue;
        };
        conversations.insert(id, recipient);
        if let Some(title) = title(&json) {
            titles.push((recipient, title));
        }
    }
    drop(rows);
    for (recipient, title) in titles {
        store
            .set_title(recipient, &title)
            .await
            .with_context(|| format!("titling imported conversation {recipient:?}"))?;
    }

    let mut summary = ImportSummary::default();
    let mut read = Vec::new();
    let mut rows =
        sqlx::query_as::<_, (Option<String>, String)>("SELECT conversationId, json FROM messages")
            .fetch(&mut conn);
    while let Some((conversation_id, json)) = rows
        .try_next()
        .await
        .context("reading Signal Desktop's messages")?
    {
        let conversation = conversation_id.and_then(|id| conversations.get(&id).copied());
        let json = parse(&json)?;
        let Some(message) = conversation.and_then(|c| message(our_aci, c, &json)) else {
            summary.skipped += 1;
            continue;
        };
        if json.get("readStatus").and_then(Value::as_u64) != Some(UNREAD) {
            read.push(message.id);
        }
        let new = store
            .insert(&message)
            .await
            .with_context(|| format!("storing imported message {:?}", message.id))?;
        if !new {
            summary.already_stored += 1;
            continue;
        }
        summary.imported += 1;
        progress(ImportProgress {
            imported: summary.imported,
            total: None,
        });
    }
    drop(rows);
    store
        .mark_read(&read)
        .await
        .context("marking imported messages read")?;
    Ok(summary)
}

/// Desktop's ReadStatus.Unread; Read is 0 and Viewed 2.
const UNREAD: u64 = 1;

fn parse(json: &str) -> anyhow::Result<Value> {
    serde_json::from_str(json).context("Signal Desktop row is not JSON")
}

/// The relay-core conversation a Desktop conversation row stands for.
fn conversation(json: &Value) -> Option<Recipient> {
    match json.get("type")?.as_str()? {
        "private" => Some(Recipient::Contact(service_id(json.get("serviceId")?)?)),
        // Legacy v1 groups have 16-byte ids and no derived identifier; they
        // can't take messages any more, so they're left out.
        "group" => {
            let id = base64::engine::general_purpose::STANDARD
                .decode(json.get("groupId")?.as_str()?)
                .ok()?;
            Some(Recipient::Group(GroupId::new(id.try_into().ok()?)))
        }
        _ => None,
    }
}

/// The name Desktop shows for a conversation: a group's name, or for a
/// contact the user's nickname, then their address book name, then the
/// contact's profile name.
fn title(json: &Value) -> Option<String> {
    let field = |key: &str| json.get(key).and_then(Value::as_str).unwrap_or_default();
    [
        full_name([field("nicknameGivenName"), field("nicknameFamilyName")]),
        full_name([field("name"), ""]),
        full_name([field("profileName"), field("profileFamilyName")]),
    ]
    .into_iter()
    .flatten()
    .next()
}

/// A Desktop message row as a relay-core message, if it's one a person sent.
fn message(our_aci: ServiceId, conversation: Recipient, json: &Value) -> Option<Message> {
    let sent_at_ms = json.get("sent_at")?.as_u64()?;
    let sender = match json.get("type")?.as_str()? {
        "incoming" => service_id(json.get("sourceServiceId")?)?,
        "outgoing" => our_aci,
        _ => return None,
    };
    let deleted = json.get("deletedForEveryone").and_then(Value::as_bool) == Some(true);
    Some(Message {
        id: MessageId {
            author: sender,
            sent_at_ms,
        },
        conversation,
        sender,
        from_me: sender == our_aci,
        received_at_ms: json
            .get("received_at_ms")
            .and_then(Value::as_u64)
            .unwrap_or(sent_at_ms),
        body: json
            .get("body")
            .and_then(Value::as_str)
            .filter(|_| !deleted)
            .map(str::to_owned),
        attachments: vec![],
        quote: json.get("quote").and_then(|q| {
            Some(MessageId {
                author: service_id(q.get("authorAci")?)?,
                sent_at_ms: q.get("id")?.as_u64()?,
            })
        }),
        edited: json
            .get("editHistory")
            .and_then(Value::as_array)
            .is_some_and(|history| !history.is_empty()),
        deleted,
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

    async fn with_alice() -> Desktop {
        let mut desktop = Desktop::new().await;
        desktop
            .conversation(
                "c-alice",
                json!({ "type": "private", "serviceId": ALICE_ACI }),
            )
            .await;
        desktop
    }

    /// Desktop doesn't store an author on what we sent; it's us, and our ACI
    /// is in Desktop's items table.
    #[tokio::test]
    async fn an_outgoing_text_is_attributed_to_our_aci() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({ "type": "outgoing", "sent_at": 2_000, "body": "sent from desktop" }),
            )
            .await;
        let (store, _) = desktop.import().await;
        let stored = all_messages(&store).await;
        let [m] = stored.as_slice() else {
            panic!("expected one message, got {stored:?}");
        };
        assert_eq!(
            (m.id.author, m.sender, m.from_me, m.conversation),
            (
                aci(OUR_ACI),
                aci(OUR_ACI),
                true,
                Recipient::Contact(aci(ALICE_ACI))
            ),
            "{m:?}"
        );
    }

    /// Desktop stores a group's derived identifier (base64), the same id
    /// link-and-sync and live messages use, so all three agree on the group.
    #[tokio::test]
    async fn group_messages_use_the_same_group_id_as_link_and_sync() {
        use zkgroup::groups::{GroupMasterKey, GroupSecretParams};
        let identifier = GroupSecretParams::derive_from_master_key(GroupMasterKey::new([9; 32]))
            .get_group_identifier();
        let mut desktop = Desktop::new().await;
        desktop
            .conversation(
                "c-group",
                json!({
                    "type": "group",
                    "groupVersion": 2,
                    "groupId": base64::engine::general_purpose::STANDARD.encode(identifier),
                }),
            )
            .await;
        desktop
            .message(
                "m1",
                "c-group",
                json!({
                    "type": "incoming",
                    "sent_at": 3_000,
                    "sourceServiceId": ALICE_ACI,
                    "body": "group hello",
                }),
            )
            .await;
        let (store, _) = desktop.import().await;
        let conversations: Vec<_> = all_messages(&store)
            .await
            .iter()
            .map(|m| m.conversation)
            .collect();
        assert_eq!(
            conversations,
            vec![Recipient::Group(GroupId::new(identifier))]
        );
    }

    /// Only what was unread in Desktop stays unread (Desktop's readStatus:
    /// 1 unread, 0 read, 2 viewed).
    #[tokio::test]
    async fn messages_read_in_desktop_import_as_read() {
        let mut desktop = with_alice().await;
        for (id, sent_at, status, body) in [
            ("m1", 1_000, 0, "read"),
            ("m2", 2_000, 1, "unread"),
            ("m3", 3_000, 2, "viewed"),
        ] {
            desktop
                .message(
                    id,
                    "c-alice",
                    json!({
                        "type": "incoming",
                        "sent_at": sent_at,
                        "sourceServiceId": ALICE_ACI,
                        "readStatus": status,
                        "body": body,
                    }),
                )
                .await;
        }
        let (store, _) = desktop.import().await;
        let unread = store.unread().await.expect("unread");
        let bodies: Vec<_> = unread.iter().filter_map(|m| m.body.as_deref()).collect();
        assert_eq!(bodies, vec!["unread"]);
    }

    async fn only_message(desktop: Desktop) -> Message {
        let (store, _) = desktop.import().await;
        let stored = all_messages(&store).await;
        let [m] = stored.as_slice() else {
            panic!("expected one message, got {stored:?}");
        };
        m.clone()
    }

    /// Deleted for everyone in Desktop means deleted here, with no body left
    /// to search.
    #[tokio::test]
    async fn a_message_deleted_for_everyone_imports_as_deleted() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({
                    "type": "incoming",
                    "sent_at": 1_000,
                    "sourceServiceId": ALICE_ACI,
                    "deletedForEveryone": true,
                    "body": "",
                }),
            )
            .await;
        let m = only_message(desktop).await;
        assert!(m.deleted && m.body.is_none(), "{m:?}");
    }

    /// Desktop keeps an edited message's original sent_at and latest body, with
    /// every version in editHistory; it imports flagged as edited.
    #[tokio::test]
    async fn an_edited_message_imports_flagged_as_edited() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({
                    "type": "incoming",
                    "sent_at": 1_000,
                    "sourceServiceId": ALICE_ACI,
                    "body": "first",
                    "editMessageTimestamp": 1_500,
                    "editHistory": [
                        { "body": "first", "timestamp": 1_500 },
                        { "body": "frist", "timestamp": 1_000 },
                    ],
                }),
            )
            .await;
        let m = only_message(desktop).await;
        assert_eq!(
            (m.id.sent_at_ms, m.body.as_deref(), m.edited),
            (1_000, Some("first"), true),
            "{m:?}"
        );
    }

    /// A Desktop reply keeps its reference to the quoted message.
    #[tokio::test]
    async fn a_reply_keeps_a_reference_to_the_quoted_message() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({
                    "type": "outgoing",
                    "sent_at": 2_000,
                    "body": "agreed",
                    "quote": { "id": 1_000, "authorAci": ALICE_ACI, "isViewOnce": false },
                }),
            )
            .await;
        let m = only_message(desktop).await;
        assert_eq!(
            m.quote,
            Some(MessageId {
                author: aci(ALICE_ACI),
                sent_at_ms: 1_000
            })
        );
    }

    /// Imported conversations get Desktop's names: the user's nickname, then
    /// their address book name, then the contact's profile name; a group's name.
    #[tokio::test]
    async fn conversations_are_titled_with_the_name_desktop_shows() {
        let mut desktop = Desktop::new().await;
        let contacts = [
            (
                "c1",
                "00000000-0000-0000-0000-0000000000b1",
                json!({ "nicknameGivenName": "Mom", "name": "Jane Doe", "profileName": "Jane" }),
            ),
            (
                "c2",
                "00000000-0000-0000-0000-0000000000b2",
                json!({ "name": "Dad", "profileName": "John" }),
            ),
            (
                "c3",
                "00000000-0000-0000-0000-0000000000b3",
                json!({ "profileName": "Alice", "profileFamilyName": "Liddell" }),
            ),
        ];
        for (id, aci, mut names) in contacts {
            names["type"] = json!("private");
            names["serviceId"] = json!(aci);
            desktop.conversation(id, names).await;
            desktop
                .message(
                    &format!("m-{id}"),
                    id,
                    json!({ "type": "incoming", "sent_at": 1_000, "sourceServiceId": aci, "body": "hi" }),
                )
                .await;
        }
        desktop
            .conversation(
                "c-group",
                json!({ "type": "group", "groupId": "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk=", "name": "Book club" }),
            )
            .await;
        desktop
            .message(
                "m-group",
                "c-group",
                json!({ "type": "outgoing", "sent_at": 2_000, "body": "chapter 3?" }),
            )
            .await;
        let (store, _) = desktop.import().await;
        let mut titles: Vec<_> = store
            .conversations()
            .await
            .expect("conversations")
            .into_iter()
            .map(|c| c.title)
            .collect();
        titles.sort();
        assert_eq!(titles, vec!["Alice Liddell", "Book club", "Dad", "Mom"]);
    }

    /// Desktop's messages table also holds events (group changes, safety number
    /// changes, calls); those aren't messages and must not be stored as them.
    #[tokio::test]
    async fn desktop_events_are_not_stored_as_messages() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({ "type": "keychange", "sent_at": 1_000, "sourceServiceId": ALICE_ACI }),
            )
            .await;
        let (store, summary) = desktop.import().await;
        let stored = all_messages(&store).await;
        assert!(stored.is_empty(), "stored an event: {stored:?}");
        assert_eq!(summary.skipped, 1, "{summary:?}");
    }

    /// Setup shows progress; it must advance with every imported message.
    #[tokio::test]
    async fn progress_counts_every_imported_message() {
        let mut desktop = with_alice().await;
        for (id, sent_at) in [("m1", 1_000), ("m2", 2_000)] {
            desktop
                .message(
                    id,
                    "c-alice",
                    json!({ "type": "incoming", "sent_at": sent_at, "sourceServiceId": ALICE_ACI, "body": "x" }),
                )
                .await;
        }
        desktop.conn.close().await.expect("close");
        let store = MemoryStore::default();
        let mut seen = vec![];
        import_signal_desktop(desktop.dir.path(), &store, &mut |p| seen.push(p.imported))
            .await
            .expect("import");
        assert_eq!(seen, vec![1, 2]);
    }

    /// Running the Desktop import again (or after link-and-sync brought the
    /// same messages) must not count them as new.
    #[tokio::test]
    async fn importing_twice_counts_the_second_pass_as_already_stored() {
        let mut desktop = with_alice().await;
        desktop
            .message(
                "m1",
                "c-alice",
                json!({ "type": "incoming", "sent_at": 1_000, "sourceServiceId": ALICE_ACI, "body": "once" }),
            )
            .await;
        desktop.conn.close().await.expect("close");
        let store = MemoryStore::default();
        let mut summaries = vec![];
        for _ in 0..2 {
            summaries.push(
                import_signal_desktop(desktop.dir.path(), &store, &mut |_| {})
                    .await
                    .expect("import"),
            );
        }
        assert_eq!(
            summaries[1],
            ImportSummary {
                imported: 0,
                skipped: 0,
                already_stored: 1
            }
        );
    }

    /// The agent runs imports on a multithreaded runtime, so the future must
    /// be Send.
    #[test]
    fn the_import_future_is_send() {
        fn assert_send<T: Send>(_: T) {}
        let store = MemoryStore::default();
        let mut progress = |_| {};
        assert_send(import_signal_desktop(Path::new("."), &store, &mut progress));
    }

    /// Copies the fixture made by the stock `sqlcipher` CLI (SQLCipher 4.19,
    /// `PRAGMA key = "x'<DB_KEY>'"`, Desktop's raw-key form) into a temp dir.
    fn stock_sqlcipher_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testdata/desktop");
        std::fs::create_dir(dir.path().join("sql")).expect("sql dir");
        for file in ["config.json", "sql/db.sqlite"] {
            std::fs::copy(src.join(file), dir.path().join(file)).expect("copy fixture");
        }
        dir
    }

    /// Our own fixtures are written by the same SQLCipher build that reads
    /// them, so a mismatch in key form or cipher settings would cancel out. A
    /// database written by an independent SQLCipher 4 with Desktop's raw key
    /// form must open too.
    #[tokio::test]
    async fn a_database_written_by_stock_sqlcipher_opens() {
        let dir = stock_sqlcipher_fixture();
        let store = MemoryStore::default();
        let summary = import_signal_desktop(dir.path(), &store, &mut |_| {})
            .await
            .expect("import");
        assert_eq!(summary.imported, 1, "{summary:?}");
        let bodies: Vec<_> = all_messages(&store)
            .await
            .into_iter()
            .filter_map(|m| m.body)
            .collect();
        assert_eq!(bodies, vec!["written by stock sqlcipher"]);
    }

    /// If encryption silently went missing (a SQLCipher build without crypto
    /// treats the key pragma as a no-op), any key would "work". A wrong key
    /// must fail.
    #[tokio::test]
    async fn the_wrong_key_does_not_open_the_database() {
        let dir = stock_sqlcipher_fixture();
        std::fs::write(
            dir.path().join("config.json"),
            json!({ "key": "0".repeat(64) }).to_string(),
        )
        .expect("config");
        let store = MemoryStore::default();
        let result = import_signal_desktop(dir.path(), &store, &mut |_| {}).await;
        assert!(result.is_err(), "opened with the wrong key: {result:?}");
    }
}
