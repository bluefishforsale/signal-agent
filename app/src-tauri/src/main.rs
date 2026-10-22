//! The native setup window for macOS and Windows. A thin renderer over
//! relay-setup's state machine; all setup behavior lives there, not here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod captcha;
mod input;
#[cfg(feature = "mock-driver")]
mod mock;
#[cfg(target_os = "macos")]
mod placement;
#[cfg_attr(feature = "mock-driver", allow(dead_code))]
mod real;
mod scripted;
mod session;
mod updater;
mod view;

use std::time::Duration;

use relay_setup::prelude::*;
use scripted::Scripted;
use session::{Session, Setup};
use tauri::{AppHandle, Emitter, Manager, State, Webview};
use view::View;

// Only macOS translocates apps; elsewhere the app is always where it belongs.
#[cfg(not(target_os = "macos"))]
mod placement {
    use std::path::{Path, PathBuf};

    pub fn misplaced_bundle() -> Option<PathBuf> {
        None
    }

    pub fn move_to_applications(_bundle: &Path) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Who carries setup forward: relay-setup's driver, or the scripted
/// walk-through in development builds.
#[cfg(not(feature = "mock-driver"))]
fn setup() -> Box<dyn Setup> {
    let start = start_state();
    real::setup(start.clone()).unwrap_or_else(|e| Box::new(Scripted::new(start, unavailable(e))))
}

#[cfg(feature = "mock-driver")]
fn setup() -> Box<dyn Setup> {
    Box::new(Scripted::new(start_state(), mock::script))
}

/// Fails each step with why setup couldn't start, instead of pretending.
#[cfg_attr(feature = "mock-driver", allow(dead_code))]
fn unavailable(why: anyhow::Error) -> impl Fn(&Command) -> Vec<(Duration, SetupEvent)> {
    let message = format!("Setup can't start on this computer: {why:#}");
    move |_| {
        vec![(
            Duration::ZERO,
            SetupEvent::CommandFailed {
                message: message.clone(),
            },
        )]
    }
}

fn start_state() -> SetupState {
    // Lets a development build open on any screen, for screenshots.
    #[cfg(feature = "mock-driver")]
    if let Some(state) = std::env::var("RELAY_SETUP_STATE")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
    {
        return state;
    }
    SetupState::start(&computer_name())
}

/// The name the user gave this computer, which the phone shows for the link.
fn computer_name() -> String {
    let name = if cfg!(target_os = "macos") {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| String::from_utf8(out.stdout).ok())
    } else {
        std::env::var("COMPUTERNAME").ok()
    };
    name.map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this computer".into())
}

#[tauri::command]
fn current(session: State<'_, Session>) -> View {
    session.view()
}

#[tauri::command]
fn send(event: SetupEvent, session: State<'_, Session>, app: AppHandle) {
    let Some(event) = input::from_page(event) else {
        // Unchanged, but the page waits on a redraw to re-enable its button.
        if let Err(e) = app.emit("setup", session.view()) {
            eprintln!("unable to redraw the setup window: {e}");
        }
        return;
    };
    session.send(event);
}

#[tauri::command]
fn open_captcha(app: AppHandle) -> Result<(), String> {
    let handle = app.clone();
    captcha::open(&app, move |token| {
        handle
            .state::<Session>()
            .send(SetupEvent::CaptchaSolved { token })
    })
    .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn update_offer(offered: State<'_, updater::Offered>) -> Option<updater::Offer> {
    offered.get()
}

#[tauri::command]
async fn install_update(webview: Webview, app: AppHandle) -> Result<(), String> {
    from_main_window(webview.label())?;
    updater::install(&app).await.map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn uninstall(
    webview: Webview,
    app: AppHandle,
    delete_data: bool,
) -> Result<Vec<StepOutcome>, String> {
    from_main_window(webview.label())?;
    // NOTE: the page's checkbox alone must not be able to delete messages;
    // a native dialog the page can't answer asks once more.
    if delete_data && !confirmed_deleting_data(&app).await {
        return Err("Nothing was removed: deleting your messages wasn't confirmed".into());
    }
    run_uninstall(delete_data)
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Refuses a command that can't be undone unless it came from the local
/// setup window; capabilities already keep it from the captcha window, and
/// this holds even if they are loosened by mistake.
fn from_main_window(label: &str) -> Result<(), String> {
    if label != "main" {
        return Err(format!("the \"{label}\" window may not do this"));
    }
    Ok(())
}

async fn confirmed_deleting_data(app: &AppHandle) -> bool {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .message(
            "Uninstall will delete your Signal messages, account data and the keys that unlock them from this computer. This can't be undone.",
        )
        .title("Delete your messages?")
        .kind(MessageDialogKind::Warning)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Delete them".into(),
            "Keep them".into(),
        ))
        .show(move |confirmed| {
            let _ = tx.send(confirmed);
        });
    rx.await.unwrap_or(false)
}

#[cfg(not(feature = "mock-driver"))]
async fn run_uninstall(delete_data: bool) -> anyhow::Result<Vec<StepOutcome>> {
    let agent = real::agent()?;
    tauri::async_runtime::spawn_blocking(move || {
        relay_setup::uninstall(&agent, relay_agent::login::unregister, delete_data)
    })
    .await?
}

// NOTE: development builds run on the developer's own machine, so their
// Uninstall button must never remove the developer's real setup.
#[cfg(feature = "mock-driver")]
async fn run_uninstall(_delete_data: bool) -> anyhow::Result<Vec<StepOutcome>> {
    Ok(vec![StepOutcome {
        name: "Nothing removed: this is a development build".into(),
        result: Ok(()),
    }])
}

#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn needs_move() -> bool {
    placement::misplaced_bundle().is_some()
}

