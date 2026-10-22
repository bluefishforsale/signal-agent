//! Tools that read the sticker packs installed on the account. They answer
//! from the local store, so they reach neither Signal nor its CDN.

use anyhow::Context;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Tool, ToolContext, ToolError,
    input::{
        parse, sticker_key, sticker_key_schema, sticker_link, sticker_link_schema, sticker_pack,
        sticker_pack_schema,
    },
    media::sticker_id_schema,
};

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

pub struct GetSticker;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetStickerInput {
    pack_id: String,
    sticker_id: u32,
}

#[async_trait]
impl Tool for GetSticker {
    fn name(&self) -> &'static str {
        "get_sticker"
    }
    fn description(&self) -> &'static str {
        "Fetch one sticker's image from a pack installed on this account: its content_type and \
         the image as base64. pack_id and sticker_id come from list_sticker_packs or a received \
         sticker; a pack that isn't installed, or a sticker it doesn't hold, is an error. Local \
         only: nothing is downloaded or sent."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pack_id": sticker_pack_schema(),
                "sticker_id": sticker_id_schema(),
            },
            "required": ["pack_id", "sticker_id"],
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: GetStickerInput = parse(input)?;
        let pack = sticker_pack(&input.pack_id)?;
        let image = ctx
            .signal
            .sticker(pack, input.sticker_id)
            .await?
            .with_context(|| {
                format!(
                    "no image for sticker {} of pack {pack}; list_sticker_packs shows what is installed",
                    input.sticker_id
                )
            })?;
        Ok(json!({
            "content_type": image.content_type,
            "base64": STANDARD.encode(image.bytes),
        }))
    }
}

pub struct AddStickerPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddStickerPackInput {
    url: Option<String>,
    pack_id: Option<String>,
    pack_key: Option<String>,
}

#[async_trait]
impl Tool for AddStickerPack {
    fn name(&self) -> &'static str {
        "add_sticker_pack"
    }
    fn description(&self) -> &'static str {
        "Install a sticker pack on this account, downloading it from Signal and telling your \
         other devices. Name it by url, Signal's https://signal.art/addstickers/ link, or by \
         pack_id and pack_key together; if both forms are given they must name the same pack. \
         Spends one lookup from the daily lookup limit. Returns the pack id to use with \
         send_sticker."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": sticker_link_schema(),
                "pack_id": sticker_pack_schema(),
                "pack_key": sticker_key_schema(),
            },
            "additionalProperties": false,
        })
    }
    fn writes(&self) -> bool {
        true
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, ToolError> {
        let input: AddStickerPackInput = parse(input)?;
        let from_link = input.url.as_deref().map(sticker_link).transpose()?;
        let given = match (input.pack_id, input.pack_key) {
            (Some(id), Some(key)) => Some((sticker_pack(&id)?, sticker_key(&key)?)),
            (None, None) => None,
            _ => {
                return Err(ToolError::InvalidInput(
                    "pack_id and pack_key go together".into(),
                ));
            }
        };
        let (pack, key) = match (from_link, given) {
            (Some(link), Some(given)) if link != given => {
                return Err(ToolError::InvalidInput(
                    "url and pack_id/pack_key name different packs".into(),
                ));
            }
            (Some(pack), _) | (None, Some(pack)) => pack,
            (None, None) => {
                return Err(ToolError::InvalidInput(
                    "name the pack by url, or by pack_id and pack_key".into(),
                ));
            }
        };
        ctx.signal.install_sticker_pack(pack, key).await?;
        Ok(json!({ "pack_id": pack.to_string() }))
    }
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use relay_core::prelude::*;
    use relay_signal::SignalBackend;
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

    /// get_sticker hands a client the image to look at, so it must return the
    /// stored bytes with their type, and a sticker that isn't installed must
    /// fail rather than come back as an empty image.
    #[tokio::test]
    async fn get_sticker_returns_an_installed_stickers_image_and_fails_otherwise() {
        let pack = StickerPackId::new([0xaa; 16]);
        let (ctx, _) = ctx_of(|s| s.with_sticker_pack(pack, vec![0, 1]));
        let registry = Registry::standard();
        let out = registry
            .call(
                &ctx,
                "get_sticker",
                json!({"pack_id": pack.to_string(), "sticker_id": 1}),
            )
            .await
            .unwrap();
        assert_eq!(
            out,
            json!({
                "content_type": "image/webp",
                "base64": STANDARD.encode(format!("{pack}/1")),
            })
        );
        for (pack_id, sticker_id) in [(pack.to_string(), 9), ("bb".repeat(16), 0)] {
            let result = registry
                .call(
                    &ctx,
                    "get_sticker",
                    json!({"pack_id": pack_id, "sticker_id": sticker_id}),
                )
                .await;
            assert!(result.is_err(), "{pack_id}/{sticker_id}: {result:?}");
        }
    }

    /// A pack arrives as Signal's share link or as its id and key; either
    /// must install that exact pack, and a link that isn't Signal's, or that
    /// disagrees with the id given beside it, must be refused before anything
    /// is downloaded.
    #[tokio::test]
    async fn add_sticker_pack_installs_from_a_signal_link_or_id_and_key_and_nothing_else() {
        let (ctx, signal) = ctx_of(|s| s);
        let registry = Registry::standard();
        let (a, b) = ("aa".repeat(16), "bb".repeat(16));
        let key = "cd".repeat(32);
        let link =
            |id: &str| format!("https://signal.art/addstickers/#pack_id={id}&pack_key={key}");
        for input in [
            json!({"url": link(&a)}),
            json!({"pack_id": b, "pack_key": key}),
            json!({"url": link(&b), "pack_id": b, "pack_key": key}),
        ] {
            registry
                .call(&ctx, "add_sticker_pack", input.clone())
                .await
                .unwrap_or_else(|e| panic!("{input} refused: {e}"));
        }
        for bad in [
            json!({"url": link(&a).replace("https://signal.art", "https://signal.art.evil.example")}),
            json!({"url": link(&a).replace("https", "http")}),
            json!({"url": link(&a).replace("addstickers", "stickers")}),
            json!({"url": link(&a).replace("#", "?")}),
            json!({"url": format!("{}&pack_id={b}", link(&a))}),
            json!({"url": link(&a), "pack_id": b, "pack_key": key}),
            json!({"pack_id": a}),
            json!({"pack_key": key}),
            json!({}),
        ] {
            let result = registry.call(&ctx, "add_sticker_pack", bad.clone()).await;
            assert!(
                matches!(result, Err(crate::ToolError::InvalidInput(_))),
                "{bad} not refused as invalid: {result:?}"
            );
        }
        let installed: Vec<_> = signal
            .sticker_packs()
            .await
            .unwrap()
            .iter()
            .map(|p| p.id.to_string())
            .collect();
        assert_eq!(installed, [a, b]);
    }
}
