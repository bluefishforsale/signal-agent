//! The native setup window for macOS and Windows. A thin renderer over
//! relay-setup's state machine; all setup behavior lives there, not here.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod input;
#[cfg(feature = "mock-driver")]
mod mock;
#[cfg(target_os = "macos")]
mod placement;
mod session;
mod view;

use std::sync::Arc;

use relay_setup::prelude::*;
use session::{Driver, Events, Session};
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

/// Who carries out setup's commands. Swap this for relay-setup's driver.
#[cfg(not(feature = "mock-driver"))]
fn driver() -> Box<dyn Driver> {
    Box::new(NotReady)
}

#[cfg(feature = "mock-driver")]
fn driver() -> Box<dyn Driver> {
    Box::new(mock::Mock)
}

/// Stands in until relay-setup's driver lands, failing each step plainly
/// instead of pretending setup worked.
#[cfg_attr(feature = "mock-driver", allow(dead_code))]
struct NotReady;

impl Driver for NotReady {
    fn run(&self, _commands: Vec<Command>, events: Events) {
        events(SetupEvent::CommandFailed {
            message: "This test build can't finish setup yet.".into(),
        });
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
fn current(session: State<'_, Arc<Session>>) -> View {
    session.view()
}

#[tauri::command]
fn send(event: SetupEvent, session: State<'_, Arc<Session>>) {
    let Some(event) = input::from_page(event) else {
        session.redraw();
        return;
    };
    session.handle(event);
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
            app.manage(Session::new(start_state(), driver(), Box::new(redraw)));
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
