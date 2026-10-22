//! Parsing and bounds for tool inputs, shared by every tool module so the same
//! field means the same thing and is refused the same way in every tool.

use relay_core::prelude::*;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{ToolContext, ToolError};

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
        "description": "A contact's phone number (E.164, e.g. +15551234567) or service id \
                        (UUID), or a group id (64 hex characters), as list_conversations \
                        returns them",
    })
}

/// A conversation named the way list_conversations names it, or by a
/// contact's phone number.
pub async fn recipient(ctx: &ToolContext, s: &str) -> Result<Recipient, ToolError> {
    if is_e164(s) {
        let contacts = ctx.signal.contacts().await?;
        let Some(contact) = contacts
            .iter()
            .find(|c| c.phone_number.as_deref() == Some(s))
        else {
            return Err(ToolError::InvalidInput(format!(
                "no contact has the number \"{s}\""
            )));
        };
        return Ok(Recipient::Contact(contact.service_id));
    }
    if let Ok(id) = ServiceId::parse(s) {
        return Ok(Recipient::Contact(id));
    }
    if let Ok(id) = GroupId::parse(s) {
        return Ok(Recipient::Group(id));
    }
    Err(ToolError::InvalidInput(format!(
        "\"{s}\" is not a phone number, a contact's service id or a group id"
    )))
}

pub fn contact_schema(description: &str) -> Value {
    json!({
        "type": "string",
        "format": "signal-service-id",
        "description": format!("{description}: a contact's phone number (E.164) or service id (UUID)"),
    })
}

/// A person, by phone number or service id; a group id is refused.
pub async fn contact(ctx: &ToolContext, s: &str) -> Result<ServiceId, ToolError> {
    match recipient(ctx, s).await? {
        Recipient::Contact(id) => Ok(id),
        Recipient::Group(_) => Err(ToolError::InvalidInput(format!(
            "\"{s}\" is a group id; this needs a person"
        ))),
    }
}

/// A message, named by the id object the read tools return with it.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageRef {
    author: ServiceId,
    sent_at_ms: SentAt,
}

impl MessageRef {
    pub fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "author": {
                    "type": "string",
                    "format": "signal-service-id",
                    "description": "The service id of who wrote the message",
                },
                "sent_at_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": SentAt::MAX,
                    "description": "The author's send time in epoch milliseconds",
                },
            },
            "required": ["author", "sent_at_ms"],
            "additionalProperties": false,
            "description": "A message id exactly as get_conversation, get_unread or \
                            search_messages return it",
        })
    }

    pub fn id(self) -> MessageId {
        MessageId {
            author: self.author,
            sent_at_ms: self.sent_at_ms.0,
        }
    }
}

/// A send time in epoch ms, capped at JSON's largest exact integer.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "u64")]
struct SentAt(u64);

impl SentAt {
    const MAX: u64 = (1 << 53) - 1;
}

impl TryFrom<u64> for SentAt {
    type Error = String;
    fn try_from(n: u64) -> Result<Self, String> {
        if n > Self::MAX {
            return Err(format!("sent_at_ms must be 0 to {}, got {n}", Self::MAX));
        }
        Ok(Self(n))
    }
}

/// Text that must say something: refused when empty or only whitespace.
pub fn non_blank(field: &str, s: String) -> Result<String, ToolError> {
    if s.trim().is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "{field} must not be empty"
        )));
    }
    Ok(s)
}

/// A plus sign then 8 to 15 digits, the E.164 range Signal accepts.
fn is_e164(s: &str) -> bool {
    let Some(digits) = s.strip_prefix('+') else {
        return false;
    };
    (8..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

/// A moment in time, given as an ISO 8601 date (`2024-01-01`) or UTC datetime
/// (`2024-01-01T09:30:00Z`, seconds and `Z` optional), held as epoch ms.
// NOTE: read as UTC, unlike upstream signal-mcp, which used the server's local
// time. The agent may run in another timezone than the client, so local time
// would silently shift every window by the difference.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "String")]
pub struct Instant(u64);

impl Instant {
    pub fn schema(description: &str) -> Value {
        json!({
            "type": "string",
            "format": "iso-datetime",
            "description": format!("{description}: an ISO date (2024-01-01) or UTC datetime (2024-01-01T09:30:00Z)"),
        })
    }

    pub fn ms(self) -> u64 {
        self.0
    }
}

impl TryFrom<String> for Instant {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        epoch_ms(&s)
            .map(Self)
            .ok_or_else(|| format!("\"{s}\" is not an ISO date or UTC datetime"))
    }
}

fn epoch_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = match s.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let (y, m, d) = (
        digits(parts.next()?, 4)?,
        digits(parts.next()?, 2)?,
        digits(parts.next()?, 2)?,
    );
    if parts.next().is_some() || y < 1970 || !(1..=12).contains(&m) || d < 1 || d > days_in(y, m) {
        return None;
    }
    let mut secs = 0;
    if let Some(time) = time {
        let mut parts = time.split(':');
        let (h, min) = (digits(parts.next()?, 2)?, digits(parts.next()?, 2)?);
        let sec = parts.next().map_or(Some(0), |p| digits(p, 2))?;
        if parts.next().is_some() || h > 23 || min > 59 || sec > 59 {
            return None;
        }
        secs = u64::from(h * 3600 + min * 60 + sec);
    }
    Some((days_since_epoch(y, m, d) * 86_400 + secs) * 1000)
}

fn digits(s: &str, len: usize) -> Option<u32> {
    if s.len() != len || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn days_in(y: u32, m: u32) -> u32 {
    let leap = y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400));
    match m {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Howard Hinnant's days_from_civil, for years from 1970 on.
fn days_since_epoch(y: u32, m: u32, d: u32) -> u64 {
    let y = u64::from(if m <= 2 { y - 1 } else { y });
    let (m, d) = (u64::from(m), u64::from(d));
    let era = y / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Date math must match the calendar across leap days and the century
    /// rules, or every since/until window drifts by a day. Expected values are
    /// from Python's datetime.
    #[test]
    fn instants_land_on_the_right_epoch_ms_across_leap_rules() {
        for (s, ms) in [
            ("1970-01-01", 0),
            ("2000-02-29", 951_782_400_000),
            ("2000-03-01", 951_868_800_000),
            ("2024-02-29", 1_709_164_800_000),
            ("2100-03-01", 4_107_542_400_000),
            ("2099-12-31T23:59:59Z", 4_102_444_799_000),
        ] {
            assert_eq!(epoch_ms(s), Some(ms), "{s}");
        }
        for s in ["2100-02-29", "1900-01-01", "2023-02-29", "2024-01-01T24:00"] {
            assert_eq!(epoch_ms(s), None, "accepted {s}");
        }
    }
}
