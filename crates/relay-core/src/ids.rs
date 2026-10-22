//! Signal identifiers as always-valid newtypes, so a malformed id is rejected
//! where it enters (tool input, store rows) rather than deep in protocol code.

use std::fmt;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A Signal account's service identifier (ACI).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ServiceId(Uuid);

impl ServiceId {
    pub fn new(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let uuid =
            Uuid::parse_str(s).with_context(|| format!("\"{s}\" is not a Signal service id"))?;
        Ok(Self(uuid))
    }

    pub fn uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServiceId({})", self.0)
    }
}

/// A Signal group's identifier: 32 bytes, written as 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct GroupId([u8; 32]);

impl GroupId {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        parse_hex(s, "group id").map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GroupId({self})")
    }
}

/// An installed sticker pack's identifier: 16 bytes, written as 32 lowercase
/// hex characters as in Signal's sticker links.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StickerPackId([u8; 16]);

impl StickerPackId {
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn parse(s: &str) -> anyhow::Result<Self> {
        parse_hex(s, "sticker pack id").map(Self)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for StickerPackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for StickerPackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StickerPackId({self})")
    }
}

/// Exactly `N` bytes from `2 * N` hex characters, ignoring surrounding space.
fn parse_hex<const N: usize>(s: &str, what: &str) -> anyhow::Result<[u8; N]> {
    let s = s.trim();
    if s.len() != N * 2 {
        bail!(
            "{what} \"{s}\" must be {} hex characters, got {}",
            N * 2,
            s.len()
        );
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let Some(pair) = s.get(i * 2..i * 2 + 2) else {
            bail!("{what} \"{s}\" is not hex");
        };
        *byte =
            u8::from_str_radix(pair, 16).with_context(|| format!("{what} \"{s}\" is not hex"))?;
    }
    Ok(bytes)
}

/// Who a message is to, or which conversation it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Recipient {
    Contact(ServiceId),
    Group(GroupId),
}

/// A message's identity. Signal identifies a message by its author and the
/// author's send timestamp, which is what reactions, edits and deletes reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MessageId {
    pub author: ServiceId,
    pub sent_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// A group id must survive its own text form, since tools take it as a string
        /// and the store keys conversations by it.
        #[test]
        fn group_id_round_trips_through_hex(bytes in any::<[u8; 32]>()) {
            let id = GroupId::new(bytes);
            let parsed = GroupId::parse(&id.to_string()).expect("own display is valid");
            prop_assert_eq!(parsed, id);
        }

        /// Arbitrary strings must never panic the parser; tool input is untrusted.
        #[test]
        fn group_id_parse_never_panics_on_garbage(s in ".*") {
            let _ = GroupId::parse(&s);
        }

        /// A sticker pack id must survive its own text form, since tools take it
        /// as the hex in a sticker link.
        #[test]
        fn sticker_pack_id_round_trips_through_hex(bytes in any::<[u8; 16]>()) {
            let id = StickerPackId::new(bytes);
            let parsed = StickerPackId::parse(&id.to_string()).expect("own display is valid");
            prop_assert_eq!(parsed, id);
        }

        /// Arbitrary strings must never panic the sticker pack parser either.
        #[test]
        fn sticker_pack_id_parse_never_panics_on_garbage(s in ".*") {
            let _ = StickerPackId::parse(&s);
        }
    }

    /// A multi-byte character at a pair boundary must be an error, not a panic from slicing.
    #[test]
    fn group_id_rejects_non_ascii_without_panicking() {
        let s = format!("{}é{}", "a".repeat(31), "b".repeat(31));
        assert!(GroupId::parse(&s).is_err(), "accepted non-hex input {s:?}");
    }
}
