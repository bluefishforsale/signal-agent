//! Registering the agent's own number: catching the captcha token Signal's
//! page hands back, so nobody copies a link by hand.

/// The `signalcaptcha://` token in `url`, if `url` is the link Signal's
/// captcha page navigates to when solved.
pub fn captcha_token(url: &str) -> Option<String> {
    let token = url.trim().strip_prefix("signalcaptcha://")?;
    let token = token.trim_end_matches('/');
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HCAPTCHA: &str = "signal-hcaptcha.5fad97ac-7d06-4e44-b18a-b950b20148ff.registration.P1_eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJwZCI6MCwiZXhwIjoxNzYxMDAwMDAwfQ.k2Zq-Xw_3bXr9aQ";
    const RECAPTCHA: &str = "signal-recaptcha-v2.6LfBXs0bAAAAAAjkDyyI1Lk5gBAUWfhI_bIyox5W.registration.03AFcWeA5ZqgXb1vKx-Ym_8Lr2Qp0";

    /// The page's result link is `signalcaptcha://<token>`, and Signal wants
    /// the token alone; a pasted or reported link may carry a trailing slash
    /// or surrounding whitespace that must not end up in the token.
    #[test]
    fn a_solved_captcha_link_yields_its_token() {
        for token in [HCAPTCHA, RECAPTCHA] {
            for link in [
                format!("signalcaptcha://{token}"),
                format!("signalcaptcha://{token}/"),
                format!("  signalcaptcha://{token}\n"),
            ] {
                assert_eq!(
                    captcha_token(&link).as_deref(),
                    Some(token),
                    "link {link:?}"
                );
            }
        }
    }

    /// Every other navigation the captcha page makes (its own pages, the
    /// captcha provider's frames) must pass through untouched, and an empty
    /// result must never be sent to Signal as a token.
    #[test]
    fn other_navigations_yield_no_token() {
        for url in [
            "https://signalcaptchas.org/registration/generate.html",
            "https://newassets.hcaptcha.com/captcha/v1/abc/static/hcaptcha.html#frame=checkbox",
            "about:blank",
            "signalcaptcha://",
            "signalcaptcha:///",
            "sgnl://linkdevice?uuid=abc",
            "https://evil.example/?next=signalcaptcha://signal-hcaptcha.x",
        ] {
            assert_eq!(captcha_token(url), None, "token from {url:?}");
        }
    }

    /// The webview reports navigations as parsed URLs, and a parser that
    /// lowercased or re-encoded the opaque part would hand Signal a token it
    /// rejects; mixed case and `_`/`-` must come through untouched.
    #[test]
    fn a_token_survives_the_webviews_url_parsing() {
        for token in [HCAPTCHA, RECAPTCHA] {
            let parsed = url::Url::parse(&format!("signalcaptcha://{token}")).expect("parse");
            assert_eq!(captcha_token(parsed.as_str()).as_deref(), Some(token));
        }
    }
}
