// SPDX-FileCopyrightText: 2026 eagle contributors
// SPDX-License-Identifier: GPL-3.0-or-later

//! Multi-step sign-in. Each flow runs on its own thread, so waiting for the user never blocks
//! the session or the catalog worker, and reports back to the session through a callback.
//! Codes, links and tokens are never logged.

use eagle_core::{
    Provider, ProviderError, SignInFlow, SignInInput, SignInMethod, SignInStep, Token,
};
use std::{
    collections::HashMap,
    fmt, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// A browser sign-in that nobody finishes is given up after this long.
const BROWSER_LIMIT: Duration = Duration::from_secs(10 * 60);
/// Shortest wait between two polls, whatever the provider asks for.
const MIN_INTERVAL: Duration = Duration::from_millis(10);
/// How often a waiting flow looks at its cancel flag.
const CANCEL_SLICE: Duration = Duration::from_millis(50);

/// What the sign-in of one provider shows to the user.
#[derive(Clone, PartialEq, Eq)]
pub enum SignInState {
    /// Nothing is going on: no flow, or the last one has finished.
    Idle,
    /// Work is running: a code is being requested, a token or password is being checked.
    Busy,
    /// The user enters `code` at `url`; `expired` once the code can no longer be used.
    Code {
        code: String,
        url: String,
        expired: bool,
    },
    /// The user finishes signing in at `url` in the browser.
    Browser { url: String },
    /// The provider asks for a second factor; `hint` says where to find it.
    SecondFactor { hint: String },
    /// The last attempt failed; the text is for the user.
    Failed(String),
}

impl fmt::Debug for SignInState {
    // The code and the link stay out of logs and test output.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Idle => "Idle",
            Self::Busy => "Busy",
            Self::Code { .. } => "Code",
            Self::Browser { .. } => "Browser",
            Self::SecondFactor { .. } => "SecondFactor",
            Self::Failed(_) => "Failed",
        })
    }
}

/// What a flow thread (or a finished token sign-in) tells the session.
pub(crate) enum SignInUpdate {
    Code {
        code: String,
        url: String,
    },
    Browser {
        url: String,
    },
    SecondFactor {
        hint: String,
        flow: SignInFlow,
    },
    Expired,
    Failed(String),
    /// The provider handed out a token; the session signs in with it.
    Done(Token),
    /// A token sign-in went through; the account state is already published.
    Finished,
}

/// The step a flow thread starts with.
pub(crate) enum Task {
    Start {
        method: SignInMethod,
        input: SignInInput,
    },
    Continue {
        flow: SignInFlow,
        code: Option<Token>,
    },
}

/// One provider's running flow.
pub(crate) struct Active {
    pub generation: u64,
    cancel: Arc<AtomicBool>,
    /// Kept while a second factor is awaited.
    pub flow: Option<SignInFlow>,
    /// Last code and link shown, to keep them on screen after the code expires.
    pub shown: Option<(String, String)>,
    pub browser: bool,
}

/// Running flows by provider id. Dropping it cancels all of them.
#[derive(Default)]
pub(crate) struct SignIns {
    active: HashMap<String, Active>,
    generation: u64,
}

