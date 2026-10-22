//! Tools that put a new message, typing indicator or receipt on the wire.

use async_trait::async_trait;
use relay_core::prelude::*;
use relay_signal::Outgoing;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{contact, contact_schema, non_blank, parse},
};

pub struct SendMessage;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendMessageInput {
    recipient: String,
    message: String,
}

#[async_trait]
impl Tool for SendMessage {
    fn name(&self) -> &'static str {
        "send_message"
    }
    fn description(&self) -> &'static str {
        "Send a text message to one Signal contact. Use send_group_message for groups and \
         send_note_to_self for your own Note to Self. recipient is the contact's phone number \
         or service id; message is the text. Not idempotent: repeating sends a duplicate. The \
         sent message is saved to the local store. Returns the new message's id, which \
         replies, reactions, edits and deletes reference."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "recipient": contact_schema("Who to send to"),
                "message": {"type": "string", "minLength": 1, "description": "Message text to send"},
            },
            "required": ["recipient", "message"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: SendMessageInput = parse(input)?;
        let to = Recipient::Contact(contact(ctx, &input.recipient).await?);
        let body = non_blank("message", input.message)?;
        send_text(ctx, to, body).await
    }
}

/// Sends `body` to `to` and records it in the local history as this
/// account's own message.
// NOTE: Signal doesn't echo a device's own sends back to it, so a send that
// skips the store never shows up in get_conversation or search_messages.
async fn send_text(ctx: &ToolContext, to: Recipient, body: String) -> Result<Value, ToolError> {
    let id = ctx
        .signal
        .send(
            to,
            Outgoing::Text {
                body: body.clone(),
                quote: None,
            },
        )
        .await?;
    ctx.store
        .insert(&Message {
            id,
            conversation: to,
            sender: id.author,
            from_me: true,
            received_at_ms: id.sent_at_ms,
            body: Some(body),
            attachments: vec![],
            quote: None,
            edited: false,
            deleted: false,
        })
        .await?;
    Ok(json!({ "id": id }))
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use relay_signal::Outgoing;
    use serde_json::json;

    use crate::{Registry, testing::*};

    /// A send must reach Signal as plain text to that one contact, return the
    /// id clients quote, edit or delete it by, and land in the local history,
    /// since Signal never echoes this device's own sends back to it.
    #[tokio::test]
    async fn send_message_sends_text_returns_its_id_and_stores_it_as_mine() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let out = Registry::standard()
            .call(
                &ctx,
                "send_message",
                json!({"recipient": sid(2).to_string(), "message": "hi"}),
            )
            .await
            .unwrap();
        let to = Recipient::Contact(sid(2));
        let text = Outgoing::Text {
            body: "hi".into(),
            quote: None,
        };
        assert_eq!(signal.sent(), [(to, text)]);
        let id: MessageId = serde_json::from_value(out["id"].clone()).unwrap();
        assert_eq!(id.author, sid(ME), "{out}");
        let stored = ctx.store.history(to, None, 10).await.unwrap();
        assert_eq!(stored.len(), 1, "{stored:?}");
        assert_eq!(stored[0].id, id);
        assert!(stored[0].from_me, "{stored:?}");
        assert_eq!(stored[0].body.as_deref(), Some("hi"));
    }
}
