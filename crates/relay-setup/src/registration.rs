//! Registering the agent's own number: catching the captcha token Signal's
//! page hands back, so nobody copies a link by hand.

use relay_signal::{Provisioner, RegistrationStep};
use url::Url;

use crate::{Command, SetupEvent};

/// Performs `command` if it is a registration step, asking `provisioner` once,
/// and reports what happened as the event to feed back into setup.
pub async fn run_registration(
    command: &Command,
    provisioner: &dyn Provisioner,
) -> Option<SetupEvent> {
    // NOTE: exactly one call per command, never a retry loop. Signal's
    // registration rate limits are its strictest, and a client that retries on
    // its own looks like abuse; the user retries, and only by acting.
    let step = match command {
        Command::RequestCode { number, captcha } => {
            provisioner
                .request_code(number, captcha.as_deref(), false)
                .await
        }
        Command::VerifyCode { code } => provisioner.verify_code(code).await,
        _ => return None,
    };
    Some(match step {
        Ok(RegistrationStep::CaptchaRequired) => SetupEvent::CaptchaRequired,
        Ok(RegistrationStep::CodeSent) => SetupEvent::CodeSent,
        Ok(RegistrationStep::Registered { .. }) => SetupEvent::Registered,
        Err(e) => SetupEvent::CommandFailed {
            message: format!("{e:#}"),
        },
    })
}

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
    let token = token.trim_end_matches('/');
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::{StreamExt, stream::BoxStream};
    use relay_signal::LinkEvent;

    use super::*;
    use crate::{SetupState, transition};

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

    /// Stands in for Signal answering every code request with a rate limit.
    #[derive(Default)]
    struct RateLimited {
        requests: AtomicUsize,
    }

    const RATE_LIMIT: &str = "Rate limit exceeded: 413, retry after 86400 seconds";

    #[async_trait::async_trait]
    impl Provisioner for RateLimited {
        fn link(&self, _: String) -> BoxStream<'static, anyhow::Result<LinkEvent>> {
            futures::stream::empty().boxed()
        }

        async fn request_code(
            &self,
            _: &str,
            _: Option<&str>,
            _: bool,
        ) -> anyhow::Result<RegistrationStep> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!(RATE_LIMIT)
        }

        async fn verify_code(&self, _: &str) -> anyhow::Result<RegistrationStep> {
            anyhow::bail!(RATE_LIMIT)
        }
    }

    /// Registration is where Signal's anti-abuse limits bite hardest: a
    /// rate-limited request must reach Signal exactly once, show Signal's own
    /// words, and wait for the user rather than trying again by itself.
    #[tokio::test]
    async fn a_rate_limited_code_request_is_sent_once_and_shown_verbatim() {
        let signal = RateLimited::default();
        let state = SetupState::RequestingCode {
            number: "+15555550123".into(),
        };
        let command = Command::RequestCode {
            number: "+15555550123".into(),
            captcha: Some(HCAPTCHA.into()),
        };

        let event = run_registration(&command, &signal)
            .await
            .expect("a registration command reports an event");
        let (failed, commands) = transition(&state, event);
        let SetupState::Failed { message, .. } = &failed else {
            panic!("rate limit did not fail setup: {failed:?}");
        };
        assert!(
            message.contains(RATE_LIMIT),
            "Signal's words lost: {message:?}"
        );
        assert!(
            commands.is_empty(),
            "failure re-sent something: {commands:?}"
        );

        let (resumed, commands) = transition(&failed, SetupEvent::Retry);
        assert_eq!(resumed, SetupState::EnterNumber);
        assert!(commands.is_empty(), "retry re-sent by itself: {commands:?}");
        assert_eq!(signal.requests.load(Ordering::SeqCst), 1);
    }

    /// Stands in for Signal asking for a captcha before it will send a code.
    #[derive(Default)]
    struct WantsCaptcha {
        captchas: std::sync::Mutex<Vec<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl Provisioner for WantsCaptcha {
        fn link(&self, _: String) -> BoxStream<'static, anyhow::Result<LinkEvent>> {
            futures::stream::empty().boxed()
        }

        async fn request_code(
            &self,
            _: &str,
            captcha: Option<&str>,
            _: bool,
        ) -> anyhow::Result<RegistrationStep> {
            let mut captchas = self.captchas.lock().expect("captchas lock");
            captchas.push(captcha.map(str::to_owned));
            Ok(match captcha {
                None => RegistrationStep::CaptchaRequired,
                Some(_) => RegistrationStep::CodeSent,
            })
        }

        async fn verify_code(&self, _: &str) -> anyhow::Result<RegistrationStep> {
            Ok(RegistrationStep::Registered {
                account: relay_signal::Account {
                    service_id: relay_core::ServiceId::parse(
                        "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
                    )?,
                    phone_number: Some("+15555550123".into()),
                    kind: relay_core::ProfileKind::Primary,
                    device_name: "Claude".into(),
                },
            })
        }
    }

    /// The whole own-number flow, driven the way a front end drives it: the
    /// token the captcha page navigates to must reach Signal unchanged on the
    /// second request, and the number ends up a primary profile.
    #[tokio::test]
    async fn own_number_registration_carries_the_captured_token_to_signal() {
        let signal = WantsCaptcha::default();
        let mut state = SetupState::EnterNumber;
        let mut pending = vec![SetupEvent::NumberEntered {
            number: "+15555550123".into(),
        }];
        while let Some(event) = pending.pop() {
            let (next, commands) = transition(&state, event);
            state = next;
            for command in &commands {
                pending.extend(run_registration(command, &signal).await);
            }
            match &state {
                SetupState::SolveCaptcha { .. } => {
                    let link = format!("signalcaptcha://{HCAPTCHA}");
                    let token = captcha_token(&link).expect("token from link");
                    pending.push(SetupEvent::CaptchaSolved { token });
                }
                SetupState::EnterCode { .. } => pending.push(SetupEvent::CodeEntered {
                    code: "123456".into(),
                }),
                _ => {}
            }
        }
        assert_eq!(
            state,
            SetupState::ChooseAccess {
                kind: relay_core::ProfileKind::Primary
            }
        );
        assert_eq!(
            *signal.captchas.lock().expect("captchas lock"),
            [None, Some(HCAPTCHA.to_owned())]
        );
    }
}
