//! The login's conversation with greetd, as a state machine fed greetd's responses and the
//! user's answers. No I/O: each step says what to send and what to show.
//!
//! `create_session(user)` starts it. Each prompt greetd sends waits for the user's answer;
//! an info or error message is shown and acknowledged with an empty answer; `success` is
//! answered with `start_session`, whose `success` ends it. A failure (`auth_error`, which
//! is how greetd reports any PAM failure, or any other error) cancels the session, and it
//! is created again: at once after an answer, which was wrong; after a pause otherwise,
//! as it would fail again at once. greetd's description of an error is never shown or
//! logged: it may hold what was typed.
//!
//! An answer submitted before greetd's prompt has come answers it, but only if the prompt
//! is of the kind the field showed: what was typed in a secret field is never sent as a
//! visible answer, nor shown as one.

use std::mem;

use greetd_ipc::{AuthMessageType, ErrorType, Response};
use tracing::warn;

/// A request for greetd.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Request {
    Create,
    /// The user's answer to a prompt.
    Answer {
        secret: bool,
    },
    /// An empty answer, to an info or error message.
    Acknowledge,
    /// Start the chosen session.
    Start,
    Cancel,
}

/// What the login screen is to show.
#[derive(Clone, Debug, PartialEq)]
pub enum Show {
    /// greetd's prompt, to be answered secretly (a password) or visibly.
    Prompt {
        secret: bool,
        text: String,
    },
    /// PAM's info or error message.
    Message(String),
    WrongPassword,
    /// greetd failed in some other way.
    Failed,
    /// The session is started: the greeter's work is done.
    Started,
}

#[derive(Debug, Default, PartialEq)]
pub struct Step {
    pub request: Option<Request>,
    pub show: Option<Show>,
    /// Wipe what is typed: it was typed for a prompt of the other kind, or its submission
    /// was given up.
    pub discard: bool,
    /// The session failed and is cancelled: call `retry` after a pause.
    pub pause: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Waiting {
    /// greetd's response to what was sent.
    Response(Request),
    /// The user's answer to greetd's prompt.
    Answer {
        secret: bool,
    },
    /// For `retry`, after a failure.
    Paused,
    Started,
}

#[derive(Debug)]
pub struct Flow {
    waiting: Waiting,
    /// Whether the field is for a secret: the last prompt's kind, a secret before the first.
    secret_field: bool,
    /// An answer was submitted before greetd's prompt came.
    pending: bool,
    /// The session being cancelled failed.
    failed: bool,
}

impl Flow {
    /// The flow, and its first step: creating the session.
    pub fn start() -> (Self, Step) {
        let mut flow = Self {
            waiting: Waiting::Started,
            secret_field: true,
            pending: false,
            failed: false,
        };
        let step = flow.send(Request::Create, None);
        (flow, step)
    }

    /// Whether greetd's prompt waits for the user's answer.
    pub fn at_prompt(&self) -> bool {
        matches!(self.waiting, Waiting::Answer { .. })
    }

    /// The user submitted an answer: it answers greetd's prompt, or the next one.
    pub fn submit(&mut self) -> Step {
        match self.waiting {
            Waiting::Answer { secret } => self.send(Request::Answer { secret }, None),
            _ => {
                self.pending = true;
                Step::default()
            }
        }
    }

    /// The user gave up (Escape): at a prompt, cancel and start over; otherwise, discard
    /// an answer submitted ahead.
    pub fn cancel(&mut self) -> Step {
        if self.at_prompt() {
            self.send(Request::Cancel, None)
        } else {
            Step {
                discard: mem::take(&mut self.pending),
                ..Step::default()
            }
        }
    }

    /// Create the session again, after the pause a failure asked for.
    pub fn retry(&mut self) -> Step {
        if self.waiting == Waiting::Paused {
            self.send(Request::Create, None)
        } else {
            Step::default()
        }
    }

    pub fn handle(&mut self, response: Response) -> Step {
        let Waiting::Response(sent) = self.waiting else {
            warn!("greetd sent a response to nothing; ignoring it");
            return Step::default();
        };
        match (sent, response) {
            // Whatever the answer, start over, after a pause if it failed.
            (Request::Cancel, _) if mem::take(&mut self.failed) => {
                self.waiting = Waiting::Paused;
                Step {
                    pause: true,
                    ..Step::default()
                }
            }
            (Request::Cancel, _) => self.send(Request::Create, None),
            (Request::Start, Response::Success) => {
                self.waiting = Waiting::Started;
                Step {
                    show: Some(Show::Started),
                    ..Step::default()
                }
            }
            (_, Response::Success) => self.send(Request::Start, None),
            (
                _,
                Response::AuthMessage {
                    auth_message_type,
                    auth_message: text,
                },
            ) => match auth_message_type {
                AuthMessageType::Secret | AuthMessageType::Visible => {
                    let secret = matches!(auth_message_type, AuthMessageType::Secret);
                    self.prompt(secret, text)
                }
                AuthMessageType::Info | AuthMessageType::Error => {
                    self.send(Request::Acknowledge, Some(Show::Message(text)))
                }
            },
            (_, Response::Error { error_type, .. }) => {
                // Only an answer can be wrong. A failure with none (a locked account, say)
                // would come back at once: the session is created again after a pause.
                let answered = matches!(sent, Request::Answer { .. });
                let show = match error_type {
                    ErrorType::AuthError if answered => Show::WrongPassword,
                    _ => Show::Failed,
                };
                self.pending = false;
                self.failed = !answered;
                self.send(Request::Cancel, Some(show))
            }
        }
    }

