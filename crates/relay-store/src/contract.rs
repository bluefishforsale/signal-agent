//! Invariants every `MessageStore` must hold, run against each implementation
//! so tools see the same behavior from the in-memory and SQLCipher stores.

use proptest::prelude::*;
use relay_core::prelude::*;
use relay_secrets::StoreKey;
use tempfile::TempDir;
use uuid::Uuid;

use crate::{MemoryStore, MessageStore, SearchQuery, SqlCipherStore};

/// One fresh instance of every implementation.
struct Stores {
    memory: MemoryStore,
    sqlcipher: SqlCipherStore,
    _dir: TempDir,
}

impl Stores {
    async fn open() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let sqlcipher = SqlCipherStore::open(
            &dir.path().join("messages.db"),
            &StoreKey::from_bytes([7; 32]),
        )
        .await
        .expect("open sqlcipher store");
        Self {
            memory: MemoryStore::default(),
            sqlcipher,
            _dir: dir,
        }
    }

    fn each(&self) -> [(&'static str, &dyn MessageStore); 2] {
        [("memory", &self.memory), ("sqlcipher", &self.sqlcipher)]
    }

    async fn close(self) {
        self.sqlcipher.close().await;
    }
}

fn sid(n: u128) -> ServiceId {
    ServiceId::new(Uuid::from_u128(n))
}

fn msg(author: u128, sent_at_ms: u64, conversation: Recipient, body: &str) -> Message {
    Message {
        id: MessageId {
            author: sid(author),
            sent_at_ms,
        },
        conversation,
        sender: sid(author),
        from_me: false,
        received_at_ms: sent_at_ms + 5,
        body: Some(body.to_owned()),
        attachments: vec![],
        quote: None,
        edited: false,
        deleted: false,
    }
}

/// Every field a tool reports must survive the round trip through storage,
/// including group conversations, quotes and attachment metadata.
#[tokio::test]
async fn a_stored_message_reads_back_unchanged() {
    let group = Recipient::Group(GroupId::new([0xab; 32]));
    let mut message = msg(1, 1_000, group, "hello");
    message.from_me = true;
    message.quote = Some(MessageId {
        author: sid(2),
        sent_at_ms: 900,
    });
    message.attachments = vec![AttachmentRef {
        id: "att-1".into(),
        content_type: "image/png".into(),
        file_name: Some("cat.png".into()),
        size: 4096,
    }];
    let mut bodiless = msg(2, 2_000, group, "");
    bodiless.body = None;
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&message).await.expect("insert");
        store.insert(&bodiless).await.expect("insert");
        let got = store.history(group, None, 10).await.expect("history");
        assert_eq!(got, vec![bodiless.clone(), message.clone()], "{name}");
    }
    stores.close().await;
}

/// Paging back through a conversation must neither repeat nor skip the
/// message at the boundary, and must not leak other conversations in.
#[tokio::test]
async fn history_pages_newest_first_strictly_before_the_cursor() {
    let alice = Recipient::Contact(sid(1));
    let bob = Recipient::Contact(sid(2));
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        for t in [10, 20, 30, 40] {
            store.insert(&msg(1, t, alice, "a")).await.expect("insert");
        }
        store.insert(&msg(2, 25, bob, "b")).await.expect("insert");
        let page = store.history(alice, Some(40), 2).await.expect("history");
        let times: Vec<_> = page.iter().map(|m| m.id.sent_at_ms).collect();
        assert_eq!(times, vec![30, 20], "{name}");
    }
    stores.close().await;
}

/// A resent or forged message reusing an existing id must neither duplicate
/// the row nor replace its body or clear its deleted state.
#[tokio::test]
async fn inserting_an_existing_id_changes_nothing() {
    let alice = Recipient::Contact(sid(1));
    let original = msg(1, 1, alice, "hi");
    let replay = msg(1, 1, alice, "rewritten");
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        let new = store.insert(&original).await.expect("insert");
        assert!(new, "{name}: first insert not new");
        store
            .mark_deleted(original.id, original.id.author)
            .await
            .expect("delete");
        let new = store.insert(&replay).await.expect("insert");
        assert!(!new, "{name}: replay reported as new");
        let stats = store.stats().await.expect("stats");
        assert_eq!(stats.messages, 1, "{name}: duplicate rows: {stats:?}");
        let kept = store.history(alice, None, 1).await.expect("history");
        let kept = kept.first().expect("row kept");
        assert_eq!(kept.body.as_deref(), Some("hi"), "{name}: body replaced");
        assert!(kept.deleted, "{name}: replay cleared deleted state");
    }
    stores.close().await;
}

