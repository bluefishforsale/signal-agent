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
        (
            S::ChooseAccount { .. },
            E::AccountChosen {
                choice: AccountChoice::OwnNumber,
            },
        ) => (S::EnterNumber, Vec::new()),
        (S::EnterNumber, E::NumberEntered { number }) if !number.trim().is_empty() => {
            request_code(number.trim(), None)
        }
        (S::RequestingCode { number }, E::CaptchaRequired) => (
            S::SolveCaptcha {
                number: number.clone(),
            },
            Vec::new(),
        ),
        (S::SolveCaptcha { number }, E::CaptchaSolved { token }) => {
            request_code(number, Some(token))
        }
        (S::RequestingCode { number }, E::CodeSent) => (
            S::EnterCode {
                number: number.clone(),
            },
            Vec::new(),
        ),
        (S::EnterCode { number }, E::CodeEntered { code }) if !code.trim().is_empty() => (
            S::VerifyingCode {
                number: number.clone(),
            },
            vec![Command::VerifyCode {
                code: code.trim().to_owned(),
            }],
        ),
        (S::Linking { device_name, .. }, E::LinkUri { uri }) => (
            S::Linking {
                device_name: device_name.clone(),
                uri: Some(uri),
            },
            Vec::new(),
        ),
        (S::Linking { .. }, E::Linked) => (
            S::ChooseAccess {
                kind: ProfileKind::Linked,
            },
            Vec::new(),
        ),
        (
            S::ChooseAccess {
                kind: ProfileKind::Linked,
            },
            E::AccessChosen { access },
        ) => import_history(access),
        (S::VerifyingCode { .. }, E::Registered) => (
            S::ChooseAccess {
                kind: ProfileKind::Primary,
            },
            Vec::new(),
        ),
        (
            S::ChooseAccess {
                kind: ProfileKind::Primary,
            },
            E::AccessChosen { access },
        ) => connect_clients(access),
        (S::ImportingHistory { access, .. }, E::HistoryProgress { imported, total }) => (
            S::ImportingHistory {
                imported,
                total,
                access: *access,
            },
            Vec::new(),
        ),
        (S::ImportingHistory { access, .. }, E::HistoryDone) => connect_clients(*access),
        (S::ConnectingClients { .. }, E::ClientsConnected { clients }) => {
            (S::Done { clients }, Vec::new())
        }
        (S::Done { .. }, E::RestartDesktop) => (state.clone(), vec![Command::RestartDesktop]),
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

fn request_code(number: &str, captcha: Option<String>) -> (SetupState, Vec<Command>) {
    (
        SetupState::RequestingCode {
            number: number.to_owned(),
        },
        vec![Command::RequestCode {
            number: number.to_owned(),
            captcha,
        }],
    )
}

fn import_history(access: AccessMode) -> (SetupState, Vec<Command>) {
    (
        SetupState::ImportingHistory {
            imported: 0,
            total: None,
            access,
        },
        vec![Command::ImportHistory],
    )
}

