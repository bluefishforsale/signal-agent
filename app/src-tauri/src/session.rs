//! The window's one setup driver, run as a single task. Clicks queue on a
//! channel and the driver works through them in order, so a long step such as
//! waiting for the phone to scan never blocks the window.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use relay_setup::prelude::*;
use tokio::sync::mpsc;

use crate::view::{View, view};

/// Called with each state setup enters.
pub type Show<'a> = &'a mut (dyn FnMut(&SetupState) + Send);

/// Carries setup forward: relay-setup's driver, or a stand-in.
#[async_trait]
pub trait Setup: Send {
    /// Applies `event` and runs what follows, showing each state entered,
    /// until setup waits on the user again.
    async fn dispatch(&mut self, event: SetupEvent, show: Show<'_>);

    fn state(&self) -> &SetupState;
}

/// Setup in progress, as the window's commands see it.
pub struct Session {
    events: mpsc::UnboundedSender<SetupEvent>,
    screen: Arc<Mutex<View>>,
}

impl Session {
    /// The session, and the task that drives `setup`, which the caller spawns.
    /// `on_change` redraws the window.
    pub fn new(
        mut setup: Box<dyn Setup>,
        on_change: impl Fn(&View) + Send + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let (events, mut queued) = mpsc::unbounded_channel();
        let screen = Arc::new(Mutex::new(view(setup.state())));
        let shown = Arc::clone(&screen);
        let run = async move {
            let mut redraw = move |state: &SetupState| {
                let view = view(state);
                on_change(&view);
                *shown.lock().expect("screen lock") = view;
            };
            while let Some(event) = queued.recv().await {
                setup.dispatch(event, &mut redraw).await;
                // NOTE: a driver shows only changed states, but the page keeps
                // the clicked button disabled until it is redrawn.
                redraw(setup.state());
            }
        };
        (Self { events, screen }, run)
    }

    /// The screen for the current state.
    pub fn view(&self) -> View {
        self.screen.lock().expect("screen lock").clone()
    }

    /// Queues `event` for the driver.
    pub fn send(&self, event: SetupEvent) {
        if self.events.send(event).is_err() {
            eprintln!("setup has stopped; ignoring a click");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc as std_mpsc, time::Duration};

    use super::*;
    use crate::scripted::Scripted;

    fn link_at_once(command: &Command) -> Vec<(Duration, SetupEvent)> {
        match command {
            Command::StartLink { .. } => vec![
                (
                    Duration::ZERO,
                    SetupEvent::LinkUri {
                        uri: "sgnl://x".into(),
                    },
                ),
                (Duration::ZERO, SetupEvent::Linked),
            ],
            _ => Vec::new(),
        }
    }

    fn screen_name(view: &View) -> String {
        serde_json::to_value(view).unwrap()["screen"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn started() -> (Session, std_mpsc::Receiver<String>) {
        let (tx, rx) = std_mpsc::channel();
        let (session, run) = Session::new(
            Box::new(Scripted::new(SetupState::start("Mac"), link_at_once)),
            move |view| tx.send(screen_name(view)).unwrap(),
        );
        tokio::spawn(run);
        (session, rx)
    }

    fn next(rx: &std_mpsc::Receiver<String>) -> String {
        rx.recv_timeout(Duration::from_secs(5))
            .expect("no redraw within 5s")
    }

    /// Every state the driver passes through must reach the window in order,
    /// or a QR refresh or progress step is lost.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_state_the_driver_enters_is_redrawn_in_order() {
        let (session, rx) = started();
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        let screens: Vec<_> = (0..3).map(|_| next(&rx)).collect();
        assert_eq!(screens, ["linking", "linking", "choose_access"]);
        assert_eq!(screen_name(&session.view()), "choose_access");
    }

    /// A click setup ignores changes nothing, but the page still needs a
    /// redraw to re-enable the button it disabled.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_ignored_click_still_redraws() {
        let (session, rx) = started();
        session.send(SetupEvent::Retry);
        assert_eq!(next(&rx), "choose_account");
    }
}