/// Someone else in the chat must not be able to rewrite or erase a message
/// they didn't write; Signal only lets the author edit or delete.
#[tokio::test]
async fn edits_and_deletes_from_non_authors_are_refused() {
    let alice = Recipient::Contact(sid(1));
    let original = msg(1, 1, alice, "original");
    let stranger = sid(666);
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&original).await.expect("insert");
        let edit = store.apply_edit(original.id, stranger, "forged").await;
        assert!(edit.is_err(), "{name}: stranger edit accepted");
        let delete = store.mark_deleted(original.id, stranger).await;
        assert!(delete.is_err(), "{name}: stranger delete accepted");
        let kept = store.history(alice, None, 1).await.expect("history");
        assert_eq!(kept, vec![original.clone()], "{name}");
    }
    stores.close().await;
}

/// An author's edit replaces the body and is flagged, so tools can say so.
#[tokio::test]
async fn an_author_edit_replaces_the_body_and_flags_it() {
    let alice = Recipient::Contact(sid(1));
    let original = msg(1, 1, alice, "original");
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&original).await.expect("insert");
        store
            .apply_edit(original.id, original.id.author, "fixed")
            .await
            .expect("edit");
        let got = store.history(alice, None, 1).await.expect("history");
        let edited = got.first().expect("message kept");
        assert_eq!(edited.body.as_deref(), Some("fixed"), "{name}");
        assert!(edited.edited, "{name}: edit not flagged: {edited:?}");
    }
    stores.close().await;
}

/// Editing or deleting a message the store never saw must fail, not
/// silently succeed, so the caller can retry once the message syncs.
#[tokio::test]
async fn edits_and_deletes_of_unknown_messages_fail() {
    let missing = MessageId {
        author: sid(1),
        sent_at_ms: 99,
    };
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        let edit = store.apply_edit(missing, missing.author, "x").await;
        assert!(edit.is_err(), "{name}: edit of unknown message accepted");
        let delete = store.mark_deleted(missing, missing.author).await;
        assert!(
            delete.is_err(),
            "{name}: delete of unknown message accepted"
        );
    }
    stores.close().await;
}

/// Unread is what others sent that hasn't been marked read; the user's own
/// messages are never unread, and marking read survives a resync of the same
/// message.
#[tokio::test]
async fn unread_excludes_own_and_read_messages_across_resyncs() {
    let alice = Recipient::Contact(sid(1));
    let read = msg(1, 1, alice, "seen");
    let unread = msg(1, 2, alice, "new");
    let mut mine = msg(3, 3, alice, "my reply");
    mine.from_me = true;
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        for m in [&read, &unread, &mine] {
            store.insert(m).await.expect("insert");
        }
        store.mark_read(&[read.id]).await.expect("mark read");
        store.insert(&read).await.expect("resync");
        let got = store.unread().await.expect("unread");
        assert_eq!(got, vec![unread.clone()], "{name}");
        let stats = store.stats().await.expect("stats");
        assert_eq!(stats.unread, 1, "{name}: {stats:?}");
    }
    stores.close().await;
}

/// The conversation list is most recently active first, carries each
/// conversation's title (even one set before its first message arrived) and
/// counts only that conversation's unread messages.
#[tokio::test]
async fn conversations_list_most_recent_first_with_titles_and_unread() {
    let alice = Recipient::Contact(sid(1));
    let group = Recipient::Group(GroupId::new([1; 32]));
    let mut mine = msg(3, 30, group, "mine");
    mine.from_me = true;
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.set_title(group, "Band").await.expect("title");
        store
            .insert(&msg(1, 10, alice, "a1"))
            .await
            .expect("insert");
        store
            .insert(&msg(1, 20, alice, "a2"))
            .await
            .expect("insert");
        store.insert(&mine).await.expect("insert");
        store.set_title(alice, "Alice").await.expect("title");
        let got = store.conversations().await.expect("conversations");
        let expected = vec![
            Conversation {
                recipient: group,
                title: "Band".into(),
                last_message_at_ms: Some(30),
                unread: 0,
            },
            Conversation {
                recipient: alice,
                title: "Alice".into(),
                last_message_at_ms: Some(20),
                unread: 2,
            },
        ];
        assert_eq!(got, expected, "{name}");
    }
    stores.close().await;
}

fn search(text: &str) -> SearchQuery {
    SearchQuery {
        text: text.into(),
        limit: 10,
        ..Default::default()
    }
}

/// A deleted-for-everyone message must stop matching searches.
#[tokio::test]
async fn deleted_messages_drop_out_of_search() {
    let alice = Recipient::Contact(sid(1));
    let secret = msg(1, 1, alice, "secret plan");
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&secret).await.expect("insert");
        store
            .mark_deleted(secret.id, secret.id.author)
            .await
            .expect("delete");
        let hits = store.search(&search("plan")).await.expect("search");
        assert!(hits.is_empty(), "{name}: deleted message found: {hits:?}");
    }
    stores.close().await;
}

