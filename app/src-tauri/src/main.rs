//! The native setup window for macOS and Windows. A thin renderer over
//! relay-setup's state machine; all setup behavior lives there, not here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
#[cfg(feature = "mock-driver")]
mod mock;
#[cfg(target_os = "macos")]
mod placement;
#[cfg_attr(feature = "mock-driver", allow(dead_code))]
mod real;
mod scripted;
mod session;
mod view;

use std::time::Duration;

use relay_setup::prelude::*;
use scripted::Scripted;
use session::{Session, Setup};
use tauri::{AppHandle, Emitter, Manager, State};
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
    let result = tauri::Builder::default()
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
            needs_move,
            move_to_applications
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("MCP Relay for Signal failed to start: {e}");
        std::process::exit(1);
    }
}
