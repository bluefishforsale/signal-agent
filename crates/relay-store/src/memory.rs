//! A store that keeps everything in a Vec, so tool and agent lanes can test
//! against real store behavior before the SQLCipher store exists. Search is a
//! case-insensitive substring match, not FTS ranking.

use std::{collections::HashMap, sync::Mutex};

use anyhow::{Context, bail};
use relay_core::prelude::*;

use crate::{MessageStore, SearchQuery, StoreStats};

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    messages: Vec<Message>,
    read: Vec<MessageId>,
    titles: HashMap<Recipient, String>,
}

impl MemoryStore {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("memory store")
    }
}

fn ensure_author(target: MessageId, actor: ServiceId, action: &str) -> anyhow::Result<()> {
    if actor != target.author {
        bail!(
            "{actor} tried to {action} a message written by {}",
            target.author
        );
    }
    Ok(())
}

fn find(messages: &mut [Message], target: MessageId) -> anyhow::Result<&mut Message> {
    messages
        .iter_mut()
        .find(|m| m.id == target)
        .with_context(|| format!("no stored message {target:?}"))
}

impl MessageStore for MemoryStore {
    fn upsert(&self, message: &Message) -> anyhow::Result<()> {
        let mut inner = self.inner();
        match inner.messages.iter_mut().find(|m| m.id == message.id) {
            Some(existing) => *existing = message.clone(),
            None => inner.messages.push(message.clone()),
        }
        Ok(())
    }

    fn apply_edit(&self, target: MessageId, editor: ServiceId, body: &str) -> anyhow::Result<()> {
        ensure_author(target, editor, "edit")?;
        let mut inner = self.inner();
        let message = find(&mut inner.messages, target)?;
        message.body = Some(body.to_owned());
        message.edited = true;
        Ok(())
    }

    fn mark_deleted(&self, target: MessageId, deleter: ServiceId) -> anyhow::Result<()> {
        ensure_author(target, deleter, "delete")?;
        let mut inner = self.inner();
        find(&mut inner.messages, target)?.deleted = true;
        Ok(())
    }

    fn mark_read(&self, targets: &[MessageId]) -> anyhow::Result<()> {
        self.inner().read.extend_from_slice(targets);
        Ok(())
    }

    fn set_title(&self, conversation: Recipient, title: &str) -> anyhow::Result<()> {
        self.inner().titles.insert(conversation, title.to_owned());
        Ok(())
    }

