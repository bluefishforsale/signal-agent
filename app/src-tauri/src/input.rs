//! Cleans what the user pastes before setup sees it, since people copy links
//! with stray spaces and the browser's scheme attached.

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
}
