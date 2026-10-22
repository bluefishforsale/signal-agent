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
    AccountChosen {
        choice: AccountChoice,
    },
    LinkUri {
        uri: String,
    },
    Linked,
    NumberEntered {
        number: String,
    },
    CaptchaRequired,
    CaptchaSolved {
        token: String,
    },
    CodeSent,
    /// From `EnterCode`: the SMS didn't come, so ask again by phone call.
    // Signal only offers a call after an SMS attempt, which is why this is an
    // event from EnterCode rather than a choice at number entry.
    CodeByVoice,
    CodeEntered {
        code: String,
    },
    Registered,
    AccessChosen {
        access: AccessMode,
    },
    HistoryProgress {
        imported: u64,
        total: Option<u64>,
    },
    HistoryDone,
    ClientsConnected {
        clients: Vec<ClientOutcome>,
    },
    RestartDesktop,
    CommandFailed {
        message: String,
    },
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
        voice: bool,
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
            request_code(number.trim(), None, false)
        }
        (S::RequestingCode { number }, E::CaptchaRequired) => (
            S::SolveCaptcha {
                number: number.clone(),
            },
            Vec::new(),
        ),
        (S::SolveCaptcha { number }, E::CaptchaSolved { token }) => {
            request_code(number, Some(token), false)
        }
        (S::RequestingCode { number }, E::CodeSent) => (
            S::EnterCode {
                number: number.clone(),
            },
            Vec::new(),
        ),
        (S::EnterCode { number }, E::CodeByVoice) => request_code(number, None, true),
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
        (state, E::CommandFailed { message }) => match resume_point(state) {
            Some(retry) => (
                S::Failed {
                    message,
                    retry: Box::new(retry),
                },
                Vec::new(),
            ),
            None => (state.clone(), Vec::new()),
        },
        (S::Failed { retry, .. }, E::Retry) => resume(retry),
        (state, _) => (state.clone(), Vec::new()),
    }
}

/// Where a failure in `state` resumes, or `None` when `state` runs no command
/// and a failure report can only be stale.
fn resume_point(state: &SetupState) -> Option<SetupState> {
    use SetupState as S;

    match state {
        S::Linking { device_name, .. } => Some(S::Linking {
            device_name: device_name.clone(),
            uri: None,
        }),
        S::RequestingCode { .. } => Some(S::EnterNumber),
        S::VerifyingCode { number } => Some(S::EnterCode {
            number: number.clone(),
        }),
        S::ImportingHistory { access, .. } => Some(S::ImportingHistory {
            imported: 0,
            total: None,
            access: *access,
        }),
        S::ConnectingClients { .. } | S::Done { .. } => Some(state.clone()),
        S::ChooseAccount { .. }
        | S::EnterNumber
        | S::SolveCaptcha { .. }
        | S::EnterCode { .. }
        | S::ChooseAccess { .. }
        | S::Failed { .. } => None,
    }
}