#[tauri::command]
fn move_to_applications(app: AppHandle) -> Result<(), String> {
    let Some(bundle) = placement::misplaced_bundle() else {
        return Ok(());
    };
    placement::move_to_applications(&bundle).map_err(|e| format!("{e:#}"))?;
    app.exit(0);
    Ok(())
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--check-for-update") {
        if let Err(e) = updater::check_in_background() {
            eprintln!("unable to look for an update: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .manage(updater::Offered::default())
        .setup(|app| {
            let handle = app.handle().clone();
            let redraw = move |view: &View| {
                if let Err(e) = handle.emit("setup", view) {
                    eprintln!("unable to redraw the setup window: {e}");
                }
            };
            let (session, run) = Session::new(setup(), redraw);
            app.manage(session);
            tauri::async_runtime::spawn(run);
            tauri::async_runtime::spawn(updater::watch(app.handle().clone()));
            // Screenshot mode for development builds: a fixed theme, kept on top.
            #[cfg(feature = "mock-driver")]
            if let (Ok(theme), Some(window)) = (
                std::env::var("RELAY_SCREENSHOT"),
                app.get_webview_window("main"),
            ) {
                let light = theme == "light";
                window.set_theme(Some(if light {
                    tauri::Theme::Light
                } else {
                    tauri::Theme::Dark
                }))?;
                window.set_always_on_top(true)?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            current,
            send,
            open_captcha,
            needs_move,
            move_to_applications,
            update_offer,
            install_update,
            uninstall,
            quit
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("MCP Relay for Signal failed to start: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    include!("../app_commands.rs");

    use super::from_main_window;

    /// Uninstall and Install must refuse any window but the setup window,
    /// above all the captcha window, which shows Signal's remote page.
    #[test]
    fn irreversible_commands_refuse_every_window_but_main() {
        assert_eq!(from_main_window("main"), Ok(()));
        for other in ["captcha", "", "Main"] {
            assert!(from_main_window(other).is_err(), "{other:?} allowed");
        }
    }

    /// Only the local main window may call app commands, and it may call
    /// exactly the declared ones; no capability may reach a remote page or
    /// the captcha window.
    #[test]
    fn capabilities_grant_app_commands_to_the_local_main_window_only() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("capabilities");
        for entry in std::fs::read_dir(&dir).expect("capabilities dir") {
            let path = entry.expect("entry").path();
            let cap: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
            assert_eq!(
                cap["windows"],
                serde_json::json!(["main"]),
                "{}",
                path.display()
            );
            assert!(cap.get("webviews").is_none(), "{}", path.display());
            assert!(
                cap.get("remote").is_none(),
                "{} reaches remote pages",
                path.display()
            );
            assert_ne!(cap["local"], serde_json::json!(false), "{}", path.display());
            let granted: Vec<&str> = cap["permissions"]
                .as_array()
                .expect("permissions")
                .iter()
                .filter_map(|p| p.as_str()?.strip_prefix("allow-"))
                .collect();
            let declared: Vec<String> = APP_COMMANDS.iter().map(|c| c.replace('_', "-")).collect();
            assert_eq!(granted, declared, "{}", path.display());
        }
    }

    /// The box that deletes messages and keys must say exactly that, in the
    /// words the acceptance checklist tells the tester to look for.
    #[test]
    fn the_delete_data_box_carries_the_checklist_label() {
        let page = include_str!("../../ui/index.html");
        let label = format!(
            "<input type=\"checkbox\" name=\"delete_data\">\n        <span><strong>{}</strong>",
            relay_setup::DELETE_DATA_LABEL
        );
        assert!(page.contains(&label), "index.html lacks {label:?}");
    }
}
