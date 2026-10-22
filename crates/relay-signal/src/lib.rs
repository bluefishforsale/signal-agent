//! The one seam between this project and the Signal protocol. Everything above
//! it talks to `SignalBackend`; presage lives behind it, and `FakeSignal` stands
//! in for it in every test so no lane needs a real account.

mod backend;
mod convert;
mod fake;
mod files;
mod presage_backend;
mod transfer;

pub use backend::{
    Account, Contact, Device, DiscoveryUnsupported, Group, HISTORY_ARCHIVE_FILE, HistoryTransfer,
    Identity, Incoming, LinkEvent, Outgoing, ProfileUpdate, Provisioner, RateLimited,
    RegistrationStep, RequestResponse, SignalBackend, StickerImage, StickerInfo, StickerPack,
    Trust,
};
pub use fake::FakeSignal;
pub use files::{SendRoots, SendableFile};
pub use presage_backend::{PresageProvisioner, PresageSignal, Servers};

pub mod prelude {
    pub use crate::{
        Account, Contact, Device, DiscoveryUnsupported, FakeSignal, Group, HistoryTransfer,
        Identity, Incoming, LinkEvent, Outgoing, ProfileUpdate, Provisioner, RateLimited,
        RegistrationStep, RequestResponse, SendRoots, SendableFile, SignalBackend, StickerImage,
        StickerInfo, StickerPack, Trust,
    };
}
