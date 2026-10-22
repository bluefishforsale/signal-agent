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
}
