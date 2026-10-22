//! Setup as data: states a front end renders and events it reports, with no UI
//! code, so the Tauri window and the Linux terminal flow behave identically and
//! setup is tested without either.

pub mod claude;
mod machine;
mod profiles;
mod registration;
mod uninstall;

pub use machine::{AccountChoice, ClientOutcome, Command, SetupEvent, SetupState, transition};
pub use profiles::Profiles;
pub use registration::captcha_token;
pub use uninstall::{Step, StepOutcome, run_steps, uninstall_steps};

pub mod prelude {
    pub use crate::{
        AccountChoice, ClientOutcome, Command, SetupEvent, SetupState, Step, StepOutcome,
        claude::{ClaudeClient, ClientReport, ClientStatus, Clients, ServerEntry},
        run_steps, transition, uninstall_steps,
    };
}
