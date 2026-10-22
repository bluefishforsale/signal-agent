//! A pretend setup that walks every screen without Signal or Claude, so the
//! window can be built and checked before the real driver is wired in.
//! Development builds only (`--features mock-driver`).
//!
//! Code `000000` fails verification, to reach the failure screen.

use std::time::Duration;

use relay_setup::prelude::*;

pub fn script(command: &Command) -> Vec<(Duration, SetupEvent)> {
    let ms = Duration::from_millis;
    match command {
        Command::StartLink { .. } => {
            let mut events: Vec<_> = (1..=3)
                .map(|n| {
                    let uri = format!(
                        "sgnl://linkdevice?uuid=mock-{n}-Tm9uY2UgdGhhdCBjaGFuZ2VzIGVhY2ggcmVmcmVzaA&pub_key=BdHJ1ZSBrZXkgZ29lcyBoZXJl"
                    );
                    let pause = if n == 1 { ms(800) } else { ms(4000) };
                    (pause, SetupEvent::LinkUri { uri })
                })
                .collect();
            events.push((ms(4000), SetupEvent::Linked));
            events
        }
        Command::RequestCode { captcha, .. } => vec![(
            ms(1000),
            match captcha {
                None => SetupEvent::CaptchaRequired,
                Some(_) => SetupEvent::CodeSent,
            },
        )],
        Command::VerifyCode { code } => vec![(
            ms(1000),
            match code.as_str() {
                "000000" => SetupEvent::CommandFailed {
                    message: "That code didn't work. Check the text message and try again.".into(),
                },
                _ => SetupEvent::Registered,
            },
        )],
        Command::ImportHistory => {
            let total = 1200;
            let mut events: Vec<_> = (0..=total)
                .step_by(100)
                .map(|imported| {
                    (
                        ms(400),
                        SetupEvent::HistoryProgress {
                            imported,
                            total: Some(total),
                        },
                    )
                })
                .collect();
            events.push((Duration::ZERO, SetupEvent::HistoryDone));
            events
        }
        Command::FindDesktop => vec![(ms(400), SetupEvent::DesktopFound)],
        Command::ImportDesktop => {
            let mut events: Vec<_> = (0..=3000)
                .step_by(500)
                .map(|imported| (ms(400), SetupEvent::DesktopProgress { imported }))
                .collect();
            events.push((Duration::ZERO, SetupEvent::DesktopImportDone));
            events
        }
        Command::ConnectClients => vec![(
            ms(1200),
            SetupEvent::ClientsConnected {
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
            },
        )],
        Command::SaveAccess { .. } | Command::RestartDesktop => Vec::new(),
    }
}
