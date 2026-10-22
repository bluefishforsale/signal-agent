//! The list of tools and the gate every call passes through.

use std::sync::Arc;

use relay_core::prelude::*;
use serde_json::Value;

use crate::{
    Limiter, Tool, ToolContext, ToolError, directory, limits, local, lookup, media, mutate, read,
    send,
};

/// The set of tools a profile serves.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
    limiter: Option<Arc<Limiter>>,
}

impl Registry {
    /// Every tool this build ships, under the default account-safety limits.
    pub fn standard() -> Self {
        // Lanes L3b to L3e add their tools here.
        Self::default()
            .with_limiter(Limiter::default())
            .with(read::ListConversations)
            .with(read::GetOwnNumber)
            .with(read::StoreStats)
            .with(read::GetUnread)
            .with(read::GetConversation)
            .with(read::SearchMessages)
            .with(send::SendMessage)
            .with(send::SendGroupMessage)
            .with(send::SendNoteToSelf)
            .with(send::SetTyping)
            .with(send::SendReadReceipt)
            .with(mutate::ReactToMessage)
            .with(mutate::EditMessage)
            .with(mutate::DeleteMessage)
            .with(mutate::DeleteGroupMessage)
            .with(directory::ListContacts)
            .with(directory::ListGroups)
            .with(directory::ListAttachments)
            .with(directory::GetAttachment)
            .with(local::ExportMessages)
            .with(local::MarkAsUnread)
            .with(local::ClearLocalStore)
            .with(local::PruneStore)
            .with(lookup::ListDevices)
            .with(lookup::ListIdentities)
            .with(lookup::GetAvatar)
            .with(media::SendAttachment)
            .with(media::SendGroupAttachment)
            .with(media::SendSticker)
            .with(media::SendGroupSticker)
            .with(lookup::GetProfile)
    }

    /// Applies account-safety limits to every send a tool makes.
    pub fn with_limiter(mut self, limiter: Limiter) -> Self {
        self.limiter = Some(Arc::new(limiter));
        self
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Every registered tool, regardless of who's asking.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools.iter()
    }

    /// The tools this caller may see: write tools vanish in read-only mode and
    /// primary-only tools vanish on linked profiles.
    pub fn visible(&self, ctx: &ToolContext) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.tools
            .iter()
            .filter(|t| allowed(t.as_ref(), ctx).is_ok())
    }

    /// Runs `name`, refusing it under the same rules that hide it.
    pub async fn call(
        &self,
        ctx: &ToolContext,
        name: &str,
        input: Value,
    ) -> Result<Value, ToolError> {
        let Some(tool) = self.tools.iter().find(|t| t.name() == name) else {
            return Err(ToolError::Unknown(name.to_owned()));
        };
        allowed(tool.as_ref(), ctx)?;
        let Some(fields) = input.as_object() else {
            return Err(ToolError::InvalidInput(format!(
                "\"{name}\" takes a JSON object, got {input}"
            )));
        };
        let schema = tool.input_schema();
        if let Some(unknown) = fields
            .keys()
            .find(|k| schema["properties"].get(k).is_none())
        {
            return Err(ToolError::InvalidInput(format!(
                "\"{name}\" has no parameter \"{unknown}\""
            )));
        }
        let ctx = &limits::guard(self.limiter.as_ref(), ctx);
        tool.call(ctx, input).await
    }
}

