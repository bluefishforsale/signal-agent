//! What the window draws for each setup state. Anything the page can't do
//! without a library (the QR code) is done here, so the page stays plain.

use qrcode::{QrCode, render::svg};
use relay_core::prelude::*;
use relay_setup::prelude::*;
use serde::Serialize;

/// One screen of the setup window and the data it shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "screen", rename_all = "snake_case")]
pub enum View {
    ChooseAccount {
        device_name: String,
    },
    /// `qr` is an SVG of the current link URI.
    Linking {
        device_name: String,
        qr: Option<String>,
    },
    EnterNumber,
    RequestingCode {
        number: String,
    },
    SolveCaptcha {
        number: String,
    },
    EnterCode {
        number: String,
    },
    VerifyingCode {
        number: String,
    },
    ChooseAccess {
        imports_history: bool,
    },
    /// `percent` is known only once Signal says how much is coming.
    ImportingHistory {
        imported: u64,
        percent: Option<u8>,
        note: &'static str,
    },
    HistoryUnavailable {
        reason: HistoryGap,
    },
    FindingDesktop,
    OfferDesktopImport,
    ImportingDesktop {
        imported: u64,
    },
    ConnectingClients,
    Done {
        clients: Vec<ClientLine>,
        can_restart_desktop: bool,
    },
    /// `can_skip_history` when what failed was the history import, which
    /// the user may give up on.
    Failed {
        message: String,
        can_skip_history: bool,
    },
}

/// How registering with one Claude app went.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientLine {
    pub name: String,
    pub problem: Option<String>,
}

/// The screen for `state`.
pub fn view(state: &SetupState) -> View {
    use SetupState as S;

    match state {
        S::ChooseAccount { device_name } => View::ChooseAccount {
            device_name: device_name.clone(),
        },
        S::Linking { device_name, uri } => View::Linking {
            device_name: device_name.clone(),
            qr: uri.as_deref().and_then(qr_svg),
        },
        S::EnterNumber => View::EnterNumber,
        S::RequestingCode { number } => View::RequestingCode {
            number: number.clone(),
        },
        S::SolveCaptcha { number } => View::SolveCaptcha {
            number: number.clone(),
        },
        S::EnterCode { number } => View::EnterCode {
            number: number.clone(),
        },
        S::VerifyingCode { number } => View::VerifyingCode {
            number: number.clone(),
        },
        S::ChooseAccess { kind } => View::ChooseAccess {
            imports_history: *kind == ProfileKind::Linked,
        },
        S::ImportingHistory {
            imported, total, ..
        } => View::ImportingHistory {
            imported: *imported,
            percent: total.map(|total| percent(*imported, total)),
            note: HISTORY_NOTE,
        },
        S::HistoryUnavailable { reason, .. } => View::HistoryUnavailable { reason: *reason },
        S::FindingDesktop { .. } => View::FindingDesktop,
        S::OfferDesktopImport { .. } => View::OfferDesktopImport,
        S::ImportingDesktop { imported, .. } => View::ImportingDesktop {
            imported: *imported,
        },
        S::ConnectingClients { .. } => View::ConnectingClients,
        S::Done { clients } => View::Done {
            clients: clients
                .iter()
                .map(|c| ClientLine {
                    name: c.client.to_string(),
                    problem: c.result.clone().err(),
                })
                .collect(),
            can_restart_desktop: clients
                .iter()
                .any(|c| c.client == ClaudeClient::Desktop && c.result.is_ok()),
        },
        S::Failed { message, retry } => View::Failed {
            message: message.clone(),
            can_skip_history: matches!(**retry, S::ImportingHistory { .. }),
        },
    }
}

fn percent(done: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    let ratio = u128::from(done.min(total)) * 100 / u128::from(total);
    u8::try_from(ratio).unwrap_or(100)
}

