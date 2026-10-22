//! Continuous capture: everything Signal delivers lands in the store whether or
//! not any Claude app is connected, so history is complete when one asks.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, bail};
use futures::StreamExt;
use relay_core::{Message, ProfileName};
use relay_signal::{Incoming, SignalBackend};
use relay_store::MessageStore;
use relay_tools::WebhookUrl;
use serde_json::json;

/// How long a webhook POST may take before it's abandoned.
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Where newly captured messages are posted, for one profile.
pub(crate) struct Webhook {
    profile: ProfileName,
    pub(crate) url: WebhookUrl,
    client: reqwest::Client,
}

impl Webhook {
    pub(crate) fn new(profile: ProfileName, url: WebhookUrl) -> anyhow::Result<Self> {
        // reqwest is built without a bundled crypto provider, so the process
        // needs one; an error here means one is already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        // NOTE: never follow a redirect. A 307 or 308 re-POSTs the message to
        // an address WebhookUrl never vetted, plain http off the machine
        // included; a redirecting webhook fails and is logged instead.
        let client = reqwest::Client::builder()
            .timeout(WEBHOOK_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("unable to build the webhook client")?;
        Ok(Self {
            profile,
            url,
            client,
        })
    }

    /// Posts `message` in the background, best effort.
    // NOTE: spawned, never awaited. A slow or dead receiver must not hold up
    // capture, or messages would back up behind it and be lost at shutdown.
    fn post(self: &Arc<Self>, message: &Message) {
        let (webhook, message) = (self.clone(), message.clone());
        tokio::spawn(async move {
            if let Some(failure) = webhook.deliver(&message).await {
                tracing::warn!("{failure}");
            }
        });
    }

    /// Posts `message` and returns the line to log if it wasn't accepted.
    // NOTE: the URL may carry a token and the body is the message, so the log
    // line names the redacted URL only, and reqwest's error loses its URL.
    async fn deliver(&self, message: &Message) -> Option<String> {
        let sent = self
            .client
            .post(self.url.as_url().clone())
            .json(&json!({
                "event": "message",
                "profile": self.profile.to_string(),
                "message": message,
            }))
            .send()
            .await;
        let why = match sent {
            Ok(response) if response.status().is_success() => return None,
            Ok(response) => format!("it answered {}", response.status()),
            Err(e) => e.without_url().to_string(),
        };
        Some(format!(
            "webhook {} missed a message: {why}",
            self.url.redacted()
        ))
    }
}

/// Applies every event from `signal.incoming()` to `store` until the stream ends.
pub(crate) async fn run(
    signal: Arc<dyn SignalBackend>,
    store: Arc<dyn MessageStore>,
    webhook: Option<Webhook>,
) {
    let webhook = webhook.map(Arc::new);
    let mut events = signal.incoming();
    while let Some(event) = events.next().await {
        let result = match event {
            Ok(event) => apply(signal.as_ref(), store.as_ref(), webhook.as_ref(), event).await,
            Err(e) => Err(e),
        };
        // NOTE: one bad event (a forged edit, a delete for a message we never saw)
        // must not stop capture, or every later message would be lost.
        if let Err(e) = result {
            tracing::warn!("capture skipped an event: {e:#}");
        }
    }
}

async fn apply(
    signal: &dyn SignalBackend,
    store: &dyn MessageStore,
    webhook: Option<&Arc<Webhook>>,
    event: Incoming,
) -> anyhow::Result<()> {
    match event {
        Incoming::Message(message) => {
            // NOTE: the store's edit and delete checks trust `id.author`, so a
            // message whose id names someone other than its sender is forged.
            if message.id.author != message.sender {
                bail!(
                    "{} sent a message under {}'s id",
                    message.sender,
                    message.id.author
                );
            }
            // A webhook only hears of messages the store took, never a
            // duplicate or one it refused.
            let inserted = store.insert(&message).await?;
            if let (true, Some(webhook)) = (inserted, webhook) {
                webhook.post(&message);
            }
            Ok(())
        }
        Incoming::Edit {
            target, from, body, ..
        } => store.apply_edit(target, from, &body).await,
        Incoming::Delete { target, from, .. } => store.mark_deleted(target, from).await,
        Incoming::AdminDelete {
            group,
            target,
            from,
        } => {
            // NOTE: the store can't check the deleter, so the admin list must
            // come from Signal here, never from the event, or any member could
            // erase anyone's messages from the local history.
            let groups = signal.groups().await?;
            let Some(found) = groups.iter().find(|g| g.id == group) else {
                bail!("admin delete by {from} in group {group}, which this account isn't in");
            };
            if !found.admins.contains(&from) {
                bail!("{from} is not an admin of group {group}, so their delete was dropped");
            }
            store.mark_admin_deleted(group, target).await
        }
        Incoming::Read { targets } => store.mark_read(&targets).await,
        // NOTE: MessageStore has no place for reactions yet, so they are dropped
        // here. When the store grows one, apply them with `from` like edits.
        Incoming::Reaction { .. } => Ok(()),
        // Nothing to store until the identity tools (lane L3d) exist; the log
        // keeps a trusted safety-number change from passing silently.
        Incoming::IdentityChanged { contact } => {
            tracing::warn!("safety number changed for {contact}; trusted the new key");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use relay_signal::{Account, FakeSignal};
    use relay_store::MemoryStore;
    use uuid::Uuid;

    use super::*;

    fn fake() -> Arc<FakeSignal> {
        Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        }))
    }