fn allowed(tool: &dyn Tool, ctx: &ToolContext) -> Result<(), ToolError> {
    if tool.writes() && ctx.access == AccessMode::ReadOnly {
        return Err(ToolError::ReadOnly(tool.name()));
    }
    if tool.needs_primary() && ctx.kind == ProfileKind::Linked {
        return Err(ToolError::NeedsPrimary(tool.name()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use relay_signal::Outgoing;
    use serde_json::json;

    use super::*;
    use crate::testing::*;

    /// Tools the boundary sweep can't call meaningfully with probe inputs, and why.
    const EXCLUDED: &[&str] = &[];

    struct Sender;

    #[async_trait]
    impl Tool for Sender {
        fn name(&self) -> &'static str {
            "probe_send"
        }
        fn description(&self) -> &'static str {
            "test-only write tool"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn writes(&self) -> bool {
            true
        }
        fn needs_primary(&self) -> bool {
            true
        }
        async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
            let me = ctx.signal.account().await?.service_id;
            ctx.signal
                .send(Recipient::Contact(me), Outgoing::Typing { started: true })
                .await?;
            Ok(json!({}))
        }
    }

    /// The registry, not the tool, must stop a write in read-only mode: the
    /// probe tool would happily send if asked.
    #[tokio::test]
    async fn read_only_refuses_a_write_tool_that_would_otherwise_send() {
        let registry = Registry::default().with(Sender);
        let (ctx, signal) = ctx(AccessMode::ReadOnly, ProfileKind::Primary);
        let result = registry.call(&ctx, "probe_send", json!({})).await;
        assert!(
            matches!(result, Err(ToolError::ReadOnly(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "read-only call still sent");
        assert_eq!(registry.visible(&ctx).count(), 0, "write tool still listed");
    }

    /// Primary-only tools must be hidden and refused on a linked device.
    #[tokio::test]
    async fn linked_profiles_never_see_or_run_primary_only_tools() {
        let registry = Registry::default().with(Sender);
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Linked);
        let result = registry.call(&ctx, "probe_send", json!({})).await;
        assert!(
            matches!(result, Err(ToolError::NeedsPrimary(_))),
            "got {result:?}"
        );
        assert!(signal.sent().is_empty(), "linked call still sent");
    }

    /// Sweep every shipped tool: in read-only mode no write tool may be listed
    /// or reach Signal, and no tool may panic or succeed on non-object input.
    /// New tools are covered by being registered.
    #[tokio::test]
    async fn every_standard_tool_obeys_read_only_and_rejects_non_objects() {
        let registry = Registry::standard();
        let (ro, signal) = ctx(AccessMode::ReadOnly, ProfileKind::Primary);
        for tool in registry.all().filter(|t| !EXCLUDED.contains(&t.name())) {
            let name = tool.name();
            if tool.writes() {
                let listed = registry.visible(&ro).any(|t| t.name() == name);
                assert!(!listed, "write tool {name} visible in read-only mode");
                let result = registry.call(&ro, name, json!({})).await;
                assert!(
                    matches!(result, Err(ToolError::ReadOnly(_))),
                    "{name} in read-only mode returned {result:?}"
                );
            }
            for probe in [json!(null), json!([]), json!("x"), json!(1)] {
                let (full, _) = ctx(AccessMode::Full, ProfileKind::Primary);
                let result = registry.call(&full, name, probe.clone()).await;
                assert!(
                    matches!(result, Err(ToolError::InvalidInput(_))),
                    "{name} accepted non-object input {probe}: {result:?}"
                );
            }
        }
        assert!(signal.sent().is_empty(), "a read-only sweep reached Signal");
    }

    /// A value the property's schema accepts, so each probe below breaks
    /// exactly one field of an otherwise valid input.
    fn sample(name: &str, prop: &Value) -> Value {
        if let Some(first) = prop["enum"].get(0) {
            return first.clone();
        }
        match prop["format"].as_str() {
            Some("signal-recipient" | "signal-service-id") => json!(sid(2).to_string()),
            Some("signal-group-id") => json!("ab".repeat(32)),
            Some("signal-sticker-pack") => json!("ab".repeat(16)),
            Some("e164") => json!("+15550000002"),
            Some("iso-datetime") => json!("2024-01-01T00:00:00Z"),
            Some(other) => panic!("{name}: no sample for format \"{other}\"; add one here"),
            None => match prop["type"].as_str() {
                Some("string") => json!("x"),
                Some("integer") => prop["minimum"].clone(),
                Some("boolean") => json!(true),
                Some("array") => json!([sample(&format!("{name}[]"), &prop["items"])]),
                Some("object") => Value::Object(object_probes(name, prop).0),
                other => panic!("{name}: no sample for type {other:?}; add one here"),
            },
        }
    }

    /// Values the property's schema refuses: wrong types, empty strings,
    /// malformed ids, integers outside the declared bounds, arrays that are
    /// empty, too long or hold a bad item, and objects with one bad field.
    fn bad_values(name: &str, prop: &Value) -> Vec<Value> {
        let mut bad = vec![json!(null), json!([]), json!({})];
        match prop["type"].as_str() {
            Some("string") => {
                bad.extend([json!(1), json!(true), json!(""), json!(" ")]);
                let malformed_ids = [
                    json!("not-an-id"),
                    json!("+"),
                    json!("+1"),
                    json!("+15559999999"),
                    json!("g".repeat(64)),
                    json!(format!("{}-", sid(2))),
                    json!("é".repeat(32)),
                ];
                if let Some(choices) = prop["enum"].as_array() {
                    bad.push(json!("not-a-choice"));
                    bad.extend(
                        choices
                            .iter()
                            .filter_map(|c| Some(json!(c.as_str()?.to_uppercase()))),
                    );
                }
                match prop["format"].as_str() {
                    Some("signal-recipient") => bad.extend(malformed_ids),
                    // A person-only field must also refuse a well-formed group id.
                    Some("signal-service-id") => {
                        bad.extend(malformed_ids);
                        bad.push(json!("ab".repeat(32)));
                    }
                    // A group-only field must also refuse a well-formed person.
                    Some("signal-group-id") => {
                        bad.extend(malformed_ids);
                        bad.push(json!(sid(2).to_string()));
                    }
                    // A well-formed stranger's number is valid here, unlike in
                    // a contact field, so only malformed numbers are refused.
                    Some("e164") => bad.extend([
                        json!("not-an-id"),
                        json!("+"),
                        json!("+1"),
                        json!("15550000002"),
                        json!("+1555000000a"),
                        json!(sid(2).to_string()),
                    ]),
                    Some("signal-sticker-pack") => bad.extend([
                        json!("g".repeat(32)),
                        json!("ab".repeat(15)),
                        json!("ab".repeat(32)),
                        json!(sid(2).to_string()),
                    ]),
                    Some("iso-datetime") => bad.extend([
                        json!("yesterday"),
                        json!("2024-13-01"),
                        json!("2024-02-30"),
                        json!("2024-01-01T25:00:00Z"),
                    ]),
                    _ => {}
                }
            }
            Some("integer") => {
                let (Some(min), Some(max)) = (prop["minimum"].as_i64(), prop["maximum"].as_i64())
                else {
                    panic!("{name}: integer properties must declare minimum and maximum: {prop}");
                };
                bad.extend([
                    json!("1"),
                    json!(1.5),
                    json!(true),
                    json!(min - 1),
                    json!(max + 1),
                    json!(-1),
                    json!(i64::MIN),
                    json!(u64::MAX),
                    json!(1e300),
                ]);
            }
            Some("boolean") => bad.extend([json!("true"), json!(1)]),
            // An empty array is already refused above, so arrays must not be empty.
            Some("array") => {
                let (Some(1..), Some(max)) = (prop["minItems"].as_u64(), prop["maxItems"].as_u64())
                else {
                    panic!(
                        "{name}: array properties must declare minItems >= 1 and maxItems: {prop}"
                    );
                };
                let item = sample(&format!("{name}[]"), &prop["items"]);
                let too_many = vec![item; max as usize + 1];
                bad.extend([json!("x"), json!(1), json!(true), json!(too_many)]);
                bad.extend(
                    bad_values(&format!("{name}[]"), &prop["items"])
                        .into_iter()
                        .map(|v| json!([v])),
                );
            }
            Some("object") => {
                assert!(
                    prop["required"].as_array().is_some_and(|r| !r.is_empty()),
                    "{name}: nested objects must require a field, or {{}} would be valid: {prop}"
                );
                bad.extend([json!(1), json!("x")]);
                bad.extend(object_probes(name, prop).1.into_iter().map(Value::Object));
            }
            other => panic!("{name}: no bad values for type {other:?}; add some here"),
        }
        bad
    }

    type Object = serde_json::Map<String, Value>;

    /// A valid object for `schema`, and variants of it that each break one
    /// thing: an unknown field, a missing required field, or one bad value.
    fn object_probes(name: &str, schema: &Value) -> (Object, Vec<Object>) {
        let props = schema["properties"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let required: Vec<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let valid: Object = props
            .iter()
            .map(|(k, p)| (k.clone(), sample(&format!("{name}.{k}"), p)))
            .collect();

        let mut probes = vec![];
        let mut unknown = valid.clone();
        unknown.insert("no_such_field".into(), json!(1));
        probes.push(unknown);
        for field in &required {
            let mut missing = valid.clone();
            missing.remove(*field);
            probes.push(missing);
        }
        for (k, p) in &props {
            for bad in bad_values(&format!("{name}.{k}"), p) {
                if bad.is_null() && !required.contains(&k.as_str()) {
                    continue;
                }
                let mut input = valid.clone();
                input.insert(k.clone(), bad);
                probes.push(input);
            }
        }
        (valid, probes)
    }

    /// Stores the messages a tool's sample input can name, so tools that act
    /// on a stored message (react, edit, delete) take their valid path on the
    /// sample: one from this account and one from the sample contact, both at
    /// the smallest timestamp, in the sample's conversation.
    // NOTE: the conversation must follow the sample. This account's message at
    // that timestamp can exist only once, and a group tool needs it in the
    // group while a DM tool needs it in the DM.
    async fn seed(ctx: &ToolContext, sample: &serde_json::Map<String, Value>) {
        let conversation = match sample.get("group_id").and_then(Value::as_str) {
            Some(g) => Recipient::Group(GroupId::parse(g).expect("sample group id")),
            None => Recipient::Contact(sid(2)),
        };
        for (author, from_me) in [(sid(ME), true), (sid(2), false)] {
            let message = Message {
                id: MessageId {
                    author,
                    sent_at_ms: 1,
                },
                conversation,
                sender: author,
                from_me,
                ..incoming(2, 1, "sample target")
            };
            ctx.store.insert(&message).await.expect("seed store");
        }
    }

    /// Calls `tool` on a fresh full-access primary profile in its own task, so
    /// a panic is reported against the tool and input that caused it.
    async fn probe(
        registry: &Registry,
        tool: &str,
        sample: &serde_json::Map<String, Value>,
        input: Value,
    ) -> (Result<Value, ToolError>, usize) {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        seed(&ctx, sample).await;
        let (registry, name, sent_input) = (registry.clone(), tool.to_owned(), input.clone());
        let task = tokio::spawn(async move { registry.call(&ctx, &name, sent_input).await });
        match task.await {
            Ok(result) => (result, signal.sent().len()),
            Err(e) => panic!("{tool} panicked on {input}: {e}"),
        }
    }

    /// Sweep every shipped tool with boundary inputs derived from its own
    /// schema, nested arrays and objects included: missing required fields,
    /// unknown fields, wrong types, empty strings, malformed ids, out-of-range
    /// and huge numbers. Each must be refused as InvalidInput, never a panic,
    /// an Internal error, or a send.
    /// New tools are covered by being registered.
    #[tokio::test]
    async fn every_standard_tool_refuses_boundary_inputs_as_invalid_input() {
        let registry = Registry::standard();
        for tool in registry.all().filter(|t| !EXCLUDED.contains(&t.name())) {
            let name = tool.name();
            let schema = tool.input_schema();
            assert_eq!(
                schema["additionalProperties"],
                json!(false),
                "{name} must refuse unknown fields in its schema: {schema}"
            );
            let (valid, probes) = object_probes(name, &schema);

            let (result, _) = probe(&registry, name, &valid, Value::Object(valid.clone())).await;
            assert!(
                !matches!(result, Err(ToolError::InvalidInput(_))),
                "{name} refused its own schema's sample input {valid:?}: {result:?}"
            );

            for input in probes {
                let input = Value::Object(input);
                let (result, sent) = probe(&registry, name, &valid, input.clone()).await;
                assert!(
                    matches!(result, Err(ToolError::InvalidInput(_))),
                    "{name} did not refuse {input} as invalid: {result:?}"
                );
                assert_eq!(sent, 0, "{name} reached Signal on invalid input {input}");
            }
        }
    }
}
