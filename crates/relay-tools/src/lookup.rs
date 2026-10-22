//! Tools that ask Signal about accounts, keys and devices. They reach Signal's
//! servers or the protocol store but never a person, so nothing here sends.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError, input::parse};

pub struct ListDevices;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListDevicesInput {}

#[async_trait]
impl Tool for ListDevices {
    fn name(&self) -> &'static str {
        "list_devices"
    }
    fn description(&self) -> &'static str {
        "List every device linked to this Signal account, as Signal's servers report them: id, \
         name, created_at_ms and last_seen_at_ms (epoch ms), and primary (id 1, the phone the \
         account was registered on). Use it to audit which devices can read the account. Takes \
         no parameters. Nothing is sent."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListDevicesInput {} = parse(input)?;
        let devices: Vec<_> = ctx
            .signal
            .devices()
            .await?
            .into_iter()
            .map(|d| {
                json!({
                    "id": d.id,
                    "name": d.name,
                    "created_at_ms": d.created_at_ms,
                    "last_seen_at_ms": d.last_seen_at_ms,
                    "primary": d.id == 1,
                })
            })
            .collect();
        Ok(json!({ "devices": devices }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use relay_signal::{Account, Device, FakeSignal};
    use relay_store::MemoryStore;
    use serde_json::json;

    use crate::{Registry, ToolContext, testing::*};
    use relay_core::prelude::*;

    /// A read-only linked profile whose Signal knows `signal`'s seeded state.
    fn ctx_of(signal: impl FnOnce(FakeSignal) -> FakeSignal) -> ToolContext {
        let fake = FakeSignal::new(Account {
            service_id: sid(ME),
            phone_number: Some("+15550000001".into()),
            kind: ProfileKind::Linked,
            device_name: "test".into(),
        });
        ToolContext {
            store: Arc::new(MemoryStore::default()),
            signal: Arc::new(signal(fake)),
            access: AccessMode::ReadOnly,
            kind: ProfileKind::Linked,
            send_roots: Arc::default(),
            webhook: None,
        }
    }

    /// Auditing who can read the account is the point of the list, so every
    /// device comes back with its times, and the primary is marked.
    #[tokio::test]
    async fn list_devices_lists_every_device_and_marks_the_primary() {
        let ctx = ctx_of(|s| {
            s.with_devices(vec![
                Device {
                    id: 1,
                    name: Some("phone".into()),
                    created_at_ms: 10,
                    last_seen_at_ms: 20,
                },
                Device {
                    id: 2,
                    name: None,
                    created_at_ms: 30,
                    last_seen_at_ms: 40,
                },
            ])
        });
        let out = Registry::standard()
            .call(&ctx, "list_devices", json!({}))
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({"devices": [
                {"id": 1, "name": "phone", "created_at_ms": 10, "last_seen_at_ms": 20, "primary": true},
                {"id": 2, "name": null, "created_at_ms": 30, "last_seen_at_ms": 40, "primary": false},
            ]})
        );
    }
}
