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
    ChooseAccount,
    /// `uri` is the current QR payload; `None` while the first one is coming.
    Linking {
        uri: Option<String>,
    },
    EnterNumber,
    SolveCaptcha {
        number: String,
    },
    EnterCode {
        number: String,
    },
    ChooseAccess,
    ImportingHistory {
        imported: u64,
        total: Option<u64>,
    },
    ConnectingClients,
    Done {
        clients: Vec<ClientOutcome>,
    },
    Failed {
        message: String,
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
    CommandFailed { message: String },
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
}

/// The next state and the commands to run, given the current state and an event.
pub fn transition(state: &SetupState, event: SetupEvent) -> (SetupState, Vec<Command>) {
    // Lane L5 implements the transitions; until then every event fails visibly.
    let _ = state;
    (
        SetupState::Failed {
            message: format!("setup is not implemented yet (lane L5); got {event:?}"),
        },
        Vec::new(),
    )
}
