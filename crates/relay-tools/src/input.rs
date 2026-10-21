//! Parsing and bounds for tool inputs, shared by every tool module so the same
//! field means the same thing and is refused the same way in every tool.

use relay_core::prelude::*;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::ToolError;

/// Deserializes a tool's input, turning every shape or type mismatch into
/// InvalidInput rather than letting it surface as an internal failure.
pub fn parse<T: DeserializeOwned>(input: Value) -> Result<T, ToolError> {
    serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

/// How many results to return: 1 to 500, 50 when not given.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
pub struct Limit(u32);

impl Limit {
    const MAX: u32 = 500;

    pub fn schema() -> Value {
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": Self::MAX,
            "default": 50,
            "description": "Most results to return, 1 to 500 (default 50)",
        })
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for Limit {
    fn default() -> Self {
        Self(50)
    }
}

impl TryFrom<u64> for Limit {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        match u32::try_from(n) {
            Ok(n @ 1..=Self::MAX) => Ok(Self(n)),
            _ => Err(format!("limit must be 1 to {}, got {n}", Self::MAX)),
        }
    }
}

/// How many of the newest results to skip: 0 to 100000, 0 when not given.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(try_from = "u64")]
pub struct Offset(u32);

impl Offset {
    const MAX: u32 = 100_000;

    pub fn schema() -> Value {
        json!({
            "type": "integer",
            "minimum": 0,
            "maximum": Self::MAX,
            "default": 0,
            "description": "How many results to skip, for paging (default 0)",
        })
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u64> for Offset {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        match u32::try_from(n) {
            Ok(n @ 0..=Self::MAX) => Ok(Self(n)),
            _ => Err(format!("offset must be 0 to {}, got {n}", Self::MAX)),
        }
    }
}

pub fn recipient_schema() -> Value {
    json!({
        "type": "string",
        "format": "signal-recipient",
        "description": "A contact's service id (UUID) or a group id (64 hex characters), \
                        as list_conversations returns them",
    })
}

/// A conversation named the way list_conversations names it.
pub fn recipient(s: &str) -> Result<Recipient, ToolError> {
    if let Ok(id) = ServiceId::parse(s) {
        return Ok(Recipient::Contact(id));
    }
    if let Ok(id) = GroupId::parse(s) {
        return Ok(Recipient::Group(id));
    }
    Err(ToolError::InvalidInput(format!(
        "\"{s}\" is neither a contact's service id nor a group id"
    )))
}
