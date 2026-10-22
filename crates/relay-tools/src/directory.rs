//! Tools that look up who and what the account knows: contacts, groups and
//! received attachments. They read from Signal's side of the seam or the local
//! store, and never send.

use async_trait::async_trait;
use relay_signal::Contact;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{non_blank, parse},
};

pub struct ListContacts;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListContactsInput {
    search: Option<String>,
}

#[async_trait]
impl Tool for ListContacts {
    fn name(&self) -> &'static str {
        "list_contacts"
    }
    fn description(&self) -> &'static str {
        "List the contacts this account knows. search keeps only contacts whose name or phone \
         number contains it (case-insensitive). Returns number (E.164, null if unknown), \
         service_id and name for each. Pass a number or service_id to any tool that takes a \
         contact. No messages sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "search": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Keep contacts whose name or number contains this (case-insensitive)",
                },
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let ListContactsInput { search } = parse(input)?;
        let needle = search
            .map(|s| non_blank("search", s))
            .transpose()?
            .map(|s| s.to_lowercase());
        let contacts: Vec<_> = ctx
            .signal
            .contacts()
            .await?
            .iter()
            .filter(|c| needle.as_deref().is_none_or(|n| matches(c, n)))
            .map(|c| {
                json!({
                    "number": c.phone_number,
                    "service_id": c.service_id.to_string(),
                    "name": c.name,
                })
            })
            .collect();
        Ok(json!({ "contacts": contacts }))
    }
}

fn matches(contact: &Contact, needle: &str) -> bool {
    [&contact.name, &contact.phone_number]
        .into_iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(needle))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use relay_signal::Contact;

    use crate::{Registry, testing::*};
    use relay_core::prelude::*;

    fn contact(n: u128, name: &str, number: &str) -> Contact {
        Contact {
            service_id: sid(n),
            name: Some(name.into()),
            phone_number: Some(number.into()),
        }
    }

    /// Clients resolve "text Ada" through search, so it must match a name or a
    /// number fragment regardless of case, and leave everyone else out.
    #[tokio::test]
    async fn list_contacts_search_matches_name_or_number_ignoring_case() {
        let contacts = vec![
            contact(2, "Ada Lovelace", "+15550000002"),
            contact(3, "Grace Hopper", "+15550000003"),
            contact(4, "Alan Turing", "+15559990004"),
        ];
        let (ctx, _) = ctx_with(AccessMode::ReadOnly, ProfileKind::Linked, contacts);
        let registry = Registry::standard();
        let names = |out: serde_json::Value| -> Vec<String> {
            out["contacts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let by_name = registry
            .call(&ctx, "list_contacts", json!({"search": "LOVE"}))
            .await
            .unwrap();
        assert_eq!(names(by_name), ["Ada Lovelace"]);
        let by_number = registry
            .call(&ctx, "list_contacts", json!({"search": "999"}))
            .await
            .unwrap();
        assert_eq!(names(by_number), ["Alan Turing"]);
        let everyone = registry
            .call(&ctx, "list_contacts", json!({}))
            .await
            .unwrap();
        assert_eq!(
            everyone["contacts"].as_array().unwrap().len(),
            3,
            "{everyone}"
        );
    }
}