impl SignIns {
    /// Cancels the provider's running flow and registers a new one.
    pub fn begin(&mut self, provider: &str) -> (u64, Arc<AtomicBool>) {
        self.cancel(provider);
        self.generation += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        self.active.insert(
            provider.to_owned(),
            Active {
                generation: self.generation,
                cancel: cancel.clone(),
                flow: None,
                shown: None,
                browser: false,
            },
        );
        (self.generation, cancel)
    }
    /// Stops the provider's flow; true if there was one.
    pub fn cancel(&mut self, provider: &str) -> bool {
        match self.active.remove(provider) {
            Some(active) => {
                active.cancel.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }
    /// The flow of `generation`; updates from a flow that was replaced or cancelled find none.
    pub fn current(&mut self, provider: &str, generation: u64) -> Option<&mut Active> {
        self.active
            .get_mut(provider)
            .filter(|active| active.generation == generation)
    }
    pub fn get(&mut self, provider: &str) -> Option<&mut Active> {
        self.active.get_mut(provider)
    }
}

impl Drop for SignIns {
    fn drop(&mut self) {
        for active in self.active.values() {
            active.cancel.store(true, Ordering::Release);
        }
    }
}

/// Runs `task` and then polls until the flow ends, is cancelled, or `report` returns false
/// because nobody listens any more.
pub(crate) fn spawn(
    provider: Arc<dyn Provider>,
    task: Task,
    cancel: Arc<AtomicBool>,
    report: impl Fn(SignInUpdate) -> bool + Send + 'static,
) -> io::Result<()> {
    thread::Builder::new()
        .name("eg-sign-in".into())
        .spawn(move || run(provider.as_ref(), task, &cancel, &report))
        .map(drop)
}

/// The step of a flow that waits for the user.
struct Poll {
    flow: SignInFlow,
    interval: Duration,
    deadline: Instant,
}

fn run(
    provider: &dyn Provider,
    task: Task,
    cancel: &AtomicBool,
    report: &dyn Fn(SignInUpdate) -> bool,
) {
    let mut step = match task {
        Task::Start { method, input } => provider.sign_in_start(method, input),
        Task::Continue { flow, code } => provider.sign_in_continue(&flow, code),
    };
    let mut poll: Option<Poll> = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        match step {
            Err(error) => {
                report(SignInUpdate::Failed(failure_text(&error, false)));
                return;
            }
            Ok(SignInStep::Done(token)) => {
                report(SignInUpdate::Done(token));
                return;
            }
            Ok(SignInStep::Expired) => {
                report(SignInUpdate::Expired);
                return;
            }
            Ok(SignInStep::SecondFactor { hint, flow }) => {
                report(SignInUpdate::SecondFactor { hint, flow });
                return;
            }
            Ok(SignInStep::ShowCode {
                code,
                url,
                interval,
                expires_in,
                flow,
            }) => {
                poll = Some(Poll {
                    flow,
                    interval,
                    deadline: Instant::now() + expires_in,
                });
                if !report(SignInUpdate::Code { code, url }) {
                    return;
                }
            }
            Ok(SignInStep::OpenBrowser {
                url,
                interval,
                flow,
            }) => {
                poll = Some(Poll {
                    flow,
                    interval,
                    deadline: Instant::now() + BROWSER_LIMIT,
                });
                if !report(SignInUpdate::Browser { url }) {
                    return;
                }
            }
            Ok(SignInStep::Pending) => {}
        }
        // `Pending`, or a step that started waiting: poll again after the interval.
        let Some(waiting) = &poll else {
            report(SignInUpdate::Failed(failure_text(
                &ProviderError::ApiChanged,
                false,
            )));
            return;
        };
        if !sleep(waiting.interval.max(MIN_INTERVAL), cancel) {
            return;
        }
        if Instant::now() >= waiting.deadline {
            report(SignInUpdate::Expired);
            return;
        }
        step = provider.sign_in_continue(&waiting.flow, None);
    }
}

/// Sleeps in short slices; false if the flow was cancelled meanwhile.
fn sleep(total: Duration, cancel: &AtomicBool) -> bool {
    let end = Instant::now() + total;
    loop {
        if cancel.load(Ordering::Acquire) {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        thread::sleep(left.min(CANCEL_SLICE));
    }
}

/// User-facing text for a failed sign-in. `token`: the user typed the token.
pub(crate) fn failure_text(error: &ProviderError, token: bool) -> String {
    match error {
        ProviderError::Auth if token => "Токен не подошёл",
        ProviderError::Network => "Нет связи с сервисом",
        ProviderError::RateLimited => "Слишком много попыток, повторите позже",
        _ => "Не получилось войти",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eagle_provider_sdk::testkit::{FAKE_SIGN_IN_TOKEN, FakeProvider};
    use std::sync::mpsc;

    fn collect(
        provider: Arc<FakeProvider>,
        task: Task,
        cancel: Arc<AtomicBool>,
    ) -> mpsc::Receiver<SignInUpdate> {
        let (tx, rx) = mpsc::channel();
        spawn(provider, task, cancel, move |update| {
            tx.send(update).is_ok()
        })
        .unwrap();
        rx
    }
    fn start(method: SignInMethod) -> Task {
        Task::Start {
            method,
            input: SignInInput::None,
        }
    }
    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn device_code_flow_shows_the_code_then_hands_over_the_token() {
        let provider = Arc::new(FakeProvider::new("fakea").signed_out().with_device_code(2));
        let rx = collect(
            provider.clone(),
            start(SignInMethod::DeviceCode),
            Arc::default(),
        );
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Code { .. }
        ));
        match rx.recv_timeout(WAIT).unwrap() {
            SignInUpdate::Done(token) => assert_eq!(token.expose(), FAKE_SIGN_IN_TOKEN),
            _ => panic!("expected Done"),
        }
        assert_eq!(provider.sign_in_polls(), 3);
    }

    #[test]
    fn expired_flow_reports_expiry() {
        let provider = Arc::new(
            FakeProvider::new("fakea")
                .signed_out()
                .with_device_code(0)
                .expiring(),
        );
        let rx = collect(provider, start(SignInMethod::DeviceCode), Arc::default());
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Code { .. }
        ));
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Expired
        ));
    }

    #[test]
    fn code_that_outlives_its_lifetime_expires_on_the_host_side() {
        let provider = Arc::new(
            FakeProvider::new("fakea")
                .signed_out()
                .with_device_code(usize::MAX)
                .with_sign_in_timing(Duration::from_millis(10), Duration::from_millis(60)),
        );
        let rx = collect(provider, start(SignInMethod::DeviceCode), Arc::default());
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Code { .. }
        ));
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Expired
        ));
    }

    #[test]
    fn cancelled_flow_stops_polling_and_reports_nothing_more() {
        let provider = Arc::new(
            FakeProvider::new("fakea")
                .signed_out()
                .with_device_code(usize::MAX),
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let rx = collect(
            provider.clone(),
            start(SignInMethod::DeviceCode),
            cancel.clone(),
        );
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Code { .. }
        ));
        cancel.store(true, Ordering::Release);
        // The thread ends and drops its sender.
        assert!(rx.recv_timeout(WAIT).is_err());
        let polls = provider.sign_in_polls();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(provider.sign_in_polls(), polls);
    }

    #[test]
    fn password_with_second_factor_waits_for_the_code() {
        let provider = Arc::new(FakeProvider::new("fakea").signed_out().with_password(true));
        let rx = collect(
            provider.clone(),
            Task::Start {
                method: SignInMethod::Password { login_hint: "" },
                input: SignInInput::Password {
                    login: "anna".into(),
                    password: Token::new("secret".into()),
                },
            },
            Arc::default(),
        );
        let SignInUpdate::SecondFactor { flow, .. } = rx.recv_timeout(WAIT).unwrap() else {
            panic!("expected SecondFactor");
        };
        let rx = collect(
            provider,
            Task::Continue {
                flow,
                code: Some(Token::new("000000".into())),
            },
            Arc::default(),
        );
        assert!(matches!(
            rx.recv_timeout(WAIT).unwrap(),
            SignInUpdate::Done(_)
        ));
    }

    #[test]
    fn wrong_password_fails_with_a_user_text() {
        let provider = Arc::new(FakeProvider::new("fakea").signed_out().with_password(false));
        let rx = collect(
            provider,
            Task::Start {
                method: SignInMethod::Password { login_hint: "" },
                input: SignInInput::Password {
                    login: "anna".into(),
                    password: Token::new("wrong".into()),
                },
            },
            Arc::default(),
        );
        match rx.recv_timeout(WAIT).unwrap() {
            SignInUpdate::Failed(text) => assert_eq!(text, "Не получилось войти"),
            _ => panic!("expected Failed"),
        }
    }

    #[test]
    fn beginning_a_flow_cancels_the_previous_one() {
        let mut flows = SignIns::default();
        let (first, first_cancel) = flows.begin("fakea");
        let (second, second_cancel) = flows.begin("fakea");
        assert!(first_cancel.load(Ordering::Acquire));
        assert!(!second_cancel.load(Ordering::Acquire));
        assert!(flows.current("fakea", first).is_none());
        assert!(flows.current("fakea", second).is_some());
        drop(flows);
        assert!(second_cancel.load(Ordering::Acquire));
    }
}