    fn msg(author: u128, sent_at_ms: u64, body: &str) -> Message {
        let sender = ServiceId::new(Uuid::from_u128(author));
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

    /// A message received while no client is connected must be findable later
    /// (PRD acceptance 7): capture writes it to the store on its own.
    #[tokio::test]
    async fn received_messages_land_in_the_store_without_a_client() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        signal.push(Incoming::Message(msg(2, 10, "dinner at 8")));
        let hits = eventually(|| async {
            store
                .search(&relay_store::SearchQuery {
                    text: "dinner".into(),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .expect("search")
        })
        .await;
        capture.abort();
        assert_eq!(hits.len(), 1, "captured message not searchable: {hits:?}");
    }

    /// Edits must reach the store with their sender so the store can refuse a
    /// forged one, and a refused edit must not stop capture of the next event.
    #[tokio::test]
    async fn a_forged_edit_is_refused_and_capture_keeps_going() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = msg(2, 10, "original");
        let conversation = original.conversation;
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Edit {
            conversation,
            target: original.id,
            from: ServiceId::new(Uuid::from_u128(666)),
            body: "forged".into(),
        });
        signal.push(Incoming::Edit {
            conversation,
            target: original.id,
            from: original.sender,
            body: "fixed typo".into(),
        });
        let edited = eventually(|| async {
            store
                .history(conversation, None, 1)
                .await
                .expect("history")
                .into_iter()
                .filter(|m| m.edited)
                .collect()
        })
        .await;
        capture.abort();
        let body = edited.first().and_then(|m| m.body.as_deref());
        assert_eq!(body, Some("fixed typo"), "edit not applied: {edited:?}");
    }