    fn conversations(&self) -> anyhow::Result<Vec<Conversation>> {
        let inner = self.inner();
        let mut by_recipient: HashMap<Recipient, Conversation> = HashMap::new();
        for m in &inner.messages {
            let entry = by_recipient
                .entry(m.conversation)
                .or_insert_with(|| Conversation {
                    recipient: m.conversation,
                    title: inner
                        .titles
                        .get(&m.conversation)
                        .cloned()
                        .unwrap_or_default(),
                    last_message_at_ms: None,
                    unread: 0,
                });
            entry.last_message_at_ms = entry.last_message_at_ms.max(Some(m.id.sent_at_ms));
            if !m.from_me && !inner.read.contains(&m.id) {
                entry.unread += 1;
            }
        }
        let mut out: Vec<_> = by_recipient.into_values().collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.last_message_at_ms));
        Ok(out)
    }

    fn history(
        &self,
        conversation: Recipient,
        before_ms: Option<u64>,
        limit: u32,
    ) -> anyhow::Result<Vec<Message>> {
        let inner = self.inner();
        let mut out: Vec<_> = inner
            .messages
            .iter()
            .filter(|m| m.conversation == conversation)
            .filter(|m| before_ms.is_none_or(|b| m.id.sent_at_ms < b))
            .cloned()
            .collect();
        out.sort_by_key(|m| std::cmp::Reverse(m.id.sent_at_ms));
        out.truncate(limit as usize);
        Ok(out)
    }

    fn search(&self, query: &SearchQuery) -> anyhow::Result<Vec<Message>> {
        let needle = query.text.to_lowercase();
        let inner = self.inner();
        let mut out: Vec<_> = inner
            .messages
            .iter()
            .filter(|m| !m.deleted)
            .filter(|m| {
                m.body
                    .as_deref()
                    .is_some_and(|b| b.to_lowercase().contains(&needle))
            })
            .filter(|m| query.sender.is_none_or(|s| m.sender == s))
            .filter(|m| query.conversation.is_none_or(|c| m.conversation == c))
            .filter(|m| query.since_ms.is_none_or(|s| m.id.sent_at_ms >= s))
            .filter(|m| query.until_ms.is_none_or(|u| m.id.sent_at_ms < u))
            .cloned()
            .collect();
        out.sort_by_key(|m| std::cmp::Reverse(m.id.sent_at_ms));
        Ok(out
            .into_iter()
            .skip(query.offset as usize)
            .take(query.limit as usize)
            .collect())
    }

    fn unread(&self) -> anyhow::Result<Vec<Message>> {
        let inner = self.inner();
        Ok(inner
            .messages
            .iter()
            .filter(|m| !m.from_me && !inner.read.contains(&m.id))
            .cloned()
            .collect())
    }

    fn stats(&self) -> anyhow::Result<StoreStats> {
        let unread = self.unread()?.len() as u64;
        let conversations = self.conversations()?.len() as u64;
        let inner = self.inner();
        let times = inner.messages.iter().map(|m| m.id.sent_at_ms);
        Ok(StoreStats {
            messages: inner.messages.len() as u64,
            conversations,
            unread,
            oldest_ms: times.clone().min(),
            newest_ms: times.max(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn msg(sent_at_ms: u64, body: &str) -> Message {
        let sender = ServiceId::new(Uuid::from_u128(9));
        Message {
            id: MessageId {
                author: sender,
                sent_at_ms,
            },
            conversation: Recipient::Contact(sender),
            sender,
            from_me: false,
            received_at_ms: sent_at_ms,
            body: Some(body.to_owned()),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        }
    }

    /// Receiving the same message twice (a resync) must not duplicate it.
    #[test]
    fn upserting_the_same_id_twice_keeps_one_row() {
        let store = MemoryStore::default();
        store.upsert(&msg(1, "hi")).expect("upsert");
        store.upsert(&msg(1, "hi again")).expect("upsert");
        let stats = store.stats().expect("stats");
        assert_eq!(stats.messages, 1, "duplicate rows: {stats:?}");
    }

    /// A deleted-for-everyone message must stop matching searches.
    #[test]
    fn deleted_messages_drop_out_of_search() {
        let store = MemoryStore::default();
        store.upsert(&msg(1, "secret plan")).expect("upsert");
        let id = msg(1, "").id;
        store.mark_deleted(id, id.author).expect("delete");
        let hits = store
            .search(&SearchQuery {
                text: "plan".into(),
                limit: 10,
                ..Default::default()
            })
            .expect("search");
        assert!(hits.is_empty(), "deleted message still found: {hits:?}");
    }

    /// Someone else in the chat must not be able to rewrite or erase a message
    /// they didn't write; Signal only lets the author edit or delete.
    #[test]
    fn edits_and_deletes_from_non_authors_are_refused() {
        let store = MemoryStore::default();
        let original = msg(1, "original");
        store.upsert(&original).expect("upsert");
        let stranger = ServiceId::new(Uuid::from_u128(666));
        assert!(
            store.apply_edit(original.id, stranger, "forged").is_err(),
            "stranger edit accepted"
        );
        assert!(
            store.mark_deleted(original.id, stranger).is_err(),
            "stranger delete accepted"
        );
        let kept = store
            .history(original.conversation, None, 1)
            .expect("history");
        assert_eq!(
            kept.first().and_then(|m| m.body.as_deref()),
            Some("original")
        );
        assert!(
            kept.first().is_some_and(|m| !m.deleted),
            "message was deleted: {kept:?}"
        );
    }
}