/// Each search filter narrows the results on its own: sender, conversation,
/// an inclusive start and an exclusive end, then offset and limit.
#[tokio::test]
async fn search_filters_narrow_by_sender_conversation_and_time() {
    let alice = Recipient::Contact(sid(1));
    let group = Recipient::Group(GroupId::new([2; 32]));
    let fixtures = [
        msg(1, 10, alice, "Lunch tomorrow?"),
        msg(1, 20, group, "lunch at noon"),
        msg(2, 30, group, "LUNCH is ready"),
        msg(2, 40, group, "dinner instead"),
    ];
    let times = |hits: Vec<Message>| {
        let mut t: Vec<_> = hits.iter().map(|m| m.id.sent_at_ms).collect();
        t.sort();
        t
    };
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        for m in &fixtures {
            store.insert(m).await.expect("insert");
        }
        let all = store.search(&search("lunch")).await.expect("search");
        assert_eq!(times(all), vec![10, 20, 30], "{name}: case-insensitive");
        let by_sender = SearchQuery {
            sender: Some(sid(2)),
            ..search("lunch")
        };
        let hits = store.search(&by_sender).await.expect("search");
        assert_eq!(times(hits), vec![30], "{name}: sender");
        let in_group = SearchQuery {
            conversation: Some(group),
            ..search("lunch")
        };
        let hits = store.search(&in_group).await.expect("search");
        assert_eq!(times(hits), vec![20, 30], "{name}: conversation");
        let window = SearchQuery {
            since_ms: Some(20),
            until_ms: Some(30),
            ..search("lunch")
        };
        let hits = store.search(&window).await.expect("search");
        assert_eq!(times(hits), vec![20], "{name}: time window");
        let page = SearchQuery {
            limit: 1,
            offset: 1,
            ..search("lunch")
        };
        let hits = store.search(&page).await.expect("search");
        assert_eq!(hits.len(), 1, "{name}: paging: {hits:?}");
    }
    stores.close().await;
}

/// After an edit, search must find the new wording and forget the old, or a
/// corrected typo keeps surfacing under what it used to say.
#[tokio::test]
async fn search_follows_edits() {
    let alice = Recipient::Contact(sid(1));
    let original = msg(1, 1, alice, "meet at the bakery");
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&original).await.expect("insert");
        store
            .apply_edit(original.id, original.id.author, "meet at the library")
            .await
            .expect("edit");
        let old = store.search(&search("bakery")).await.expect("search");
        assert!(old.is_empty(), "{name}: old wording still found: {old:?}");
        let new = store.search(&search("library")).await.expect("search");
        assert_eq!(new.len(), 1, "{name}: new wording not found: {new:?}");
    }
    stores.close().await;
}

/// Search text comes straight from an MCP client, so no input may error,
/// whatever query syntax or punctuation it carries.
#[test]
fn search_accepts_any_text() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let stores = rt.block_on(Stores::open());
    let alice = Recipient::Contact(sid(1));
    for (_, store) in stores.each() {
        rt.block_on(store.insert(&msg(1, 1, alice, "an \"odd\" (message) AND*")))
            .expect("insert");
    }
    proptest!(ProptestConfig::with_cases(128), |(text in ".{0,40}")| {
        for (name, store) in stores.each() {
            let hits = rt.block_on(store.search(&search(&text)));
            prop_assert!(hits.is_ok(), "{name}: search {text:?} failed: {hits:?}");
        }
    });
    for text in [
        "",
        " ",
        "\"",
        "*",
        "AND",
        "NOT x",
        "a:b",
        "(",
        "^",
        "NEAR(a b)",
    ] {
        for (name, store) in stores.each() {
            let hits = rt.block_on(store.search(&search(text)));
            assert!(hits.is_ok(), "{name}: search {text:?} failed: {hits:?}");
        }
    }
    rt.block_on(stores.close());
}

/// Marking a read message unread must bring it back into unread counts,
/// or mark_as_unread would silently do nothing.
#[tokio::test]
async fn mark_unread_restores_a_read_message_to_unread() {
    let alice = Recipient::Contact(sid(1));
    let m = msg(1, 1, alice, "hi");
    let stores = Stores::open().await;
    for (name, store) in stores.each() {
        store.insert(&m).await.expect("insert");
        store.mark_read(&[m.id]).await.expect("read");
        let unread = store.unread().await.expect("unread");
        assert!(unread.is_empty(), "{name}: still unread after mark_read");
        store.mark_unread(&[m.id]).await.expect("unread");
        let unread = store.unread().await.expect("unread");
        assert_eq!(unread, vec![m.clone()], "{name}: not unread again");
    }
    stores.close().await;
}
