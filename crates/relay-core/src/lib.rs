//! The vocabulary every crate agrees on, so lanes can be built in parallel
//! against one set of types instead of converting between private ones.

mod history_key;
mod ids;
mod message;
mod paths;
mod profile;

pub use history_key::LinkAndSyncKey;
pub use ids::{GroupId, MessageId, Recipient, ServiceId, StickerPackId};
pub use message::{AttachmentRef, Conversation, Message};
pub use paths::ProfilePaths;
pub use profile::{AccessMode, ProfileKind, ProfileName};

pub mod prelude {
    pub use crate::{
        AccessMode, AttachmentRef, Conversation, GroupId, Message, MessageId, ProfileKind,
        ProfileName, ProfilePaths, Recipient, ServiceId, StickerPackId,
    };
}
