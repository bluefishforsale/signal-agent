//! Setup driven by a fixed script instead of Signal and Claude, for the
//! stand-ins the window runs before relay-setup's driver is wired in. It keeps
//! that driver's rules: commands run in order, and a failure drops the rest of
//! its batch.

use std::{collections::VecDeque, time::Duration};

use async_trait::async_trait;
use relay_setup::prelude::*;

use crate::session::{Setup, Show};

/// The events a command produces, each after a pause.
pub type Script = fn(&Command) -> Vec<(Duration, SetupEvent)>;

pub struct Scripted {
    state: SetupState,
    script: Script,
}

impl Scripted {
    pub fn new(state: SetupState, script: Script) -> Self {
        Self { state, script }
    }

    fn apply(&mut self, event: SetupEvent, show: Show<'_>) -> Vec<Command> {
        let (next, commands) = transition(&self.state, event);
        if next != self.state {
            self.state = next;
            show(&self.state);
        }
        commands
    }
}

#[async_trait]
impl Setup for Scripted {
    async fn dispatch(&mut self, event: SetupEvent, show: Show<'_>) {
        let mut pending: VecDeque<Command> = self.apply(event, show).into();
        while let Some(command) = pending.pop_front() {
            for (pause, event) in (self.script)(&command) {
                tokio::time::sleep(pause).await;
                if matches!(event, SetupEvent::CommandFailed { .. }) {
                    pending.clear();
                }
                pending.extend(self.apply(event, show));
            }
        }
    }

    fn state(&self) -> &SetupState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use relay_core::prelude::*;

    use super::*;

    fn save_fails(command: &Command) -> Vec<(Duration, SetupEvent)> {
        let event = match command {
            Command::SaveAccess { .. } => SetupEvent::CommandFailed {
                message: "disk full".into(),
            },
            Command::ConnectClients => SetupEvent::ClientsConnected {
                clients: Vec::new(),
            },
            _ => return Vec::new(),
        };
        vec![(Duration::ZERO, event)]
    }

    /// Like the real driver, a failed save must stop the clients from
    /// connecting, or they would get a profile in the wrong access mode.
    #[tokio::test]
    async fn a_failed_command_drops_the_rest_of_its_batch() {
        let mut setup = Scripted::new(
            SetupState::ChooseAccess {
                kind: ProfileKind::Primary,
            },
            save_fails,
        );
        setup
            .dispatch(
                SetupEvent::AccessChosen {
                    access: AccessMode::Full,
                },
                &mut |_| {},
            )
            .await;
        assert!(
            matches!(setup.state(), SetupState::Failed { message, .. } if message == "disk full"),
            "{:?}",
            setup.state()
        );
    }
}
