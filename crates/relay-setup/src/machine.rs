//! The setup state machine. A driver applies `transition`, performs the
//! returned commands, and feeds their results back in as events.

use relay_core::prelude::*;
use serde::{Deserialize, Serialize};

use crate::claude::ClaudeClient;

/// Whose account the agent uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountChoice {
    LinkExisting,
    OwnNumber,
}

/// What happened when registering with one Claude client.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientOutcome {
    pub client: ClaudeClient,
    pub result: Result<(), String>,
}

/// What the front end shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SetupState {
    /// `device_name` is what the phone will list this computer as once linked.
    ChooseAccount {
        device_name: String,
    },
    /// `uri` is the current QR payload; `None` while the first one is coming.
    Linking {
        device_name: String,
        uri: Option<String>,
    },
    EnterNumber,
    /// Waiting for Signal to send a verification code, or ask for a captcha.
    RequestingCode {
        number: String,
    },
    SolveCaptcha {
        number: String,
    },
    EnterCode {
        number: String,
    },
    /// Waiting for Signal to accept the verification code.
    VerifyingCode {
        number: String,
    },
    /// `kind` decides what follows: a linked device imports history, a new
    /// number has none.
    ChooseAccess {
        kind: ProfileKind,
    },
    ImportingHistory {
        imported: u64,
        total: Option<u64>,
        access: AccessMode,
    },
    ConnectingClients {
        access: AccessMode,
    },
    Done {
        clients: Vec<ClientOutcome>,
    },
    /// `retry` is the state a `Retry` event resumes from.
    Failed {
        message: String,
        retry: Box<SetupState>,
    },
}

/// What the user did, or what a command produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum SetupEvent {
    AccountChosen { choice: AccountChoice },
    LinkUri { uri: String },
    Linked,
    NumberEntered { number: String },
    CaptchaRequired,
    CaptchaSolved { token: String },
    CodeSent,
    CodeEntered { code: String },
    Registered,
    AccessChosen { access: AccessMode },
    HistoryProgress { imported: u64, total: Option<u64> },
    HistoryDone,
    ClientsConnected { clients: Vec<ClientOutcome> },
    RestartDesktop,
    CommandFailed { message: String },
    Retry,
}

/// Side effects the driver performs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    StartLink {
        device_name: String,
    },
    RequestCode {
        number: String,
        captcha: Option<String>,
    },
    VerifyCode {
        code: String,
    },
    SaveAccess {
        access: AccessMode,
    },
    ImportHistory,
    ConnectClients,
    RestartDesktop,
}

impl SetupState {
    /// Where setup begins on the computer called `computer_name`.
    pub fn start(computer_name: &str) -> Self {
        Self::ChooseAccount {
            device_name: format!("Claude ({computer_name})"),
        }
    }
}

/// The next state and the commands to run, given the current state and an event.
/// An event that makes no sense in the current state leaves it unchanged.
pub fn transition(state: &SetupState, event: SetupEvent) -> (SetupState, Vec<Command>) {
    use SetupEvent as E;
    use SetupState as S;

    match (state, event) {
        (
            S::ChooseAccount { device_name },
            E::AccountChosen {
                choice: AccountChoice::LinkExisting,
            },
        ) => start_link(device_name),
        (state, _) => (state.clone(), Vec::new()),
    }
}

fn start_link(device_name: &str) -> (SetupState, Vec<Command>) {
    (
        SetupState::Linking {
            device_name: device_name.to_owned(),
            uri: None,
        },
        vec![Command::StartLink {
            device_name: device_name.to_owned(),
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: &str = "Claude (Terrac's MacBook)";

    fn start() -> SetupState {
        SetupState::start("Terrac's MacBook")
    }

    /// Linking must tell the phone a readable device name, so the user can tell
    /// this computer apart in Signal's linked-devices list.
    #[test]
    fn linking_starts_with_the_readable_device_name() {
        let (state, commands) = transition(
            &start(),
            SetupEvent::AccountChosen {
                choice: AccountChoice::LinkExisting,
            },
        );
        assert_eq!(
            state,
            SetupState::Linking {
                device_name: DEVICE.into(),
                uri: None
            }
        );
        assert_eq!(
            commands,
            vec![Command::StartLink {
                device_name: DEVICE.into()
            }]
        );
    }
}
