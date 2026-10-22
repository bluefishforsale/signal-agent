//! Tools that read the sticker packs installed on the account. They answer
//! from the local store, so they reach neither Signal nor its CDN.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolError};

pub struct ListStickerPacks;

#[async_trait]
impl Tool for ListStickerPacks {
    fn name(&self) -> &'static str {
        "list_sticker_packs"
    }
    fn description(&self) -> &'static str {
        "List the sticker packs installed on this account: id (pass it as pack_id to \
         send_sticker, send_group_sticker and get_sticker), title, author, and each sticker's \
         id and emoji. Takes no parameters. Local only: nothing is downloaded or sent."
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, _input: Value) -> Result<Value, ToolError> {
        let packs: Vec<_> = ctx
            .signal
            .sticker_packs()
            .await?
            .iter()
            .map(|p| {
                json!({
                    "id": p.id.to_string(),
                    "title": p.title,
                    "author": p.author,
                    "stickers": p.stickers,
                })
            })
            .collect();
        Ok(json!({ "packs": packs }))
    }
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;
    use serde_json::json;

    use crate::{Registry, testing::*};

    /// A client picks a sticker to send from this list, so every installed
    /// pack must be listed with the id send_sticker takes and each sticker's
    /// number within it.
    #[tokio::test]
    async fn list_sticker_packs_lists_each_installed_pack_with_its_sticker_numbers() {
        let (ctx, _) = ctx_of(|s| {
            s.with_sticker_pack(StickerPackId::new([0xbb; 16]), vec![0])
                .with_sticker_pack(StickerPackId::new([0xaa; 16]), vec![0, 1])
        });
        let out = Registry::standard()
            .call(&ctx, "list_sticker_packs", json!({}))
            .await
            .unwrap();
        let packs: Vec<_> = out["packs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p["id"].clone(), p["stickers"].clone()))
            .collect();
        assert_eq!(
            packs,
            [
                (
                    json!("aa".repeat(16)),
                    json!([{"id": 0, "emoji": null}, {"id": 1, "emoji": null}])
                ),
                (json!("bb".repeat(16)), json!([{"id": 0, "emoji": null}])),
            ],
            "{out}"
        );
    }
}
