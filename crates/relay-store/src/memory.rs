//! A store that keeps everything in a Vec, so tool and agent lanes can test
//! against real store behavior before the SQLCipher store exists. Search is a
//! case-insensitive substring match, not FTS ranking.

use std::{collections::HashMap, sync::Mutex};

use anyhow::Context;
use async_trait::async_trait;
use relay_core::prelude::*;

use crate::{MessageStore, SearchQuery, StoreStats, ensure_author};

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

fn find(messages: &mut [Message], target: MessageId) -> anyhow::Result<&mut Message> {
    messages
        .iter_mut()
        .find(|m| m.id == target)
        .with_context(|| format!("no stored message {target:?}"))
}

#[async_trait]
impl MessageStore for MemoryStore {
    async fn insert(&self, message: &Message) -> anyhow::Result<bool> {
        let mut inner = self.inner();
        if inner.messages.iter().any(|m| m.id == message.id) {
            return Ok(false);
        }
        inner.messages.push(message.clone());
        Ok(true)
    }

    async fn apply_edit(
        &self,
        target: MessageId,
        editor: ServiceId,
        body: &str,
    ) -> anyhow::Result<()> {
        ensure_author(target, editor, "edit")?;
        let mut inner = self.inner();
        let message = find(&mut inner.messages, target)?;
        message.body = Some(body.to_owned());
        message.edited = true;
        Ok(())
    }

    async fn mark_deleted(&self, target: MessageId, deleter: ServiceId) -> anyhow::Result<()> {
        ensure_author(target, deleter, "delete")?;
        let mut inner = self.inner();
        find(&mut inner.messages, target)?.deleted = true;
        Ok(())
    }

    async fn mark_read(&self, targets: &[MessageId]) -> anyhow::Result<()> {
        self.inner().read.extend_from_slice(targets);
        Ok(())
    }

    async fn mark_unread(&self, targets: &[MessageId]) -> anyhow::Result<()> {
        self.inner().read.retain(|id| !targets.contains(id));
        Ok(())
    }

    async fn set_title(&self, conversation: Recipient, title: &str) -> anyhow::Result<()> {
        self.inner().titles.insert(conversation, title.to_owned());
        Ok(())
    }

    async fn conversations(&self) -> anyhow::Result<Vec<Conversation>> {
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

    async fn history(
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

    async fn search(&self, query: &SearchQuery) -> anyhow::Result<Vec<Message>> {
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

    async fn unread(&self) -> anyhow::Result<Vec<Message>> {
        let inner = self.inner();
        Ok(inner
            .messages
            .iter()
            .filter(|m| !m.from_me && !inner.read.contains(&m.id))
            .cloned()
            .collect())
    }

    async fn prune(&self, older_than_ms: u64) -> anyhow::Result<u64> {
        let mut inner = self.inner();
        let before = inner.messages.len();
        inner.messages.retain(|m| m.id.sent_at_ms >= older_than_ms);
        Ok((before - inner.messages.len()) as u64)
    }

    async fn export(
        &self,
        conversation: Option<Recipient>,
        since_ms: Option<u64>,
    ) -> anyhow::Result<Vec<Message>> {
        let inner = self.inner();
        let mut out: Vec<_> = inner
            .messages
            .iter()
            .filter(|m| conversation.is_none_or(|c| m.conversation == c))
            .filter(|m| since_ms.is_none_or(|s| m.id.sent_at_ms >= s))
            .cloned()
            .collect();
        out.sort_by_key(|m| m.id.sent_at_ms);
        Ok(out)
    }

    async fn stats(&self) -> anyhow::Result<StoreStats> {
        let unread = self.unread().await?.len() as u64;
        let conversations = self.conversations().await?.len() as u64;
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