    /// A delete-for-everyone must reach the store with its sender, so the
    /// author's delete hides the message and a stranger's cannot.
    #[tokio::test]
    async fn deletes_are_applied_with_their_sender() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = msg(2, 10, "oops");
        let conversation = original.conversation;
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Delete {
            conversation,
            target: original.id,
            from: original.sender,
        });
        let deleted = eventually(|| async {
            store
                .history(conversation, None, 1)
                .await
                .expect("history")
                .into_iter()
                .filter(|m| m.deleted)
                .collect()
        })
        .await;
        capture.abort();
        assert_eq!(deleted.len(), 1, "delete not applied: {deleted:?}");
    }

    /// A read sync from the phone must clear unread here too, or Claude would
    /// keep reporting messages the user already read.
    #[tokio::test]
    async fn read_syncs_clear_unread() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = msg(2, 10, "seen it");
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::Read {
            targets: vec![original.id],
        });
        signal.push(Incoming::Message(msg(2, 11, "marker")));
        let marker = eventually(|| async {
            let unread = store.unread().await.expect("unread");
            if unread.iter().any(|m| m.id.sent_at_ms == 11) {
                unread
            } else {
                vec![]
            }
        })
        .await;
        capture.abort();
        let still_unread: Vec<_> = marker.iter().filter(|m| m.id == original.id).collect();
        assert!(
            still_unread.is_empty(),
            "read sync ignored: {still_unread:?}"
        );
    }

    /// A message claiming another author's id is dropped and the stored
    /// original is unchanged: otherwise anyone could plant text under someone
    /// else's id and pass the author checks that guard edits and deletes.
    #[tokio::test]
    async fn a_message_claiming_another_authors_id_is_dropped_and_the_original_is_unchanged() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = msg(2, 10, "original");
        signal.push(Incoming::Message(original.clone()));
        let mut forged = msg(2, 11, "forged");
        forged.sender = ServiceId::new(Uuid::from_u128(666));
        signal.push(Incoming::Message(forged));
        signal.push(Incoming::Message(msg(2, 12, "marker")));
        let stored = eventually(|| async {
            let all = store
                .history(original.conversation, None, 10)
                .await
                .expect("history");
            if all.iter().any(|m| m.id.sent_at_ms == 12) {
                all
            } else {
                vec![]
            }
        })
        .await;
        capture.abort();
        let bodies: Vec<_> = stored.iter().filter_map(|m| m.body.as_deref()).collect();
        assert!(
            !bodies.contains(&"forged"),
            "forged message stored: {bodies:?}"
        );
        assert!(bodies.contains(&"original"), "original lost: {bodies:?}");
    }

    /// A safety-number change is an event to note, not an error: capture must
    /// keep storing what arrives after it.
    #[tokio::test]
    async fn an_identity_change_does_not_interrupt_capture() {
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        signal.push(Incoming::IdentityChanged {
            contact: ServiceId::new(Uuid::from_u128(2)),
        });
        let after = msg(2, 10, "after the change");
        signal.push(Incoming::Message(after.clone()));
        let stored = eventually(|| async {
            store
                .history(after.conversation, None, 1)
                .await
                .expect("history")
        })
        .await;
        capture.abort();
        assert_eq!(stored.len(), 1, "capture stopped after an identity change");
    }

    fn group_msg(group: GroupId, author: u128, sent_at_ms: u64, body: &str) -> Message {
        Message {
            conversation: Recipient::Group(group),
            ..msg(author, sent_at_ms, body)
        }
    }

    fn fake_with_group(group: GroupId, admins: &[u128]) -> Arc<FakeSignal> {
        let ids = |ns: &[u128]| {
            ns.iter()
                .map(|n| ServiceId::new(Uuid::from_u128(*n)))
                .collect()
        };
        Arc::new(
            Arc::into_inner(fake())
                .expect("sole owner")
                .with_groups(vec![relay_signal::Group {
                    id: group,
                    title: "g".into(),
                    members: ids(&[2, 3, 4]),
                    admins: ids(admins),
                }]),
        )
    }

    /// A group admin's delete-for-everyone hides the target in the local
    /// history, as it does on every phone in the group.
    #[tokio::test]
    async fn an_admins_delete_hides_another_members_group_message() {
        let group = GroupId::new([7; 32]);
        let signal = fake_with_group(group, &[3]);
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = group_msg(group, 2, 10, "spam");
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::AdminDelete {
            group,
            target: original.id,
            from: ServiceId::new(Uuid::from_u128(3)),
        });
        let deleted = eventually(|| async {
            let got = store.get(original.id).await.expect("get");
            got.filter(|m| m.deleted).into_iter().collect()
        })
        .await;
        capture.abort();
        assert_eq!(deleted.len(), 1, "admin delete not applied");
    }

    /// An admin delete from a member who isn't an admin is forged or stale:
    /// it's dropped, the message stays, and capture keeps going.
    #[tokio::test]
    async fn a_non_admins_admin_delete_is_dropped_and_capture_keeps_going() {
        let group = GroupId::new([7; 32]);
        let signal = fake_with_group(group, &[3]);
        let store = Arc::new(MemoryStore::default());
        let capture = tokio::spawn(run(signal.clone(), store.clone(), None));
        let original = group_msg(group, 2, 10, "keep me");
        signal.push(Incoming::Message(original.clone()));
        signal.push(Incoming::AdminDelete {
            group,
            target: original.id,
            from: ServiceId::new(Uuid::from_u128(4)),
        });
        let marker = group_msg(group, 2, 11, "marker");
        signal.push(Incoming::Message(marker.clone()));
        let landed = eventually(|| async {
            store
                .get(marker.id)
                .await
                .expect("get")
                .into_iter()
                .collect()
        })
        .await;
        let kept = store.get(original.id).await.expect("get");
        capture.abort();
        assert_eq!(
            landed.len(),
            1,
            "capture stopped after a forged admin delete"
        );
        assert!(
            kept.is_some_and(|m| !m.deleted),
            "a non-admin's delete was applied"
        );
    }

    const OK: &str = "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n";

    /// A webhook that redirects must not move message contents anywhere the
    /// user didn't set: a 307 re-POSTs the body, and the new address could
    /// be plain http off the machine, which WebhookUrl forbids.
    #[tokio::test]
    async fn a_redirecting_webhook_is_not_followed() {
        let (elsewhere, mut diverted) = receiver(Some(OK.into())).await;
        let redirect = format!(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: {elsewhere}\r\ncontent-length: 0\r\n\r\n"
        );
        let (url, mut posts) = receiver(Some(redirect)).await;
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let webhook = Webhook::new(ProfileName::default_profile(), url).expect("client");
        let capture = tokio::spawn(run(signal.clone(), store.clone(), Some(webhook)));
        signal.push(Incoming::Message(msg(2, 10, "private")));
        tokio::time::timeout(std::time::Duration::from_secs(5), posts.recv())
            .await
            .expect("no POST within 5 s");
        let followed =
            tokio::time::timeout(std::time::Duration::from_millis(500), diverted.recv()).await;
        capture.abort();
        assert!(
            followed.is_err(),
            "the redirect target received {followed:?}"
        );
    }

    /// A failed delivery is logged, but a token in the webhook's query or
    /// userinfo, and the message itself, must never reach the log file. A
    /// redirect answer counts as a failure, since it isn't followed.
    #[tokio::test]
    async fn a_failed_delivery_logs_neither_the_token_nor_the_message() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = closed.local_addr().expect("addr").port();
        drop(closed);
        let (redirecting, _posts) = receiver(Some(
            "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\n\r\n"
                .into(),
        ))
        .await;
        let redirecting = redirecting.as_url();
        let urls = [
            format!("http://user:SECRET@127.0.0.1:{port}/hook?token=SECRET"),
            format!(
                "http://user:SECRET@127.0.0.1:{}/hook?token=SECRET",
                redirecting.port().expect("port")
            ),
        ];
        for url in urls {
            let url = WebhookUrl::parse(&url).expect("url");
            let webhook = Webhook::new(ProfileName::default_profile(), url).expect("client");
            let logged = webhook
                .deliver(&msg(2, 10, "BODYTEXT"))
                .await
                .expect("a failed delivery must be logged");
            assert!(
                !logged.contains("SECRET") && !logged.contains("BODYTEXT"),
                "leaked into the log: {logged}"
            );
            assert!(
                logged.contains("127.0.0.1"),
                "log doesn't say where: {logged}"
            );
        }
    }

    /// A webhook receiver on loopback that hands each request body to the
    /// test and answers with `response`, or never answers when it's None.
    async fn receiver(
        response: Option<String>,
    ) -> (WebhookUrl, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}/hook", listener.local_addr().expect("addr"));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let body = loop {
                        let Ok(n) = conn.read(&mut chunk).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else {
                            continue;
                        };
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            break format!("{}\n{body}", head.lines().next().unwrap_or(""));
                        }
                    };
                    let _ = tx.send(body);
                    let Some(response) = response else {
                        return std::future::pending::<()>().await;
                    };
                    let _ = conn.write_all(response.as_bytes()).await;
                });
            }
        });
        (WebhookUrl::parse(&url).expect("loopback url"), rx)
    }

    /// Every message the store accepted is posted to the user's webhook,
    /// named with its profile, in the shape the contract fixes.
    #[tokio::test]
    async fn a_captured_message_is_posted_to_the_webhook() {
        let (url, mut posts) = receiver(Some(OK.into())).await;
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let webhook = Webhook::new(ProfileName::default_profile(), url).expect("client");
        let capture = tokio::spawn(run(signal.clone(), store.clone(), Some(webhook)));
        let message = msg(2, 10, "hello hook");
        signal.push(Incoming::Message(message.clone()));
        let post = tokio::time::timeout(std::time::Duration::from_secs(5), posts.recv())
            .await
            .expect("no POST within 5 s")
            .expect("receiver alive");
        capture.abort();
        let (request_line, body) = post.split_once('\n').expect("request line");
        assert!(request_line.starts_with("POST /hook "), "{request_line}");
        let body: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(
            body,
            serde_json::json!({
                "event": "message",
                "profile": ProfileName::default_profile().to_string(),
                "message": serde_json::to_value(&message).expect("message json"),
            })
        );
    }

    /// A webhook must never see a message the store refused: a redelivered
    /// duplicate is posted once, and a forged one not at all.
    #[tokio::test]
    async fn a_message_the_store_refused_is_never_posted() {
        let (url, mut posts) = receiver(Some(OK.into())).await;
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let webhook = Webhook::new(ProfileName::default_profile(), url).expect("client");
        let capture = tokio::spawn(run(signal.clone(), store.clone(), Some(webhook)));
        let first = msg(2, 10, "once");
        signal.push(Incoming::Message(first.clone()));
        signal.push(Incoming::Message(first));
        let mut forged = msg(2, 11, "forged");
        forged.sender = ServiceId::new(Uuid::from_u128(666));
        signal.push(Incoming::Message(forged));
        signal.push(Incoming::Message(msg(2, 12, "marker")));
        let mut bodies = vec![];
        while bodies.len() < 2 {
            let post = tokio::time::timeout(std::time::Duration::from_secs(5), posts.recv())
                .await
                .expect("posts stopped arriving")
                .expect("receiver alive");
            bodies.push(post);
        }
        // Anything still in flight had as long as the marker to arrive.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        while let Ok(post) = posts.try_recv() {
            bodies.push(post);
        }
        capture.abort();
        let texts: Vec<_> = bodies
            .iter()
            .map(|b| {
                let json: serde_json::Value =
                    serde_json::from_str(b.split_once('\n').expect("line").1).expect("json");
                json["message"]["body"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        let mut sorted = texts.clone();
        sorted.sort();
        assert_eq!(sorted, ["marker", "once"], "posted: {texts:?}");
    }

    /// A receiver that never answers must not hold capture up: later
    /// messages still land in the store right away.
    #[tokio::test]
    async fn a_webhook_that_never_answers_does_not_stall_capture() {
        let (url, _posts) = receiver(None).await;
        let signal = fake();
        let store = Arc::new(MemoryStore::default());
        let webhook = Webhook::new(ProfileName::default_profile(), url).expect("client");
        let capture = tokio::spawn(run(signal.clone(), store.clone(), Some(webhook)));
        for n in 0..5 {
            signal.push(Incoming::Message(msg(2, 10 + n, "stuck")));
        }
        let stored = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let all = store
                    .history(msg(2, 0, "").conversation, None, 10)
                    .await
                    .expect("history");
                if all.len() == 5 {
                    return all;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        capture.abort();
        assert!(
            stored.is_ok(),
            "capture stalled behind an unanswered webhook"
        );
    }

    async fn eventually<F, Fut>(mut probe: F) -> Vec<Message>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Vec<Message>>,
    {
        for _ in 0..200 {
            let found = probe().await;
            if !found.is_empty() {
                return found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        probe().await
    }
}
