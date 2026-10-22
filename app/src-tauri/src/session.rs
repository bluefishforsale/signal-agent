//! The window's one setup driver, run as a single task. Clicks queue on a
//! channel and the driver works through them in order, so a long step such as
//! waiting for the phone to scan never blocks the window.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use relay_setup::{driver::HistorySkip, prelude::*};
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

    /// Stops a history import while `dispatch` is still running it.
    fn history_skip(&self) -> Option<HistorySkip> {
        None
    }
}

/// Setup in progress, as the window's commands see it.
pub struct Session {
    events: mpsc::UnboundedSender<SetupEvent>,
    screen: Arc<Mutex<View>>,
    skip: Option<HistorySkip>,
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
        let skip = setup.history_skip();
        let shown = Arc::clone(&screen);
        let run = async move {
            let mut redraw = move |state: &SetupState| {
                let view = view(state);
                // Stored first: the page may ask for the current screen as
                // soon as it hears of a redraw.
                *shown.lock().expect("screen lock") = view.clone();
                on_change(&view);
            };
            while let Some(event) = queued.recv().await {
                setup.dispatch(event, &mut redraw).await;
                // NOTE: a driver shows only changed states, but the page keeps
                // the clicked button disabled until it is redrawn.
                redraw(setup.state());
            }
        };
        (
            Self {
                events,
                screen,
                skip,
            },
            run,
        )
    }

    /// The screen for the current state.
    pub fn view(&self) -> View {
        self.screen.lock().expect("screen lock").clone()
    }

    /// Queues `event` for the driver. Skip also goes straight to a running
    /// history import, which would otherwise hold the queue.
    pub fn send(&self, event: SetupEvent) {
        if let (SetupEvent::SkipHistory, Some(skip)) = (&event, &self.skip) {
            skip.skip();
        }
        if self.events.send(event).is_err() {
            eprintln!("setup has stopped; ignoring a click");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::OnceLock, time::Duration};

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

    fn scripted() -> Box<dyn Setup> {
        Box::new(Scripted::new(SetupState::start("Mac"), link_at_once))
    }

    fn started() -> (Session, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (session, run) = Session::new(scripted(), move |view| {
            tx.send(screen_name(view)).unwrap();
        });
        tokio::spawn(run);
        (session, rx)
    }

    // The clock is paused, so this timeout never waits in real time: it fires
    // only if the driver task can make no further progress.
    async fn next(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("driver stalled before redrawing")
            .expect("driver task ended")
    }

    /// Every state the driver passes through must reach the window in order,
    /// or a QR refresh or progress step is lost.
    #[tokio::test(start_paused = true)]
    async fn every_state_the_driver_enters_is_redrawn_in_order() {
        let (session, mut rx) = started();
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        let mut screens = Vec::new();
        for _ in 0..3 {
            screens.push(next(&mut rx).await);
        }
        assert_eq!(screens, ["linking", "linking", "choose_access"]);
        assert_eq!(screen_name(&session.view()), "choose_access");
    }

    /// The page reads the current screen as soon as it is told to redraw, so
    /// that screen must already be stored when the redraw goes out. It once
    /// lagged by one state and flaked on Windows CI.
    #[tokio::test(start_paused = true)]
    async fn a_redraw_stores_the_new_screen_before_anyone_is_told_it_changed() {
        let screen: Arc<OnceLock<Arc<Mutex<View>>>> = Arc::default();
        let stored = Arc::clone(&screen);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (session, run) = Session::new(scripted(), move |view: &View| {
            let current = stored.get().unwrap().lock().unwrap().clone();
            tx.send((screen_name(view), current == *view)).unwrap();
        });
        screen.set(Arc::clone(&session.screen)).unwrap();
        tokio::spawn(run);
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        for _ in 0..3 {
            let (name, current) = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("driver stalled before redrawing")
                .expect("driver task ended");
            assert!(current, "redraw of {name} went out before it was stored");
        }
    }

    /// A click setup ignores changes nothing, but the page still needs a
    /// redraw to re-enable the button it disabled.
    #[tokio::test(start_paused = true)]
    async fn an_ignored_click_still_redraws() {
        let (session, mut rx) = started();
        session.send(SetupEvent::Retry);
        assert_eq!(next(&mut rx).await, "choose_account");
    }
}