// A URI too long for any QR version is the only failure; showing the waiting
// state beats a crash, and the next refresh replaces it.
fn qr_svg(uri: &str) -> Option<String> {
    let code = QrCode::new(uri).ok()?;
    Some(
        code.render::<svg::Color>()
            .quiet_zone(true)
            .min_dimensions(240, 240)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linking(uri: Option<&str>) -> SetupState {
        SetupState::Linking {
            device_name: "Claude (Mac)".into(),
            uri: uri.map(Into::into),
        }
    }

    /// The phone scans a QR drawn from the link URI, so the linking screen must
    /// carry one as SVG, and none until the first URI arrives.
    #[test]
    fn linking_draws_a_qr_svg_only_once_a_uri_arrives() {
        let View::Linking { device_name, qr } = view(&linking(Some("sgnl://linkdevice?uuid=a")))
        else {
            panic!("not the linking screen");
        };
        assert_eq!(device_name, "Claude (Mac)");
        let qr = qr.expect("no QR for a known URI");
        assert!(
            qr.starts_with("<?xml") || qr.starts_with("<svg"),
            "not SVG: {qr:.40}"
        );

        let View::Linking { qr, .. } = view(&linking(None)) else {
            panic!("not the linking screen");
        };
        assert_eq!(qr, None);
    }

    fn every_state() -> Vec<SetupState> {
        let number = || "+15555550123".to_owned();
        vec![
            SetupState::start("Mac"),
            linking(None),
            linking(Some("sgnl://x")),
            SetupState::EnterNumber,
            SetupState::RequestingCode { number: number() },
            SetupState::SolveCaptcha { number: number() },
            SetupState::EnterCode { number: number() },
            SetupState::VerifyingCode { number: number() },
            SetupState::ChooseAccess {
                kind: ProfileKind::Linked,
            },
            SetupState::ChooseAccess {
                kind: ProfileKind::Primary,
            },
            SetupState::ImportingHistory {
                imported: 3,
                total: Some(10),
                access: AccessMode::Full,
            },
            SetupState::HistoryUnavailable {
                reason: HistoryGap::TimedOut,
                access: AccessMode::Full,
            },
            SetupState::FindingDesktop {
                access: AccessMode::Full,
            },
            SetupState::OfferDesktopImport {
                access: AccessMode::Full,
            },
            SetupState::ImportingDesktop {
                imported: 4,
                access: AccessMode::Full,
            },
            SetupState::ConnectingClients {
                access: AccessMode::Full,
            },
            SetupState::Done {
                clients: Vec::new(),
            },
            SetupState::Failed {
                message: "no".into(),
                retry: Box::new(SetupState::EnterNumber),
            },
        ]
    }

    const PAGE: &str = include_str!("../../ui/index.html");

    /// The page picks its screen by the state's name, so every state needs a
    /// screen of the same name, and a section for it, or the window goes blank.
    #[test]
    fn every_state_has_a_screen_of_the_same_name() {
        for state in every_state() {
            let tag = |v: serde_json::Value, key: &str| v[key].as_str().map(str::to_owned);
            let state_name = tag(serde_json::to_value(&state).unwrap(), "state");
            let screen_name = tag(serde_json::to_value(view(&state)).unwrap(), "screen");
            assert!(state_name.is_some(), "untagged state {state:?}");
            assert_eq!(screen_name, state_name, "for {state:?}");
            let section = format!(r#"data-screen="{}""#, screen_name.unwrap_or_default());
            assert!(PAGE.contains(&section), "index.html has no {section}");
        }
    }

    fn history_percent(imported: u64, total: Option<u64>) -> Option<u8> {
        let View::ImportingHistory { percent, .. } = view(&SetupState::ImportingHistory {
            imported,
            total,
            access: AccessMode::Full,
        }) else {
            panic!("not the history screen");
        };
        percent
    }

    /// The progress bar must stay within 0 to 100 even when Signal's count
    /// overshoots its own total or the total is zero, and stay indeterminate
    /// until a total is known.
    #[test]
    fn history_percent_stays_in_range_and_waits_for_a_total() {
        assert_eq!(history_percent(5, None), None);
        assert_eq!(history_percent(0, Some(0)), Some(0));
        assert_eq!(history_percent(1, Some(4)), Some(25));
        assert_eq!(history_percent(12, Some(10)), Some(100));
        assert_eq!(history_percent(u64::MAX, Some(u64::MAX)), Some(100));
    }

    /// The window once claimed Signal shares about 45 days of messages, which
    /// is its limit for media only; it must show relay-setup's note instead of
    /// copy of its own.
    #[test]
    fn importing_history_shows_the_shared_history_note() {
        let View::ImportingHistory { note, .. } = view(&SetupState::ImportingHistory {
            imported: 0,
            total: None,
            access: AccessMode::Full,
        }) else {
            panic!("not the history screen");
        };
        assert_eq!(note, HISTORY_NOTE);
        assert!(PAGE.contains(r#"data-field="note""#), "no note field");
        assert!(!PAGE.contains("45 days"), "index.html has its own copy");
    }

    /// The page shows the reason whose name the view sends; a reason with no
    /// matching paragraph would leave the screen without its explanation.
    #[test]
    fn every_missing_history_reason_has_its_explanation() {
        for reason in [
            HistoryGap::NotSent,
            HistoryGap::RelinkRequested,
            HistoryGap::TimedOut,
        ] {
            let json = serde_json::to_value(view(&SetupState::HistoryUnavailable {
                reason,
                access: AccessMode::Full,
            }))
            .unwrap();
            let name = json["reason"].as_str().expect("reason name");
            let paragraph = format!(r#"data-reason="{name}""#);
            assert!(PAGE.contains(&paragraph), "index.html has no {paragraph}");
        }
    }

    /// Only a failed history import may offer skipping; anywhere else the
    /// button would send an event setup ignores.
    #[test]
    fn only_a_failed_history_import_offers_skipping() {
        let failed = |retry| {
            let View::Failed {
                can_skip_history, ..
            } = view(&SetupState::Failed {
                message: "no".into(),
                retry: Box::new(retry),
            })
            else {
                panic!("not the failure screen");
            };
            can_skip_history
        };
        assert!(failed(SetupState::ImportingHistory {
            imported: 0,
            total: None,
            access: AccessMode::Full,
        }));
        assert!(!failed(SetupState::EnterNumber));
        assert!(!failed(SetupState::OfferDesktopImport {
            access: AccessMode::Full,
        }));
    }

    fn done(clients: &[(ClaudeClient, Result<(), &str>)]) -> View {
        view(&SetupState::Done {
            clients: clients
                .iter()
                .map(|(client, result)| ClientOutcome {
                    client: *client,
                    result: result.map_err(str::to_owned),
                })
                .collect(),
        })
    }

    /// Restarting Claude Desktop only helps when setup added the relay to it;
    /// offering it otherwise would close the user's app for nothing.
    #[test]
    fn restart_is_offered_only_when_claude_desktop_was_connected() {
        let cases = [
            (vec![(ClaudeClient::Desktop, Ok(()))], true),
            (vec![(ClaudeClient::Desktop, Err("not installed"))], false),
            (vec![(ClaudeClient::Code, Ok(()))], false),
            (vec![], false),
        ];
        for (clients, expected) in cases {
            let View::Done {
                can_restart_desktop,
                ..
            } = done(&clients)
            else {
                panic!("not the done screen");
            };
            assert_eq!(can_restart_desktop, expected, "for {clients:?}");
        }
    }

    /// Each Claude app gets its own line, named for the user, with the reason
    /// it failed so they know what to fix.
    #[test]
    fn done_lists_each_client_by_name_with_its_problem() {
        let View::Done { clients, .. } = done(&[
            (ClaudeClient::Desktop, Ok(())),
            (ClaudeClient::Code, Err("Claude Code isn't installed")),
        ]) else {
            panic!("not the done screen");
        };
        assert_eq!(
            clients,
            vec![
                ClientLine {
                    name: "Claude Desktop".into(),
                    problem: None
                },
                ClientLine {
                    name: "Claude Code".into(),
                    problem: Some("Claude Code isn't installed".into())
                },
            ]
        );
    }
}
