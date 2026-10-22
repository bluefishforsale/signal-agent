//! Checks and cleans what the page sends before setup sees it: people paste
//! captcha links and type phone numbers loosely, and the page must never be
//! able to claim a result only the driver can report.

use relay_setup::prelude::*;

/// `event` as setup should see it, or `None` when the page may not send it.
pub fn from_page(event: SetupEvent) -> Option<SetupEvent> {
    use SetupEvent as E;

    match event {
        E::CaptchaSolved { token } => Some(E::CaptchaSolved {
            token: pasted_captcha_token(&token)?,
        }),
        E::NumberEntered { number } => Some(E::NumberEntered {
            number: number
                .chars()
                .filter(|c| !c.is_whitespace() && !"()-.".contains(*c))
                .collect(),
        }),
        E::AccountChosen { .. }
        | E::CodeEntered { .. }
        | E::AccessChosen { .. }
        | E::RestartDesktop
        | E::Retry => Some(event),
        E::LinkUri { .. }
        | E::Linked
        | E::CaptchaRequired
        | E::CodeSent
        | E::Registered
        | E::HistoryProgress { .. }
        | E::HistoryDone
        | E::ClientsConnected { .. }
        | E::CommandFailed { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pasted(text: &str) -> Option<String> {
        match from_page(SetupEvent::CaptchaSolved { token: text.into() })? {
            SetupEvent::CaptchaSolved { token } => Some(token),
            other => panic!("captcha paste became {other:?}"),
        }
    }

    /// Signal wants the bare token, but people paste either the whole
    /// `signalcaptcha://` link or the token copied out of it, often with
    /// whitespace from the copy.
    #[test]
    fn a_pasted_captcha_link_or_bare_token_yields_the_bare_token() {
        let token = "signal-hcaptcha.5fad97ac.registration.P1_eyJ0";
        for text in [
            format!("signalcaptcha://{token}"),
            format!("  signalcaptcha://{token}\n"),
            token.to_owned(),
            format!(" {token} "),
        ] {
            assert_eq!(pasted(&text).as_deref(), Some(token), "from {text:?}");
        }
    }

    /// Every malformed token sent to Signal spends a registration attempt, so
    /// an empty, line-wrapped or quoted paste must stop at the window.
    #[test]
    fn a_malformed_captcha_paste_never_reaches_signal() {
        for text in [
            "",
            "   ",
            "signalcaptcha://",
            "signal-hcaptcha.5fad97ac\n.registration",
            "\"signal-hcaptcha.5fad97ac\"",
            "hello",
        ] {
            assert_eq!(pasted(text), None, "accepted {text:?}");
        }
    }

    /// Results such as "linked" or "registered" come only from the driver; a
    /// page that could send them could skip linking altogether.
    #[test]
    fn the_page_cannot_report_driver_results() {
        for event in [
            SetupEvent::LinkUri {
                uri: "sgnl://x".into(),
            },
            SetupEvent::Linked,
            SetupEvent::CaptchaRequired,
            SetupEvent::CodeSent,
            SetupEvent::Registered,
            SetupEvent::HistoryProgress {
                imported: 1,
                total: None,
            },
            SetupEvent::HistoryDone,
            SetupEvent::ClientsConnected {
                clients: Vec::new(),
            },
            SetupEvent::CommandFailed {
                message: "x".into(),
            },
        ] {
            assert_eq!(from_page(event.clone()), None, "accepted {event:?}");
        }
    }

    /// People type numbers the way they read them; Signal needs the bare
    /// international form, so spacing and punctuation must not reach it.
    #[test]
    fn a_typed_number_reaches_setup_in_international_form() {
        for typed in ["+1 (555) 555-0123", " +1.555.555.0123 ", "+15555550123"] {
            assert_eq!(
                from_page(SetupEvent::NumberEntered {
                    number: typed.into()
                }),
                Some(SetupEvent::NumberEntered {
                    number: "+15555550123".into()
                }),
                "from {typed:?}"
            );
        }
    }

    /// app.js builds these payloads by hand; if one stops matching setup's
    /// event shape, its button silently does nothing.
    #[test]
    fn every_payload_the_page_sends_is_a_setup_event_it_may_send() {
        let page = include_str!("../../ui/app.js");
        let sent = [
            (
                r#"event: "account_chosen", choice: "#,
                r#"{"event":"account_chosen","choice":"link_existing"}"#,
            ),
            (
                r#"event: "account_chosen", choice: "#,
                r#"{"event":"account_chosen","choice":"own_number"}"#,
            ),
            (
                r#"event: "access_chosen", access: "#,
                r#"{"event":"access_chosen","access":"full"}"#,
            ),
            (
                r#"event: "access_chosen", access: "#,
                r#"{"event":"access_chosen","access":"read_only"}"#,
            ),
            (
                r#"event: "restart_desktop""#,
                r#"{"event":"restart_desktop"}"#,
            ),
            (r#"event: "retry""#, r#"{"event":"retry"}"#),
            (
                r#"event: "number_entered", number: "#,
                r#"{"event":"number_entered","number":"+15555550123"}"#,
            ),
            (
                r#"event: "captcha_solved", token: "#,
                r#"{"event":"captcha_solved","token":"signal-hcaptcha.x"}"#,
            ),
            (
                r#"event: "code_entered", code: "#,
                r#"{"event":"code_entered","code":"123456"}"#,
            ),
        ];
        for (in_page, json) in sent {
            assert!(page.contains(in_page), "app.js no longer sends {in_page}");
            let event: SetupEvent = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("{json} is not a setup event: {e}"));
            assert!(
                from_page(event).is_some(),
                "{json} is refused from the page"
            );
        }
        let html = include_str!("../../ui/index.html");
        for value in ["link_existing", "own_number", "full", "read_only"] {
            assert!(
                html.contains(&format!(r#"value="{value}""#)),
                "no {value} choice in index.html"
            );
        }
    }
}