// NOTE: SaveAccess must come first. The driver runs commands in order, and a
// client connecting before the mode is saved would see a read-only profile's
// write tools.
fn connect_clients(access: AccessMode) -> (SetupState, Vec<Command>) {
    (
        SetupState::ConnectingClients { access },
        vec![Command::SaveAccess { access }, Command::ConnectClients],
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

    fn linking(uri: Option<&str>) -> SetupState {
        SetupState::Linking {
            device_name: DEVICE.into(),
            uri: uri.map(Into::into),
        }
    }

    /// The QR code expires, so each new URI must replace the shown one without
    /// starting another link session.
    #[test]
    fn a_refreshed_link_uri_replaces_the_qr_without_restarting_the_link() {
        for before in [None, Some("sgnl://old")] {
            let (state, commands) = transition(
                &linking(before),
                SetupEvent::LinkUri {
                    uri: "sgnl://new".into(),
                },
            );
            assert_eq!(state, linking(Some("sgnl://new")), "from {before:?}");
            assert!(commands.is_empty(), "restarted the link: {commands:?}");
        }
    }

    /// A linked device goes to the access choice next, remembering it is linked
    /// so history import follows.
    #[test]
    fn a_scanned_link_asks_for_access_as_a_linked_device() {
        let (state, commands) = transition(&linking(Some("sgnl://x")), SetupEvent::Linked);
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Linked
            }
        );
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// A linked device imports history after the access choice, carrying the
    /// choice forward so it is saved before any client connects.
    #[test]
    fn a_linked_device_imports_history_after_choosing_access() {
        for access in [AccessMode::Full, AccessMode::ReadOnly] {
            let (state, commands) = transition(
                &SetupState::ChooseAccess {
                    kind: ProfileKind::Linked,
                },
                SetupEvent::AccessChosen { access },
            );
            assert_eq!(
                state,
                SetupState::ImportingHistory {
                    imported: 0,
                    total: None,
                    access
                }
            );
            assert_eq!(commands, vec![Command::ImportHistory]);
        }
    }

    /// Progress updates the counts the UI shows and keeps the access choice.
    #[test]
    fn history_progress_updates_counts_and_keeps_the_access_choice() {
        let importing = SetupState::ImportingHistory {
            imported: 0,
            total: None,
            access: AccessMode::ReadOnly,
        };
        let (state, commands) = transition(
            &importing,
            SetupEvent::HistoryProgress {
                imported: 40,
                total: Some(100),
            },
        );
        assert_eq!(
            state,
            SetupState::ImportingHistory {
                imported: 40,
                total: Some(100),
                access: AccessMode::ReadOnly
            }
        );
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// The access mode must be saved before any client can connect, or a client
    /// could reach a read-only profile's write tools in the gap.
    #[test]
    fn finished_history_saves_access_before_connecting_clients() {
        let importing = SetupState::ImportingHistory {
            imported: 100,
            total: Some(100),
            access: AccessMode::ReadOnly,
        };
        let (state, commands) = transition(&importing, SetupEvent::HistoryDone);
        assert_eq!(
            state,
            SetupState::ConnectingClients {
                access: AccessMode::ReadOnly
            }
        );
        assert_eq!(
            commands,
            vec![
                Command::SaveAccess {
                    access: AccessMode::ReadOnly
                },
                Command::ConnectClients
            ]
        );
    }

    fn outcomes() -> Vec<ClientOutcome> {
        vec![
            ClientOutcome {
                client: ClaudeClient::Code,
                result: Ok(()),
            },
            ClientOutcome {
                client: ClaudeClient::Desktop,
                result: Err("config is not valid JSON".into()),
            },
        ]
    }

    /// Done shows each client's own outcome, so one client failing doesn't hide
    /// that the other worked.
    #[test]
    fn connected_clients_finish_setup_with_each_outcome() {
        let (state, commands) = transition(
            &SetupState::ConnectingClients {
                access: AccessMode::Full,
            },
            SetupEvent::ClientsConnected {
                clients: outcomes(),
            },
        );
        assert_eq!(
            state,
            SetupState::Done {
                clients: outcomes()
            }
        );
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// The Restart Claude Desktop button stays on the done screen and asks the
    /// driver to restart, since Desktop only reads its config at launch.
    #[test]
    fn restart_desktop_from_done_keeps_the_done_screen() {
        let done = SetupState::Done {
            clients: outcomes(),
        };
        let (state, commands) = transition(&done, SetupEvent::RestartDesktop);
        assert_eq!(state, done);
        assert_eq!(commands, vec![Command::RestartDesktop]);
    }

    /// Giving the agent its own number asks for the number before talking to Signal.
    #[test]
    fn choosing_own_number_asks_for_the_number() {
        let (state, commands) = transition(
            &start(),
            SetupEvent::AccountChosen {
                choice: AccountChoice::OwnNumber,
            },
        );
        assert_eq!(state, SetupState::EnterNumber);
        assert!(commands.is_empty(), "{commands:?}");
    }

    const NUMBER: &str = "+15555550100";

    fn requesting() -> SetupState {
        SetupState::RequestingCode {
            number: NUMBER.into(),
        }
    }

    /// A number asks Signal for a code without a captcha first, and waits in a
    /// state that can't send a second request (each one costs an SMS).
    #[test]
    fn an_entered_number_requests_a_code_once() {
        let (state, commands) = transition(
            &SetupState::EnterNumber,
            SetupEvent::NumberEntered {
                number: format!(" {NUMBER} "),
            },
        );
        assert_eq!(state, requesting());
        assert_eq!(
            commands,
            vec![Command::RequestCode {
                number: NUMBER.into(),
                captcha: None
            }]
        );
        let (again, commands) = transition(
            &state,
            SetupEvent::NumberEntered {
                number: NUMBER.into(),
            },
        );
        assert_eq!(again, state);
        assert!(commands.is_empty(), "requested a second code: {commands:?}");
    }

    /// A blank number is a stray submit, not a request: it must not reach Signal.
    #[test]
    fn a_blank_number_is_ignored() {
        let (state, commands) = transition(
            &SetupState::EnterNumber,
            SetupEvent::NumberEntered {
                number: "  ".into(),
            },
        );
        assert_eq!(state, SetupState::EnterNumber);
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// When Signal wants a captcha, the solved token goes back with the same
    /// number in a fresh code request.
    #[test]
    fn a_solved_captcha_re_requests_the_code_with_its_token() {
        let (state, commands) = transition(&requesting(), SetupEvent::CaptchaRequired);
        assert_eq!(
            state,
            SetupState::SolveCaptcha {
                number: NUMBER.into()
            }
        );
        assert!(commands.is_empty(), "{commands:?}");

        let (state, commands) = transition(
            &state,
            SetupEvent::CaptchaSolved {
                token: "signalcaptcha://abc".into(),
            },
        );
        assert_eq!(state, requesting());
        assert_eq!(
            commands,
            vec![Command::RequestCode {
                number: NUMBER.into(),
                captcha: Some("signalcaptcha://abc".into())
            }]
        );
    }

    /// A sent code asks for it, and an entered code is verified exactly once
    /// (repeat submits must not burn verification attempts).
    #[test]
    fn an_entered_code_is_verified_once() {
        let (state, _) = transition(&requesting(), SetupEvent::CodeSent);
        assert_eq!(
            state,
            SetupState::EnterCode {
                number: NUMBER.into()
            }
        );
        let (state, commands) = transition(
            &state,
            SetupEvent::CodeEntered {
                code: " 123456 ".into(),
            },
        );
        assert_eq!(
            state,
            SetupState::VerifyingCode {
                number: NUMBER.into()
            }
        );
        assert_eq!(
            commands,
            vec![Command::VerifyCode {
                code: "123456".into()
            }]
        );
        let (again, commands) = transition(
            &state,
            SetupEvent::CodeEntered {
                code: "123456".into(),
            },
        );
        assert_eq!(again, state);
        assert!(commands.is_empty(), "verified twice: {commands:?}");
    }

    /// A blank code would burn a verification attempt for nothing.
    #[test]
    fn a_blank_code_is_ignored() {
        let entering = SetupState::EnterCode {
            number: NUMBER.into(),
        };
        let (state, commands) =
            transition(&entering, SetupEvent::CodeEntered { code: "\t".into() });
        assert_eq!(state, entering);
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// A newly registered number has no history to import, so access leads
    /// straight to saving it and connecting clients.
    #[test]
    fn a_registered_number_skips_history_after_choosing_access() {
        let verifying = SetupState::VerifyingCode {
            number: NUMBER.into(),
        };
        let (state, _) = transition(&verifying, SetupEvent::Registered);
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: ProfileKind::Primary
            }
        );
        let (state, commands) = transition(
            &state,
            SetupEvent::AccessChosen {
                access: AccessMode::Full,
            },
        );
        assert_eq!(
            state,
            SetupState::ConnectingClients {
                access: AccessMode::Full
            }
        );
        assert_eq!(
            commands,
            vec![
                Command::SaveAccess {
                    access: AccessMode::Full
                },
                Command::ConnectClients
            ]
        );
    }
}
