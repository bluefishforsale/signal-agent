//! The window's one setup driver, run as a single task. Clicks queue on a
//! channel and the driver works through them in order, so a long step such as
//! waiting for the phone to scan never blocks the window.

use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::bail;
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
    current: Mutex<Current>,
    screen: Arc<Mutex<View>>,
    on_change: Arc<dyn Fn(&View) + Send + Sync>,
}

/// The setup the window is showing now.
struct Current {
    events: mpsc::UnboundedSender<SetupEvent>,
    skip: Option<HistorySkip>,
    /// Clicks sent and not yet handled.
    unhandled: Arc<AtomicUsize>,
}

impl Session {
    /// The session, and the task that drives `setup`, which the caller spawns.
    /// `on_change` redraws the window.
    pub fn new(
        setup: Box<dyn Setup>,
        on_change: impl Fn(&View) + Send + Sync + 'static,
    ) -> (Self, impl Future<Output = ()> + Send + 'static) {
        let screen = Arc::new(Mutex::new(view(setup.state())));
        let on_change: Arc<dyn Fn(&View) + Send + Sync> = Arc::new(on_change);
        let (current, run) = drive(setup, Arc::clone(&screen), Arc::clone(&on_change));
        (
            Self {
                current: Mutex::new(current),
                screen,
                on_change,
            },
            run,
        )
    }

    /// Shows `setup` in place of the current one, and returns the task that
    /// drives it, which the caller spawns.
    pub fn switch_to(
        &self,
        setup: Box<dyn Setup>,
    ) -> anyhow::Result<impl Future<Output = ()> + Send + 'static> {
        let mut current = self.current.lock().expect("setup lock");
        if current.unhandled.load(Ordering::SeqCst) > 0 {
            bail!("setup is still working; wait for it to finish");
        }
        let at_rest = matches!(
            *self.screen.lock().expect("screen lock"),
            View::AlreadySetUp { .. }
                | View::ChooseAccount { .. }
                | View::Done { .. }
                | View::Failed { .. }
        );
        if !at_rest {
            bail!("finish or close this account's setup first");
        }
        let first = view(setup.state());
        let (next, run) = drive(setup, Arc::clone(&self.screen), Arc::clone(&self.on_change));
        *current = next;
        *self.screen.lock().expect("screen lock") = first.clone();
        (self.on_change)(&first);
        Ok(run)
    }

    /// The screen for the current state.
    pub fn view(&self) -> View {
        self.screen.lock().expect("screen lock").clone()
    }

    /// Queues `event` for the driver. Skip also goes straight to a running
    /// history import, which would otherwise hold the queue.
    pub fn send(&self, event: SetupEvent) {
        let current = self.current.lock().expect("setup lock");
        if let (SetupEvent::SkipHistory, Some(skip)) = (&event, &current.skip) {
            skip.skip();
        }
        current.unhandled.fetch_add(1, Ordering::SeqCst);
        if current.events.send(event).is_err() {
            eprintln!("setup has stopped; ignoring a click");
        }
    }
}

/// The handle on `setup` and the task that drives it, drawing each state it
/// enters into `screen` and through `on_change`.
fn drive(
    mut setup: Box<dyn Setup>,
    screen: Arc<Mutex<View>>,
    on_change: Arc<dyn Fn(&View) + Send + Sync>,
) -> (Current, impl Future<Output = ()> + Send + 'static) {
    let (events, mut queued) = mpsc::unbounded_channel();
    let skip = setup.history_skip();
    let unhandled = Arc::new(AtomicUsize::new(0));
    let handled = Arc::clone(&unhandled);
    let run = async move {
        let mut redraw = move |state: &SetupState| {
            let view = view(state);
            // Stored first: the page may ask for the current screen as
            // soon as it hears of a redraw.
            *screen.lock().expect("screen lock") = view.clone();
            on_change(&view);
        };
        while let Some(event) = queued.recv().await {
            setup.dispatch(event, &mut redraw).await;
            // NOTE: a driver shows only changed states, but the page keeps
            // the clicked button disabled until it is redrawn.
            redraw(setup.state());
            handled.fetch_sub(1, Ordering::SeqCst);
        }
    };
    (
        Current {
            events,
            skip,
            unhandled,
        },
        run,
    )
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

    /// "Add another account" swaps in setup for the new profile: the window
    /// must show that setup's screens, and clicks must reach it.
    #[tokio::test(start_paused = true)]
    async fn a_switched_in_setup_gets_the_window_and_the_clicks() {
        let (session, mut rx) = started();
        let run = session
            .switch_to(Box::new(Scripted::new(
                SetupState::EnterNumber,
                link_at_once,
            )))
            .expect("an idle setup switches");
        tokio::spawn(run);
        assert_eq!(next(&mut rx).await, "enter_number");
        session.send(SetupEvent::NumberEntered {
            number: "+15555550123".into(),
        });
        assert_eq!(next(&mut rx).await, "requesting_code");
    }

    /// A click still queued or running would carry on against the setup
    /// being replaced, which for the real driver may be mid-link with Signal,
    /// so switching must wait until setup is idle.
    #[tokio::test(start_paused = true)]
    async fn switching_is_refused_while_a_click_is_queued_or_running() {
        let (session, mut rx) = started();
        session.send(SetupEvent::AccountChosen {
            choice: AccountChoice::LinkExisting,
        });
        let switched = session.switch_to(scripted());
        assert!(switched.is_err(), "switched with a click queued");
        assert_eq!(next(&mut rx).await, "linking");
    }

    /// Between screens of one account's setup (a number entered, a code on
    /// its way) switching would drop that setup halfway, so only a setup that
    /// hasn't started or has ended may be replaced.
    #[tokio::test(start_paused = true)]
    async fn switching_is_refused_halfway_through_a_setup() {
        let (session, _run) = Session::new(
            Box::new(Scripted::new(SetupState::EnterNumber, link_at_once)),
            |_| {},
        );
        let switched = session.switch_to(scripted());
        assert!(switched.is_err(), "switched halfway through");
    }

    /// Opening the app on a set-up account shows "already set up", where
    /// Advanced offers Accounts, so adding another account must work there.
    #[tokio::test(start_paused = true)]
    async fn an_already_set_up_account_can_be_switched_out() {
        let (session, _run) = Session::new(
            Box::new(Scripted::new(
                SetupState::for_profile("Mac", Some(relay_core::AccessMode::Full)),
                link_at_once,
            )),
            |_| {},
        );
        let switched = session.switch_to(scripted());
        assert!(switched.is_ok(), "refused from already set up");
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
