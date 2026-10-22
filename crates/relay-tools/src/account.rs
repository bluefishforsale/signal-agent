//! Tools that change how this account appears to every contact. A change here
//! is the one a prompt injection would most want, so these are primary-only
//! and spend the limiter's account-changes budget.

use async_trait::async_trait;
use relay_signal::ProfileUpdate;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{non_blank, parse},
};

pub struct UpdateProfile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateProfileInput {
    given_name: Option<String>,
    family_name: Option<String>,
    about: Option<String>,
    about_emoji: Option<String>,
}

#[async_trait]
impl Tool for UpdateProfile {
    fn name(&self) -> &'static str {
        "update_profile"
    }
    fn description(&self) -> &'static str {
        "Change this account's Signal profile as every contact sees it: given_name, \
         family_name, about text and about_emoji. Fields left out keep their current value; \
         the avatar is kept and can't be changed here. Only on a profile with its own number, \
         never a linked one. At most 3 changes a day by default."
    }
    fn input_schema(&self) -> Value {
        let text = |description: &str| json!({"type": "string", "minLength": 1, "description": description});
        json!({
            "type": "object",
            "properties": {
                "given_name": text("First name"),
                "family_name": text("Last name"),
                "about": text("The about text under the name"),
                "about_emoji": text("One emoji shown with the about text"),
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    fn needs_primary(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: UpdateProfileInput = parse(input)?;
        let field = |name, value: Option<String>| value.map(|v| non_blank(name, v)).transpose();
        let update = ProfileUpdate {
            given_name: field("given_name", input.given_name)?,
            family_name: field("family_name", input.family_name)?,
            about: field("about", input.about)?,
            about_emoji: field("about_emoji", input.about_emoji)?,
        };
        if update == ProfileUpdate::default() {
            return Err(ToolError::InvalidInput(
                "name at least one of given_name, family_name, about or about_emoji".into(),
            ));
        }
        ctx.signal.update_profile(update).await?;
        Ok(json!({ "status": "updated" }))
    }
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use relay_signal::ProfileUpdate;
    use serde_json::json;

    use crate::{Registry, testing::*};

    /// Only the fields asked for may change; the rest must reach the backend
    /// as None so the current profile keeps them.
    #[tokio::test]
    async fn update_profile_sends_only_the_fields_given() {
        let (ctx, signal) = ctx(AccessMode::Full, ProfileKind::Primary);
        Registry::standard()
            .call(
                &ctx,
                "update_profile",
                json!({"about": "answers in Signal", "about_emoji": "🤖"}),
            )
            .await
            .unwrap();
        assert_eq!(
            signal.profile_updates(),
            [ProfileUpdate {
                about: Some("answers in Signal".into()),
                about_emoji: Some("🤖".into()),
                ..ProfileUpdate::default()
            }]
        );
    }
}
