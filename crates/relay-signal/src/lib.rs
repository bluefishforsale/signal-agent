//! The one seam between this project and the Signal protocol. Everything above
//! it talks to `SignalBackend`; presage lives behind it, and `FakeSignal` stands
//! in for it in every test so no lane needs a real account.

mod backend;
mod convert;
mod fake;
mod files;
mod presage_backend;

pub use backend::{
    Account, Contact, Device, Group, Identity, Incoming, LinkEvent, Outgoing, Provisioner,
    RateLimited, RegistrationStep, SignalBackend, Trust,
};
pub use fake::FakeSignal;
pub use files::{SendRoots, SendableFile};
pub use presage_backend::{PresageProvisioner, PresageSignal, Servers};

pub mod prelude {
    pub use crate::{
        Account, Contact, Device, FakeSignal, Group, Identity, Incoming, LinkEvent, Outgoing,
        Provisioner, RateLimited, RegistrationStep, SendRoots, SendableFile, SignalBackend, Trust,
    };
}