/// Re-enters `state`, re-issuing the command it waits on, if any.
fn resume(state: &SetupState) -> (SetupState, Vec<Command>) {
    use SetupState as S;

    match state {
        S::Linking { device_name, .. } => start_link(device_name),
        S::ImportingHistory { access, .. } => import_history(*access),
        S::ConnectingClients { access } => connect_clients(*access),
        state => (state.clone(), Vec::new()),
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

fn request_code(number: &str, captcha: Option<String>, voice: bool) -> (SetupState, Vec<Command>) {
    (
        SetupState::RequestingCode {
            number: number.to_owned(),
        },
        vec![Command::RequestCode {
            number: number.to_owned(),
            captcha,
            voice,
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
    use proptest::prelude::*;

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
                captcha: None,
                voice: false,
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
                captcha: Some("signalcaptcha://abc".into()),
                voice: false,
            }]
        );
    }

    /// A code that never arrived by SMS can be asked for again by phone call,
    /// once per press, and only from the code entry screen.
    #[test]
    fn asking_for_a_call_re_requests_the_code_by_voice() {
        let entering = SetupState::EnterCode {
            number: NUMBER.into(),
        };
        let (state, commands) = transition(&entering, SetupEvent::CodeByVoice);
        assert_eq!(state, requesting());
        assert_eq!(
            commands,
            vec![Command::RequestCode {
                number: NUMBER.into(),
                captcha: None,
                voice: true,
            }]
        );
        let (again, commands) = transition(&state, SetupEvent::CodeByVoice);
        assert_eq!(again, state, "a second press moved setup on");
        assert!(commands.is_empty(), "requested a second call: {commands:?}");
        let (state, commands) = transition(&SetupState::EnterNumber, SetupEvent::CodeByVoice);
        assert_eq!(state, SetupState::EnterNumber);
        assert!(commands.is_empty(), "called before any SMS: {commands:?}");
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

    /// A failed command shows its message, and Retry resumes where the user can
    /// act again: a fresh link, the number or code to correct, or the same work
    /// re-run with the access choice still saved first.
    #[test]
    fn a_failed_command_retries_from_where_the_user_can_act() {
        let full = AccessMode::Full;
        let cases = [
            (
                linking(Some("sgnl://x")),
                linking(None),
                vec![Command::StartLink {
                    device_name: DEVICE.into(),
                }],
            ),
            (requesting(), SetupState::EnterNumber, vec![]),
            (
                SetupState::VerifyingCode {
                    number: NUMBER.into(),
                },
                SetupState::EnterCode {
                    number: NUMBER.into(),
                },
                vec![],
            ),
            (
                SetupState::ImportingHistory {
                    imported: 40,
                    total: Some(100),
                    access: full,
                },
                SetupState::ImportingHistory {
                    imported: 0,
                    total: None,
                    access: full,
                },
                vec![Command::ImportHistory],
            ),
            (
                SetupState::ConnectingClients { access: full },
                SetupState::ConnectingClients { access: full },
                vec![
                    Command::SaveAccess { access: full },
                    Command::ConnectClients,
                ],
            ),
            (
                SetupState::Done {
                    clients: outcomes(),
                },
                SetupState::Done {
                    clients: outcomes(),
                },
                vec![],
            ),
        ];
        for (waiting, resumed, rerun) in cases {
            let (failed, commands) = transition(
                &waiting,
                SetupEvent::CommandFailed {
                    message: "network down".into(),
                },
            );
            let SetupState::Failed { message, .. } = &failed else {
                panic!("{waiting:?} did not fail: {failed:?}");
            };
            assert_eq!(message, "network down");
            assert!(commands.is_empty(), "{commands:?}");

            let (state, commands) = transition(&failed, SetupEvent::Retry);
            assert_eq!(state, resumed, "retry from {waiting:?}");
            assert_eq!(commands, rerun, "retry from {waiting:?}");
        }
    }

    fn every_event() -> Vec<SetupEvent> {
        use SetupEvent as E;
        vec![
            E::AccountChosen {
                choice: AccountChoice::LinkExisting,
            },
            E::AccountChosen {
                choice: AccountChoice::OwnNumber,
            },
            E::LinkUri {
                uri: "sgnl://y".into(),
            },
            E::Linked,
            E::NumberEntered {
                number: NUMBER.into(),
            },
            E::CaptchaRequired,
            E::CaptchaSolved { token: "t".into() },
            E::CodeSent,
            E::CodeByVoice,
            E::CodeEntered { code: "1".into() },
            E::Registered,
            E::AccessChosen {
                access: AccessMode::Full,
            },
            E::HistoryProgress {
                imported: 1,
                total: None,
            },
            E::HistoryDone,
            E::ClientsConnected { clients: vec![] },
            E::RestartDesktop,
            E::CommandFailed {
                message: "boom".into(),
            },
            E::Retry,
        ]
    }

    /// Events that don't belong to a state (a late reply, a double click, a
    /// confused driver) must leave it untouched and run nothing, in every state.
    #[test]
    fn events_that_do_not_belong_to_a_state_leave_it_untouched() {
        use SetupEvent as E;
        use std::mem::discriminant;

        let failed = SetupState::Failed {
            message: "boom".into(),
            retry: Box::new(SetupState::EnterNumber),
        };
        let failure = E::CommandFailed {
            message: String::new(),
        };
        let accepts: Vec<(SetupState, Vec<E>)> = vec![
            (
                start(),
                vec![E::AccountChosen {
                    choice: AccountChoice::OwnNumber,
                }],
            ),
            (
                linking(None),
                vec![
                    E::LinkUri { uri: String::new() },
                    E::Linked,
                    failure.clone(),
                ],
            ),
            (
                SetupState::EnterNumber,
                vec![E::NumberEntered {
                    number: String::new(),
                }],
            ),
            (
                requesting(),
                vec![E::CaptchaRequired, E::CodeSent, failure.clone()],
            ),
            (
                SetupState::SolveCaptcha {
                    number: NUMBER.into(),
                },
                vec![E::CaptchaSolved {
                    token: String::new(),
                }],
            ),
            (
                SetupState::EnterCode {
                    number: NUMBER.into(),
                },
                vec![
                    E::CodeEntered {
                        code: String::new(),
                    },
                    E::CodeByVoice,
                ],
            ),
            (
                SetupState::VerifyingCode {
                    number: NUMBER.into(),
                },
                vec![E::Registered, failure.clone()],
            ),
            (
                SetupState::ChooseAccess {
                    kind: ProfileKind::Linked,
                },
                vec![E::AccessChosen {
                    access: AccessMode::Full,
                }],
            ),
            (
                SetupState::ImportingHistory {
                    imported: 0,
                    total: None,
                    access: AccessMode::Full,
                },
                vec![
                    E::HistoryProgress {
                        imported: 0,
                        total: None,
                    },
                    E::HistoryDone,
                    failure.clone(),
                ],
            ),
            (
                SetupState::ConnectingClients {
                    access: AccessMode::Full,
                },
                vec![E::ClientsConnected { clients: vec![] }, failure.clone()],
            ),
            (
                SetupState::Done { clients: vec![] },
                vec![E::RestartDesktop, failure.clone()],
            ),
            (failed, vec![E::Retry]),
        ];
        for (state, accepted) in accepts {
            for event in every_event() {
                if accepted
                    .iter()
                    .any(|a| discriminant(a) == discriminant(&event))
                {
                    continue;
                }
                let (next, commands) = transition(&state, event.clone());
                assert_eq!(next, state, "{event:?} changed {state:?}");
                assert!(
                    commands.is_empty(),
                    "{event:?} in {state:?} ran {commands:?}"
                );
            }
        }
    }

    fn any_event() -> impl Strategy<Value = SetupEvent> {
        use SetupEvent as E;
        let text = prop_oneof![Just(String::new()), Just(" ".to_owned()), ".{0,12}"];
        let access = prop_oneof![Just(AccessMode::Full), Just(AccessMode::ReadOnly)];
        prop_oneof![
            prop_oneof![
                Just(AccountChoice::LinkExisting),
                Just(AccountChoice::OwnNumber)
            ]
            .prop_map(|choice| E::AccountChosen { choice }),
            text.clone().prop_map(|uri| E::LinkUri { uri }),
            Just(E::Linked),
            text.clone().prop_map(|number| E::NumberEntered { number }),
            Just(E::CaptchaRequired),
            text.clone().prop_map(|token| E::CaptchaSolved { token }),
            Just(E::CodeSent),
            Just(E::CodeByVoice),
            text.clone().prop_map(|code| E::CodeEntered { code }),
            Just(E::Registered),
            access.prop_map(|access| E::AccessChosen { access }),
            (any::<u64>(), any::<Option<u64>>())
                .prop_map(|(imported, total)| E::HistoryProgress { imported, total }),
            Just(E::HistoryDone),
            Just(E::ClientsConnected { clients: vec![] }),
            Just(E::RestartDesktop),
            text.prop_map(|message| E::CommandFailed { message }),
            Just(E::Retry),
        ]
    }

    proptest! {
        /// No sequence of events, however nonsensical, may panic the machine or
        /// connect a client before the user chose an access mode and it was saved.
        #[test]
        fn clients_never_connect_before_access_is_chosen_and_saved(
            events in prop::collection::vec(any_event(), 0..80)
        ) {
            let mut state = start();
            let mut chosen = None;
            for event in events {
                if let (SetupState::ChooseAccess { .. }, SetupEvent::AccessChosen { access }) =
                    (&state, &event)
                {
                    chosen = Some(*access);
                }
                let (next, commands) = transition(&state, event.clone());
                if let Some(i) = commands.iter().position(|c| *c == Command::ConnectClients) {
                    let Some(access) = chosen else {
                        panic!("{event:?} in {state:?} connected clients before an access choice");
                    };
                    prop_assert!(
                        i > 0 && commands.get(i - 1) == Some(&Command::SaveAccess { access }),
                        "{event:?} in {state:?} connected without saving {access:?} first: {commands:?}"
                    );
                }
                if matches!(next, SetupState::ConnectingClients { .. } | SetupState::Done { .. }) {
                    prop_assert!(chosen.is_some(), "reached {next:?} without an access choice");
                }
                state = next;
            }
        }
    }
}
