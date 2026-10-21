//! The list of tools and the gate every call passes through.

use std::sync::Arc;

use relay_core::prelude::*;
use serde_json::Value;

use crate::{Tool, ToolContext, ToolError};

/// The set of tools a profile serves.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    /// Every tool this build ships.
    pub fn standard() -> Self {
        // Lanes L3 and L3b to L3e add their tools here.
        Self::default()
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
        if !input.is_object() {
            return Err(ToolError::InvalidInput(format!(
                "\"{name}\" takes a JSON object, got {input}"
            )));
        }
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
    use std::sync::Arc;

    use async_trait::async_trait;
    use relay_signal::{Account, FakeSignal, Outgoing};
    use relay_store::MemoryStore;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    /// Tools the boundary sweep can't call meaningfully with probe inputs, and why.
    const EXCLUDED: &[&str] = &[];

    fn ctx(access: AccessMode, kind: ProfileKind) -> (ToolContext, Arc<FakeSignal>) {
        let signal = Arc::new(FakeSignal::new(Account {
            service_id: ServiceId::new(Uuid::from_u128(1)),
            phone_number: None,
            kind,
            device_name: "test".into(),
        }));
        let ctx = ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: signal.clone(),
            access,
            kind,
        };
        (ctx, signal)
    }

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
}
