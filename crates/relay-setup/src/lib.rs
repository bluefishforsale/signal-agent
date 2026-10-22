//! Setup as data: states a front end renders and events it reports, with no UI
//! code, so the Tauri window and the Linux terminal flow behave identically and
//! setup is tested without either.

mod captcha;
pub mod claude;
pub mod driver;
mod machine;
mod profiles;
mod uninstall;

pub use captcha::{CAPTCHA_PAGE, captcha_navigation_allowed, captcha_token, pasted_captcha_token};
pub use machine::{AccountChoice, ClientOutcome, Command, SetupEvent, SetupState, transition};
pub use profiles::Profiles;
pub use uninstall::{
    APP_ID, DELETE_DATA_LABEL, Footprint, LAUNCHD_LABEL, Leftover, Machine, Roots, Step,
    StepOutcome, checklist, removal_steps, run_steps, stop_agents, uninstall, uninstall_steps,
};

pub mod prelude {
    pub use crate::{
        AccountChoice, CAPTCHA_PAGE, ClientOutcome, Command, Profiles, SetupEvent, SetupState,
        Step, StepOutcome, captcha_navigation_allowed, captcha_token,
        claude::{ClaudeClient, ClientReport, ClientStatus, Clients, ServerEntry},
        pasted_captcha_token, run_steps, transition, uninstall_steps,
    };
}
