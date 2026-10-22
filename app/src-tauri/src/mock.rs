//! A pretend driver that walks every screen without Signal or Claude, so the
//! window can be built and checked before the real driver exists. Development
//! builds only (`--features mock-driver`).
//!
//! Code `000000` fails verification, to reach the failure screen.

use std::{thread, time::Duration};

use relay_setup::prelude::*;

use crate::session::{Driver, Events};

pub struct Mock;

impl Driver for Mock {
    fn run(&self, commands: Vec<Command>, events: Events) {
        thread::spawn(move || {
            for command in commands {
                perform(command, &events);
            }
        });
    }
}

fn perform(command: Command, events: &Events) {
    let pause = |ms| thread::sleep(Duration::from_millis(ms));
    match command {
        Command::StartLink { .. } => {
            for n in 1..=3 {
                pause(if n == 1 { 800 } else { 4000 });
                events(SetupEvent::LinkUri {
                    uri: format!(
                        "sgnl://linkdevice?uuid=mock-{n}-Tm9uY2UgdGhhdCBjaGFuZ2VzIGVhY2ggcmVmcmVzaA&pub_key=BdHJ1ZSBrZXkgZ29lcyBoZXJl"
                    ),
                });
            }
            pause(4000);
            events(SetupEvent::Linked);
        }
        Command::RequestCode { captcha, .. } => {
            pause(1000);
            events(match captcha {
                None => SetupEvent::CaptchaRequired,
                Some(_) => SetupEvent::CodeSent,
            });
        }
        Command::VerifyCode { code } => {
            pause(1000);
            events(match code.as_str() {
                "000000" => SetupEvent::CommandFailed {
                    message: "That code didn't work. Check the text message and try again.".into(),
                },
                _ => SetupEvent::Registered,
            });
        }
        Command::ImportHistory => {
            let total = 1200;
            for imported in (0..=total).step_by(100) {
                pause(400);
                events(SetupEvent::HistoryProgress {
                    imported,
                    total: Some(total),
                });
            }
            events(SetupEvent::HistoryDone);
        }
        Command::ConnectClients => {
            pause(1200);
            events(SetupEvent::ClientsConnected {
                clients: vec![
                    ClientOutcome {
                        client: ClaudeClient::Desktop,
                        result: Ok(()),
                    },
                    ClientOutcome {
                        client: ClaudeClient::Code,
                        result: Err("Claude Code isn't installed on this computer.".into()),
                    },
                ],
            });
        }
        Command::SaveAccess { .. } | Command::RestartDesktop => {}
    }
}
