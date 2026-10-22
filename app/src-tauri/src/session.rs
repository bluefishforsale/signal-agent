//! The window's one live setup state. Clicks from the page and results from the
//! driver both land here, so they go through relay-setup in a single order.

use std::sync::{Arc, Mutex};

use relay_setup::prelude::*;

use crate::view::{View, view};

/// Feeds results back into setup.
pub type Events = Arc<dyn Fn(SetupEvent) + Send + Sync>;

/// Carries out setup's side effects.
pub trait Driver: Send + Sync {
    /// Runs `commands` in order without blocking the caller, reporting each
    /// result through `events`.
    fn run(&self, commands: Vec<Command>, events: Events);
}

/// Setup in progress, redrawn through `on_change` after every event.
pub struct Session {
    state: Mutex<SetupState>,
    driver: Box<dyn Driver>,
    on_change: Box<dyn Fn(&View) + Send + Sync>,
}

impl Session {
    pub fn new(
        start: SetupState,
        driver: Box<dyn Driver>,
        on_change: Box<dyn Fn(&View) + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(start),
            driver,
            on_change,
        })
    }

    /// The screen for the current state.
    pub fn view(&self) -> View {
        view(&self.state.lock().expect("setup state lock"))
    }

    /// Applies `event` and starts whatever commands it calls for.
    pub fn handle(self: &Arc<Self>, event: SetupEvent) {
        let commands = {
            let mut state = self.state.lock().expect("setup state lock");
            let (next, commands) = transition(&state, event);
            *state = next;
            // Redrawn under the lock so screens reach the page in state order.
            (self.on_change)(&view(&state));
            commands
        };
        if commands.is_empty() {
            return;
        }
        let session = Arc::clone(self);
        self.driver
            .run(commands, Arc::new(move |event| session.handle(event)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Log = Arc<Mutex<Vec<Vec<Command>>>>;

    struct Recording(Log);

    impl Driver for Recording {
        fn run(&self, commands: Vec<Command>, _events: Events) {
            self.0.lock().unwrap().push(commands);
        }
    }

    fn recorded() -> (Arc<Session>, Log) {
        let log = Log::default();
        let session = Session::new(
            SetupState::start("Mac"),
            Box::new(Recording(log.clone())),
            Box::new(|_| {}),
        );
        (session, log)
    }

    /// A click must move the window to the next screen and hand the driver
    /// exactly the commands relay-setup asked for.
    #[test]
    fn an_event_moves_the_screen_and_hands_its_commands_to_the_driver() {
        let (session, log) = recorded();
        session.handle(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        assert!(
            matches!(session.view(), View::Linking { qr: None, .. }),
            "{:?}",
            session.view()
        );
        assert_eq!(
            *log.lock().unwrap(),
            vec![vec![Command::StartLink {
                device_name: "Claude (Mac)".into()
            }]]
        );
    }

    struct Instant;

    impl Driver for Instant {
        fn run(&self, _commands: Vec<Command>, events: Events) {
            events(SetupEvent::LinkUri {
                uri: "sgnl://x".into(),
            });
            events(SetupEvent::Linked);
        }
    }

    /// A driver may answer before `run` returns; that must neither deadlock
    /// on the state lock nor redraw screens out of order.
    #[test]
    fn a_driver_answering_inside_run_neither_deadlocks_nor_reorders_screens() {
        let screens = Arc::new(Mutex::new(Vec::new()));
        let seen = screens.clone();
        let session = Session::new(
            SetupState::start("Mac"),
            Box::new(Instant),
            Box::new(move |view| {
                let name = serde_json::to_value(view).unwrap()["screen"].clone();
                seen.lock().unwrap().push(name.as_str().unwrap().to_owned());
            }),
        );
        session.handle(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        assert_eq!(
            *screens.lock().unwrap(),
            ["linking", "linking", "choose_access"]
        );
    }
}
