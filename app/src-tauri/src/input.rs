//! Checks and cleans what the page sends before setup sees it: people paste
//! links with stray spaces and the browser's scheme attached, and the page
//! must never be able to claim a result only the driver can report.

use relay_setup::prelude::*;

/// `event` as setup should see it, or `None` when the page may not send it.
pub fn from_page(event: SetupEvent) -> Option<SetupEvent> {
    use SetupEvent as E;

    match event {
        E::CaptchaSolved { token } => Some(E::CaptchaSolved {
            token: captcha_token(&token)?,
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

/// The captcha token inside a pasted `signalcaptcha://` link, or `None` when
/// nothing usable was pasted.
pub fn captcha_token(pasted: &str) -> Option<String> {
    let pasted = pasted.trim();
    let token = pasted
        .strip_prefix("signalcaptcha://")
        .unwrap_or(pasted)
        .trim();
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Signal wants the bare token, but the browser hands over the whole
    /// `signalcaptcha://` link, often with whitespace from the copy.
    #[test]
    fn a_pasted_captcha_link_yields_the_bare_token() {
        let token = "signal-hcaptcha.5fad97ac.registration.P1_eyJ0";
        for pasted in [
            format!("signalcaptcha://{token}"),
            format!("  signalcaptcha://{token}\n"),
            token.to_owned(),
        ] {
            assert_eq!(
                captcha_token(&pasted).as_deref(),
                Some(token),
                "from {pasted:?}"
            );
        }
    }

    /// An empty paste must not be sent as a solved captcha.
    #[test]
    fn an_empty_paste_is_no_captcha() {
        for pasted in ["", "   ", "signalcaptcha://", " signalcaptcha:// "] {
            assert_eq!(captcha_token(pasted), None, "from {pasted:?}");
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
                r#"{"event":"captcha_solved","token":"t"}"#,
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
