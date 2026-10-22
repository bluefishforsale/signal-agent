//! Signal's captcha inside our own window. The page runs in a locked-down
//! webview, and the `signalcaptcha://` link it navigates to when solved is
//! caught on the way out, so nobody copies a link by hand.

use anyhow::Context;
use relay_setup::{CAPTCHA_PAGE, captcha_navigation_allowed, captcha_token};
use tauri::{
    AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, webview::NewWindowResponse,
};

const LABEL: &str = "captcha";

/// Opens Signal's captcha page over the setup window and calls `solved` with
/// the token once the user passes it. Opening it again focuses the open one.
pub fn open<R: Runtime>(
    app: &AppHandle<R>,
    solved: impl Fn(String) + Send + 'static,
) -> anyhow::Result<()> {
    if let Some(window) = app.get_webview_window(LABEL) {
        return window
            .set_focus()
            .context("unable to focus the captcha window");
    }
    let page = CAPTCHA_PAGE.parse().context("captcha page URL")?;
    let closer = app.clone();
    let mut builder = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::External(page))
        .title("Show Signal you're a person")
        .inner_size(480.0, 640.0)
        .on_navigation(move |url| match navigation(url.as_str()) {
            Navigation::Solved(token) => {
                solved(token);
                close(closer.clone());
                false
            }
            Navigation::Allow => true,
            Navigation::Block => false,
        })
        .on_new_window(|_, _| NewWindowResponse::Deny);
    if let Some(main) = app.get_webview_window("main") {
        builder = builder.parent(&main)?;
    }
    builder
        .build()
        .context("unable to open the captcha window")?;
    Ok(())
}

/// What the captcha webview does with one navigation.
#[derive(Debug, PartialEq, Eq)]
enum Navigation {
    /// The page finished: hand over the token and close.
    Solved(String),
    Allow,
    Block,
}

fn navigation(url: &str) -> Navigation {
    if let Some(token) = captcha_token(url) {
        return Navigation::Solved(token);
    }
    if captcha_navigation_allowed(url) {
        return Navigation::Allow;
    }
    Navigation::Block
}

// NOTE: closed from another task, never from inside the navigation callback.
// That callback runs inside the webview's own navigation delegate, and tearing
// the webview down synchronously there can crash WebKit.
fn close<R: Runtime>(app: AppHandle<R>) {
    tauri::async_runtime::spawn(async move {
        let Some(window) = app.get_webview_window(LABEL) else {
            return;
        };
        if let Err(e) = window.close() {
            eprintln!("unable to close the captcha window: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "signal-hcaptcha.5fad97ac-7d06-4e44-b18a-b950b20148ff.registration.P1_eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.k2Zq-Xw_3bXr9aQ";

    /// A malformed result link spends a registration attempt if it reaches
    /// Signal, so it must be blocked without reporting a solved captcha, the
    /// same as any other off-list navigation.
    #[test]
    fn a_malformed_captcha_link_is_blocked_without_a_token() {
        for url in [
            format!("signalcaptcha://{TOKEN}%22"),
            format!("signalcaptcha://{TOKEN}?next=x"),
            format!("signalcaptcha://{TOKEN}/extra"),
            "signalcaptcha://".to_owned(),
        ] {
            assert_eq!(navigation(&url), Navigation::Block, "from {url:?}");
        }
    }

    /// The solved link is the one navigation that reports a token; the
    /// captcha's own pages load, and anything else stays out of the window.
    #[test]
    fn only_the_solved_link_reports_a_token() {
        assert_eq!(
            navigation(&format!("signalcaptcha://{TOKEN}")),
            Navigation::Solved(TOKEN.to_owned())
        );
        assert_eq!(navigation(relay_setup::CAPTCHA_PAGE), Navigation::Allow);
        assert_eq!(navigation("https://example.com/"), Navigation::Block);
    }
}
