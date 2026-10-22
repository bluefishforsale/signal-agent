//! The captcha step of registering the agent's own number: catching the token
//! Signal's page hands back, so nobody copies a link by hand.

use url::Url;

/// Signal's captcha page for registering a number on the production servers.
pub const CAPTCHA_PAGE: &str = "https://signalcaptchas.org/registration/generate.html";

// Host, whether its subdomains count too, and the path prefix it is limited to.
// NOTE: Google and gstatic serve far more than reCAPTCHA, so they are held to
// its path; widening either turns the webview into a general browser.
const CAPTCHA_HOSTS: &[(&str, bool, &str)] = &[
    ("signalcaptchas.org", false, "/"),
    ("hcaptcha.com", true, "/"),
    ("www.google.com", false, "/recaptcha/"),
    ("www.recaptcha.net", false, "/recaptcha/"),
    ("www.gstatic.com", false, "/recaptcha/"),
];

/// Whether the captcha webview may load `url`.
pub fn captcha_navigation_allowed(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    if url.as_str() == "about:blank" {
        return true;
    }
    let Some(host) = url.host_str().filter(|_| url.scheme() == "https") else {
        return false;
    };
    CAPTCHA_HOSTS.iter().any(|&(allowed, subdomains, path)| {
        let host_ok = host == allowed
            || (subdomains
                && host
                    .strip_suffix(allowed)
                    .is_some_and(|sub| sub.ends_with('.')));
        host_ok && url.path().starts_with(path)
    })
}

/// The `signalcaptcha://` token in `url`, if `url` is the link Signal's
/// captcha page navigates to when solved.
pub fn captcha_token(url: &str) -> Option<String> {
    let token = url.trim().strip_prefix("signalcaptcha://")?;
    valid_token(token.trim_end_matches('/'))
}

/// The token in what a user pasted: the whole `signalcaptcha://` link, or the
/// bare token copied out of it.
pub fn pasted_captcha_token(text: &str) -> Option<String> {
    let text = text.trim();
    if text.starts_with("signalcaptcha://") {
        return captcha_token(text);
    }
    // Every Signal captcha token names its provider first, e.g. signal-hcaptcha.
    text.starts_with("signal-").then(|| valid_token(text))?
}

// NOTE: every malformed token sent to Signal spends a registration attempt
// against its anti-abuse limits, so anything outside the token alphabet (a
// line-wrapped paste, a stray quote) is refused here, before Signal sees it.
fn valid_token(token: &str) -> Option<String> {
    let alphabet = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    (!token.is_empty() && token.chars().all(alphabet)).then(|| token.to_owned())
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

    /// A link wrapped across lines when pasted over SSH, or carrying any
    /// character Signal tokens never contain, must be refused before it spends
    /// a registration attempt at Signal.
    #[test]
    fn a_line_wrapped_or_malformed_token_is_refused() {
        let (head, tail) = HCAPTCHA.split_at(40);
        for bad in [
            format!("signalcaptcha://{head}\n{tail}"),
            format!("signalcaptcha://{head} {tail}"),
            format!("signalcaptcha://{HCAPTCHA}\""),
            format!("signalcaptcha://{HCAPTCHA}?x=1"),
            format!("signalcaptcha://{head}/{tail}"),
        ] {
            assert_eq!(captcha_token(&bad), None, "accepted {bad:?}");
            assert_eq!(pasted_captcha_token(&bad), None, "accepted {bad:?}");
        }
    }

    /// People paste either the whole link or just the token copied out of it;
    /// both yield the token, under the same checks.
    #[test]
    fn a_pasted_link_or_bare_token_yields_the_token() {
        for token in [HCAPTCHA, RECAPTCHA] {
            for pasted in [format!("signalcaptcha://{token}"), format!(" {token}\n")] {
                assert_eq!(
                    pasted_captcha_token(&pasted).as_deref(),
                    Some(token),
                    "pasted {pasted:?}"
                );
            }
        }
        let (head, tail) = RECAPTCHA.split_at(30);
        for bad in [
            format!("{head} {tail}"),
            "+15555550100".to_owned(),
            "hcaptcha.5fad".to_owned(),
            String::new(),
        ] {
            assert_eq!(pasted_captcha_token(&bad), None, "accepted {bad:?}");
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

    /// The captcha webview shows a page from the open web inside our window,
    /// so it may load Signal's captcha site and the captcha providers it
    /// embeds over TLS and nothing else: no lookalike hosts, no plain HTTP,
    /// no wandering off to the rest of google.com.
    #[test]
    fn the_captcha_webview_loads_only_signal_and_captcha_hosts() {
        let allowed = [
            CAPTCHA_PAGE,
            "https://signalcaptchas.org/registration/generate.html?lang=de",
            "https://hcaptcha.com/1/api.js?render=explicit",
            "https://newassets.hcaptcha.com/captcha/v1/abc/static/hcaptcha.html#frame=checkbox",
            "https://api.hcaptcha.com/checksiteconfig?v=abc",
            "https://imgs.hcaptcha.com/tile.png",
            "https://www.google.com/recaptcha/api.js",
            "https://www.recaptcha.net/recaptcha/api2/anchor?k=abc",
            "https://www.gstatic.com/recaptcha/releases/abc/recaptcha__en.js",
            "about:blank",
        ];
        let refused = [
            "http://signalcaptchas.org/registration/generate.html",
            "https://signalcaptchas.org.evil.example/registration/generate.html",
            "https://evilhcaptcha.com/captcha",
            "https://hcaptcha.com.evil.example/captcha",
            "https://www.google.com/search?q=signal",
            "https://accounts.google.com/recaptcha/",
            "https://www.gstatic.com/firebasejs/app.js",
            "https://signalcaptchas.org@evil.example/",
            "file:///etc/passwd",
            "signalcaptcha://signal-hcaptcha.abc",
            "not a url",
        ];
        for url in allowed {
            assert!(captcha_navigation_allowed(url), "refused {url:?}");
        }
        for url in refused {
            assert!(!captcha_navigation_allowed(url), "allowed {url:?}");
        }
    }
}
