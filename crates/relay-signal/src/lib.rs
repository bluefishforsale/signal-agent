//! The one seam between this project and the Signal protocol. Everything above
//! it talks to `SignalBackend`; presage lives behind it, and `FakeSignal` stands
//! in for it in every test so no lane needs a real account.

mod backend;
mod fake;

pub use backend::{
    Account, Contact, Group, Incoming, LinkEvent, Outgoing, Provisioner, RegistrationStep,
    SignalBackend,
};
pub use fake::FakeSignal;

pub mod prelude {
    pub use crate::{
        Account, Contact, FakeSignal, Group, Incoming, LinkEvent, Outgoing, Provisioner,
        RegistrationStep, SignalBackend,
    };
}
