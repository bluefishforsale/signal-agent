//! Parsing and bounds for tool inputs, shared by every tool module so the same
//! field means the same thing and is refused the same way in every tool.

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