    /// greetd's prompt: answered at once by an answer submitted ahead, unless the field
    /// was of the other kind, whose text is discarded.
    fn prompt(&mut self, secret: bool, text: String) -> Step {
        let show = Some(Show::Prompt { secret, text });
        let same_kind = secret == self.secret_field;
        self.secret_field = secret;
        self.waiting = Waiting::Answer { secret };
        if !same_kind {
            self.pending = false;
            return Step {
                show,
                discard: true,
                ..Step::default()
            };
        }
        if mem::take(&mut self.pending) {
            return self.send(Request::Answer { secret }, show);
        }
        Step {
            show,
            ..Step::default()
        }
    }

    fn send(&mut self, request: Request, show: Option<Show>) -> Step {
        self.waiting = Waiting::Response(request);
        Step {
            request: Some(request),
            show,
            ..Step::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::greetd::Greetd;
    use greetd_ipc as ipc;
    use greetd_ipc::codec::TokioCodec;
    use tokio::net::UnixStream;

    fn auth_message(kind: AuthMessageType, text: &str) -> Response {
        Response::AuthMessage {
            auth_message_type: kind,
            auth_message: text.into(),
        }
    }

    fn error(error_type: ErrorType) -> Response {
        Response::Error {
            error_type,
            description: "pam: AUTH_ERR, typed: hunter2".into(),
        }
    }

    /// The flow's client against a fake greetd on a socket pair: the user's answers go
    /// through `Greetd` as the greeter sends them, and greetd's side is scripted.
    struct Login {
        flow: Flow,
        greetd: Greetd,
        theirs: UnixStream,
        /// What the user types, sent as the answer to the current prompt.
        typed: &'static str,
    }

    impl Login {
        async fn new() -> (Self, Step) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let (flow, step) = Flow::start();
            let mut login = Self {
                flow,
                greetd: Greetd::new(ours),
                theirs,
                typed: "",
            };
            login.perform(&step).await;
            (login, step)
        }

        /// Send what `step` says, as the greeter does.
        async fn perform(&mut self, step: &Step) {
            let request = match step.request {
                None => return,
                Some(Request::Answer { secret }) => {
                    return self
                        .greetd
                        .answer(self.typed.as_bytes(), secret)
                        .await
                        .unwrap();
                }
                Some(Request::Create) => ipc::Request::CreateSession {
                    username: "alice".into(),
                },
                Some(Request::Acknowledge) => {
                    ipc::Request::PostAuthMessageResponse { response: None }
                }
                Some(Request::Start) => ipc::Request::StartSession {
                    cmd: vec!["'niri-session'".into()],
                    env: vec!["XDG_SESSION_TYPE=wayland".into()],
                },
                Some(Request::Cancel) => ipc::Request::CancelSession,
            };
            self.greetd.request(&request).await.unwrap();
        }

        /// greetd receives a request that `check` accepts and responds with `response`; the
        /// greeter handles that and performs the step, which is returned.
        async fn greetd(
            &mut self,
            check: impl Fn(&ipc::Request) -> bool,
            response: Response,
        ) -> Step {
            let request = ipc::Request::read_from(&mut self.theirs).await.unwrap();
            assert!(check(&request), "unexpected {request:?}");
            response.write_to(&mut self.theirs).await.unwrap();
            let response = self.greetd.receive().await.unwrap();
            let step = self.flow.handle(response);
            self.perform(&step).await;
            step
        }

        async fn user_answers(&mut self, typed: &'static str) {
            self.typed = typed;
            let step = self.flow.submit();
            assert!(matches!(step.request, Some(Request::Answer { .. })));
            self.perform(&step).await;
        }

        /// The pause after a failure is over.
        async fn retry(&mut self) {
            let step = self.flow.retry();
            assert_eq!(step.request, Some(Request::Create));
            self.perform(&step).await;
        }
    }

    fn create(request: &ipc::Request) -> bool {
        matches!(request, ipc::Request::CreateSession { username } if username == "alice")
    }

    fn cancel(request: &ipc::Request) -> bool {
        matches!(request, ipc::Request::CancelSession)
    }

    fn answer(want: Option<&'static str>) -> impl Fn(&ipc::Request) -> bool {
        move |request| matches!(request, ipc::Request::PostAuthMessageResponse { response } if response.as_deref() == want)
    }

    fn start(request: &ipc::Request) -> bool {
        matches!(request, ipc::Request::StartSession { cmd, env }
            if cmd == &["'niri-session'"] && env == &["XDG_SESSION_TYPE=wayland"])
    }

    fn prompt(secret: bool, text: &str) -> Option<Show> {
        Some(Show::Prompt {
            secret,
            text: text.into(),
        })
    }

    #[tokio::test]
    async fn a_password_then_the_session() {
        let (mut login, step) = Login::new().await;
        assert_eq!(step.request, Some(Request::Create));
        assert!(!login.flow.at_prompt());
        let password = auth_message(AuthMessageType::Secret, "Password: ");
        let step = login.greetd(create, password).await;
        assert_eq!(step.show, prompt(true, "Password: "));
        assert!(login.flow.at_prompt());
        login.user_answers("pa\"ss\\wörd").await;
        assert!(!login.flow.at_prompt());
        let step = login
            .greetd(answer(Some("pa\"ss\\wörd")), Response::Success)
            .await;
        assert_eq!(step.request, Some(Request::Start));
        let step = login.greetd(start, Response::Success).await;
        assert_eq!(step.show, Some(Show::Started));
        // The flow is over: nothing more is answered or sent.
        assert_eq!(login.flow.cancel(), Step::default());
        assert_eq!(login.flow.retry(), Step::default());
        assert_eq!(login.flow.submit(), Step::default());
        assert_eq!(login.flow.handle(Response::Success), Step::default());
    }

    #[tokio::test]
    async fn a_wrong_password_cancels_and_starts_over() {
        let (mut login, _) = Login::new().await;
        let password = || auth_message(AuthMessageType::Secret, "Password: ");
        login.greetd(create, password()).await;
        login.user_answers("wrong").await;
        let step = login
            .greetd(answer(Some("wrong")), error(ErrorType::AuthError))
            .await;
        // greetd's description is not passed on.
        assert_eq!(step.show, Some(Show::WrongPassword));
        assert!(!format!("{step:?}").contains("hunter2"));
        assert_eq!(step.request, Some(Request::Cancel));
        // Created again at once: the answer was wrong (the entry's cooldown slows guessing).
        let step = login.greetd(cancel, Response::Success).await;
        assert_eq!(step.request, Some(Request::Create));
        assert!(!step.pause);
        let step = login.greetd(create, password()).await;
        assert_eq!(step.show, prompt(true, "Password: "));
        login.user_answers("right").await;
        login.greetd(answer(Some("right")), Response::Success).await;
    }

    #[tokio::test]
    async fn a_visible_prompt_messages_and_a_password() {
        let (mut login, _) = Login::new().await;
        let info = auth_message(AuthMessageType::Info, "Welcome");
        let step = login.greetd(create, info).await;
        assert_eq!(step.show, Some(Show::Message("Welcome".into())));
        assert_eq!(step.request, Some(Request::Acknowledge));
        let code = auth_message(AuthMessageType::Visible, "Verification code: ");
        let step = login.greetd(answer(None), code).await;
        assert_eq!(step.show, prompt(false, "Verification code: "));
        login.user_answers("123456").await;
        let error_message = auth_message(AuthMessageType::Error, "Token expired soon");
        let step = login.greetd(answer(Some("123456")), error_message).await;
        assert_eq!(step.show, Some(Show::Message("Token expired soon".into())));
        let password = auth_message(AuthMessageType::Secret, "Password: ");
        let step = login.greetd(answer(None), password).await;
        assert_eq!(step.show, prompt(true, "Password: "));
        login.user_answers("pw").await;
        login.greetd(answer(Some("pw")), Response::Success).await;
        login.greetd(start, Response::Success).await;
    }

    #[tokio::test]
    async fn escape_at_a_prompt_cancels_and_starts_over() {
        let (mut login, _) = Login::new().await;
        // Not before greetd's prompt.
        assert_eq!(login.flow.cancel(), Step::default());
        let password = || auth_message(AuthMessageType::Secret, "Password: ");
        login.greetd(create, password()).await;
        let step = login.flow.cancel();
        assert_eq!(step.request, Some(Request::Cancel));
        login.perform(&step).await;
        // Not a failure: created again at once.
        let step = login.greetd(cancel, Response::Success).await;
        assert_eq!(step.request, Some(Request::Create));
        let step = login.greetd(create, password()).await;
        assert_eq!(step.show, prompt(true, "Password: "));
    }

    #[tokio::test]
    async fn other_errors_show_a_failure_and_start_over() {
        let (mut login, _) = Login::new().await;
        let step = login.greetd(create, error(ErrorType::Error)).await;
        assert_eq!(step.show, Some(Show::Failed));
        // A failed cancel starts over too, after the pause.
        let step = login.greetd(cancel, error(ErrorType::Error)).await;
        assert!(step.pause && step.request.is_none());
        login.retry().await;
        let password = auth_message(AuthMessageType::Secret, "Password: ");
        login.greetd(create, password).await;
        login.user_answers("pw").await;
        login.greetd(answer(Some("pw")), Response::Success).await;
        let step = login.greetd(start, error(ErrorType::Error)).await;
        assert_eq!(step.show, Some(Show::Failed));
        assert_eq!(step.request, Some(Request::Cancel));
        assert!(login.greetd(cancel, Response::Success).await.pause);
    }

    #[tokio::test]
    async fn a_failure_without_an_answer_is_not_a_wrong_password_and_pauses() {
        let (mut login, _) = Login::new().await;
        // pam_faillock or pam_nologin, before any prompt.
        let step = login.greetd(create, error(ErrorType::AuthError)).await;
        assert_eq!(step.show, Some(Show::Failed));
        assert!(login.greetd(cancel, Response::Success).await.pause);
        login.retry().await;
        // After an acknowledged message, too.
        let info = auth_message(AuthMessageType::Info, "Account locked");
        login.greetd(create, info).await;
        let step = login
            .greetd(answer(None), error(ErrorType::AuthError))
            .await;
        assert_eq!(step.show, Some(Show::Failed));
        assert!(login.greetd(cancel, Response::Success).await.pause);
    }

    #[tokio::test]
    async fn a_user_without_a_prompt_gets_the_session() {
        let (mut login, _) = Login::new().await;
        let step = login.greetd(create, Response::Success).await;
        assert_eq!(step.request, Some(Request::Start));
        let step = login.greetd(start, Response::Success).await;
        assert_eq!(step.show, Some(Show::Started));
    }

    fn secret_prompt() -> Response {
        auth_message(AuthMessageType::Secret, "Password: ")
    }

    #[test]
    fn a_secret_typed_ahead_answers_a_secret_prompt() {
        let (mut flow, _) = Flow::start();
        assert_eq!(flow.submit(), Step::default());
        let step = flow.handle(secret_prompt());
        assert_eq!(step.request, Some(Request::Answer { secret: true }));
        assert_eq!(step.show, prompt(true, "Password: "));
        assert!(!step.discard);
    }

    #[test]
    fn a_secret_typed_ahead_is_discarded_for_a_visible_prompt() {
        let (mut flow, _) = Flow::start();
        flow.submit();
        let step = flow.handle(auth_message(AuthMessageType::Visible, "Code: "));
        assert_eq!(step.request, None);
        assert!(step.discard);
        // What is typed for the visible prompt answers it.
        let step = flow.submit();
        assert_eq!(step.request, Some(Request::Answer { secret: false }));
        assert!(!step.discard);
    }

    #[test]
    fn a_prompt_of_the_other_kind_discards_what_is_typed() {
        let (mut flow, _) = Flow::start();
        // Typed, not submitted, for the secret field: the visible prompt discards it.
        assert!(
            flow.handle(auth_message(AuthMessageType::Visible, "Code: "))
                .discard
        );
        flow.submit();
        // And text typed at the visible prompt does not stay for the secret one.
        assert!(flow.handle(secret_prompt()).discard);
        flow.cancel();
        flow.handle(Response::Success);
        // The same kind again keeps it.
        assert!(!flow.handle(secret_prompt()).discard);
    }

    #[test]
    fn a_failure_drops_an_answer_submitted_ahead() {
        let (mut flow, _) = Flow::start();
        flow.submit();
        let step = flow.handle(error(ErrorType::Error));
        assert_eq!(step.request, Some(Request::Cancel));
        assert!(flow.handle(Response::Success).pause);
        assert_eq!(flow.retry().request, Some(Request::Create));
        // No empty secret is sent for the stale submission.
        let step = flow.handle(secret_prompt());
        assert_eq!(step.request, None);
    }

    #[test]
    fn an_answer_submitted_ahead_is_not_wrong_when_greetd_fails_first() {
        let (mut flow, _) = Flow::start();
        flow.submit();
        // It never reached PAM: a failure, not a wrong password (which would count).
        let step = flow.handle(error(ErrorType::AuthError));
        assert_eq!(step.show, Some(Show::Failed));
    }

    #[test]
    fn escape_discards_an_answer_submitted_ahead() {
        let (mut flow, _) = Flow::start();
        flow.submit();
        let step = flow.cancel();
        assert!(step.discard && step.request.is_none());
        assert_eq!(flow.handle(secret_prompt()).request, None);
    }
}
