//! Setup as data: states a front end renders and events it reports, with no UI
//! code, so the Tauri window and the Linux terminal flow behave identically and
//! setup is tested without either.

mod agent_keys;
mod captcha;
pub mod claude;
pub mod driver;
pub mod history;
mod machine;
mod profiles;
mod uninstall;

pub use agent_keys::{AgentKeys, KeyAction, serve_key};
pub use captcha::{CAPTCHA_PAGE, captcha_navigation_allowed, captcha_token, pasted_captcha_token};
pub use machine::{
    AccountChoice, ClientOutcome, Command, HistoryGap, SetupEvent, SetupState, transition,
};
pub use profiles::Profiles;
pub use uninstall::{
    APP_ID, DELETE_DATA_LABEL, Footprint, LAUNCHD_LABEL, Leftover, Machine, Roots, Step,
    StepOutcome, checklist, removal_steps, run_steps, stop_agents, uninstall, uninstall_steps,
};

/// What every front end tells the user while a linked device imports history.
pub const HISTORY_NOTE: &str = "Your phone sends all the messages it has. Photos and files \
                                aren't brought over. Keep your phone on and connected until \
                                this finishes.";

pub mod prelude {
    pub use crate::{
        AccountChoice, CAPTCHA_PAGE, ClientOutcome, Command, HISTORY_NOTE, HistoryGap, Profiles,
        SetupEvent, SetupState, Step, StepOutcome, captcha_navigation_allowed, captcha_token,
        claude::{ClaudeClient, ClientReport, ClientStatus, Clients, ServerEntry},
        pasted_captcha_token, run_steps, transition, uninstall_steps,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 45-day limit on a phone's history transfer covers photos and files,
    /// not messages (docs/link-and-sync.md), and setup imports no attachments,
    /// so the note must not promise a window of messages or any media.
    #[test]
    fn the_history_note_promises_no_45_day_window_or_media() {
        assert!(!HISTORY_NOTE.contains("45"), "{HISTORY_NOTE}");
        assert!(
            HISTORY_NOTE.contains("all the messages")
                && HISTORY_NOTE.contains("aren't brought over"),
            "{HISTORY_NOTE}"
        );
    }
}
