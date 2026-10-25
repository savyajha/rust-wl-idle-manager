//! The login's conversation with greetd, as a state machine fed greetd's responses and the
//! user's answers. No I/O: each step says what to send and what to show.
//!
//! `create_session(user)` starts it (`begin`). Each prompt greetd sends waits for the user's answer;
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
//!
//! The user can go back to choose who logs in (`leave`), and choose again (`begin`). The
//! session greetd has is then cancelled: at once at a prompt, else as soon as greetd has
//! responded, as a request is answered before the next is sent. Then the flow waits for the
//! user, or creates the session of the user chosen since; at once, even after a failure,
//! since it is for a user the user chose again.

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
    /// The answer to a login the user gave up was wrong: count it, showing nothing.
    pub wrong: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Waiting {
    /// For the user to choose who logs in: greetd has no session.
    Idle,
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

/// What the user wants instead of the session greetd has.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Instead {
    /// Nothing: it is the one wanted.
    Nothing,
    /// No session, until `begin`.
    Idle,
    /// The session of the user chosen since.
    New,
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
    instead: Instead,
}

impl Flow {
    /// The flow, waiting for `begin`.
    pub fn new() -> Self {
        Self {
            waiting: Waiting::Idle,
            secret_field: true,
            pending: false,
            failed: false,
            instead: Instead::Nothing,
        }
    }

    /// Log the user just chosen in: create their session, once greetd's is cancelled if it
    /// has one. The field is for a secret again.
    pub fn begin(&mut self) -> Step {
        self.secret_field = true;
        let step = self.give_up(Instead::New);
        if self.waiting == Waiting::Idle {
            return self.send(Request::Create, None);
        }
        step
    }

    /// The user went back to choose who logs in: cancel greetd's session if it has one,
    /// then wait for `begin`. What is typed is discarded.
    pub fn leave(&mut self) -> Step {
        Step {
            discard: true,
            ..self.give_up(Instead::Idle)
        }
    }

    /// Give greetd's session up for `instead`: cancel it at once at a prompt, else once
    /// greetd has responded. An answer submitted ahead is dropped.
    fn give_up(&mut self, instead: Instead) -> Step {
        self.pending = false;
        match self.waiting {
            Waiting::Answer { .. } => {
                self.instead = instead;
                self.send(Request::Cancel, None)
            }
            Waiting::Response(_) => {
                self.instead = instead;
                Step::default()
            }
            // The failed session is cancelled already.
            Waiting::Paused => {
                self.waiting = Waiting::Idle;
                Step::default()
            }
            Waiting::Idle | Waiting::Started => Step::default(),
        }
    }

    /// The user submitted an answer: it answers greetd's prompt, or the next one.
    pub fn submit(&mut self) -> Step {
        match self.waiting {
            Waiting::Answer { secret } => self.send(Request::Answer { secret }, None),
            // No one to answer for.
            Waiting::Idle => Step::default(),
            _ => {
                self.pending = true;
                Step::default()
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
            // Too late to give it up.
            (Request::Start, Response::Success) => {
                self.waiting = Waiting::Started;
                Step {
                    show: Some(Show::Started),
                    ..Step::default()
                }
            }
            // Whatever the answer, start over, after a pause if it failed; or wait for the
            // user, or create the session of the user they chose.
            (Request::Cancel, _) => {
                let failed = mem::take(&mut self.failed);
                match mem::replace(&mut self.instead, Instead::Nothing) {
                    Instead::Idle => {
                        self.waiting = Waiting::Idle;
                        Step::default()
                    }
                    Instead::Nothing if failed => {
                        self.waiting = Waiting::Paused;
                        Step {
                            pause: true,
                            ..Step::default()
                        }
                    }
                    Instead::Nothing | Instead::New => self.send(Request::Create, None),
                }
            }
            // Whatever greetd says of a session the user gave up; but a wrong answer still
            // counts, or giving up would be a way round the cooldown.
            (_, response) if self.instead != Instead::Nothing => {
                let answered = matches!(sent, Request::Answer { .. });
                let wrong = answered
                    && matches!(
                        response,
                        Response::Error {
                            error_type: ErrorType::AuthError,
                            ..
                        }
                    );
                Step {
                    wrong,
                    ..self.send(Request::Cancel, None)
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

    /// A flow that has begun: the user is chosen.
    fn begun() -> (Flow, Step) {
        let mut flow = Flow::new();
        let step = flow.begin();
        (flow, step)
    }

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
            let (flow, step) = begun();
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
        let password = auth_message(AuthMessageType::Secret, "Password: ");
        let step = login.greetd(create, password).await;
        assert_eq!(step.show, prompt(true, "Password: "));
        login.user_answers("pa\"ss\\wörd").await;
        let step = login
            .greetd(answer(Some("pa\"ss\\wörd")), Response::Success)
            .await;
        assert_eq!(step.request, Some(Request::Start));
        let step = login.greetd(start, Response::Success).await;
        assert_eq!(step.show, Some(Show::Started));
        // The flow is over: nothing more is answered or sent.
        assert_eq!(login.flow.leave().request, None);
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
    async fn leaving_at_a_prompt_cancels_and_beginning_creates_again() {
        let (mut login, _) = Login::new().await;
        let password = || auth_message(AuthMessageType::Secret, "Password: ");
        login.greetd(create, password()).await;
        let step = login.flow.leave();
        assert_eq!(step.request, Some(Request::Cancel));
        login.perform(&step).await;
        // Not a failure, and nothing more until the user chooses again.
        assert_eq!(
            login.greetd(cancel, Response::Success).await,
            Step::default()
        );
        let step = login.flow.begin();
        assert_eq!(step.request, Some(Request::Create));
        login.perform(&step).await;
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
        let (mut flow, _) = begun();
        assert_eq!(flow.submit(), Step::default());
        let step = flow.handle(secret_prompt());
        assert_eq!(step.request, Some(Request::Answer { secret: true }));
        assert_eq!(step.show, prompt(true, "Password: "));
        assert!(!step.discard);
    }

    #[test]
    fn a_secret_typed_ahead_is_discarded_for_a_visible_prompt() {
        let (mut flow, _) = begun();
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
        let (mut flow, _) = begun();
        // Typed, not submitted, for the secret field: the visible prompt discards it.
        assert!(
            flow.handle(auth_message(AuthMessageType::Visible, "Code: "))
                .discard
        );
        flow.submit();
        // And text typed at the visible prompt does not stay for the secret one.
        assert!(flow.handle(secret_prompt()).discard);
        flow.leave();
        flow.handle(Response::Success);
        flow.begin();
        // The same kind again keeps it.
        assert!(!flow.handle(secret_prompt()).discard);
    }

    #[test]
    fn a_failure_drops_an_answer_submitted_ahead() {
        let (mut flow, _) = begun();
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
        let (mut flow, _) = begun();
        flow.submit();
        // It never reached PAM: a failure, not a wrong password (which would count).
        let step = flow.handle(error(ErrorType::AuthError));
        assert_eq!(step.show, Some(Show::Failed));
    }

    #[test]
    fn nothing_is_created_before_begin() {
        let mut flow = Flow::new();
        assert_eq!(flow.submit(), Step::default());
        assert_eq!(flow.retry(), Step::default());
        assert_eq!(flow.leave().request, None);
        assert_eq!(flow.handle(Response::Success), Step::default());
        assert_eq!(flow.begin().request, Some(Request::Create));
    }

    #[test]
    fn leaving_at_a_prompt_cancels_and_waits_for_begin() {
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        let step = flow.leave();
        assert_eq!(step.request, Some(Request::Cancel));
        // What was typed for that user is wiped.
        assert!(step.discard);
        // Cancelled: not created again.
        assert_eq!(flow.handle(Response::Success), Step::default());
        assert_eq!(flow.retry(), Step::default());
        assert_eq!(flow.begin().request, Some(Request::Create));
        assert_eq!(
            flow.handle(secret_prompt()).show,
            prompt(true, "Password: ")
        );
    }

    #[test]
    fn leaving_while_greetd_works_cancels_once_it_has_responded() {
        let (mut flow, _) = begun();
        // create_session is in flight: nothing can be sent before its response.
        assert_eq!(flow.leave().request, None);
        let step = flow.handle(secret_prompt());
        // That user's prompt is not shown, but cancelled.
        assert_eq!((step.request, step.show), (Some(Request::Cancel), None));
        assert_eq!(flow.handle(Response::Success), Step::default());
        // The same after an answer, whatever greetd says of it: a wrong password is not
        // shown or counted for a user who is gone.
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        flow.submit();
        flow.leave();
        let step = flow.handle(error(ErrorType::AuthError));
        assert_eq!((step.request, step.show), (Some(Request::Cancel), None));
        assert!(!flow.handle(Response::Success).pause);
        // Or a success: the session is not started.
        let (mut flow, _) = begun();
        flow.leave();
        assert_eq!(
            flow.handle(Response::Success).request,
            Some(Request::Cancel)
        );
    }

    #[test]
    fn choosing_another_user_cancels_then_creates_their_session() {
        // Gone back, then chosen again before greetd answered the cancel.
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        flow.leave();
        assert_eq!(flow.begin(), Step::default());
        assert_eq!(
            flow.handle(Response::Success).request,
            Some(Request::Create)
        );
        // Before greetd answered create_session: that session is cancelled first.
        let (mut flow, _) = begun();
        flow.leave();
        flow.begin();
        assert_eq!(flow.handle(secret_prompt()).request, Some(Request::Cancel));
        assert_eq!(
            flow.handle(Response::Success).request,
            Some(Request::Create)
        );
        assert_eq!(
            flow.handle(secret_prompt()).show,
            prompt(true, "Password: ")
        );
    }

    #[test]
    fn leaving_during_the_pause_drops_it() {
        let (mut flow, _) = begun();
        flow.handle(error(ErrorType::Error));
        assert!(flow.handle(Response::Success).pause);
        // The failed session is gone already: nothing to send, and the pause is over.
        assert_eq!(flow.leave().request, None);
        assert_eq!(flow.retry(), Step::default());
        // Chosen again, at once.
        assert_eq!(flow.begin().request, Some(Request::Create));
        // A failure while leaving does not pause either.
        let (mut flow, _) = begun();
        flow.handle(error(ErrorType::Error));
        flow.leave();
        assert_eq!(flow.handle(Response::Success), Step::default());
        assert_eq!(flow.begin().request, Some(Request::Create));
    }

    #[test]
    fn an_answer_typed_ahead_is_for_the_user_it_was_typed_for() {
        let (mut flow, _) = begun();
        flow.submit();
        // Gone back: it is discarded, and answers no one's prompt.
        assert!(flow.leave().discard);
        flow.handle(secret_prompt());
        flow.handle(Response::Success);
        flow.begin();
        assert_eq!(flow.handle(secret_prompt()).request, None);
        // Typed ahead for the user chosen next, it answers their prompt.
        flow.leave();
        flow.handle(Response::Success);
        flow.begin();
        flow.submit();
        let step = flow.handle(secret_prompt());
        assert_eq!(step.request, Some(Request::Answer { secret: true }));
    }

    #[test]
    fn the_next_user_starts_with_a_secret_field() {
        let (mut flow, _) = begun();
        flow.handle(auth_message(AuthMessageType::Visible, "Code: "));
        flow.leave();
        flow.handle(Response::Success);
        flow.begin();
        // A secret typed ahead for them is kept for their password.
        flow.submit();
        let step = flow.handle(secret_prompt());
        assert!(!step.discard);
        assert_eq!(step.request, Some(Request::Answer { secret: true }));
    }

    #[test]
    fn a_session_being_started_is_not_given_up() {
        let (mut flow, _) = begun();
        flow.handle(Response::Success);
        flow.leave();
        assert_eq!(flow.handle(Response::Success).show, Some(Show::Started));
        assert_eq!(flow.begin(), Step::default());
        assert_eq!(flow.leave().request, None);
    }

    #[test]
    fn a_wrong_answer_to_a_login_given_up_still_counts() {
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        flow.submit();
        flow.leave();
        let step = flow.handle(error(ErrorType::AuthError));
        assert!(step.wrong && step.show.is_none());
        // Not for a failure with no answer, or another error.
        let (mut flow, _) = begun();
        flow.leave();
        assert!(!flow.handle(error(ErrorType::AuthError)).wrong);
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        flow.submit();
        flow.leave();
        assert!(!flow.handle(error(ErrorType::Error)).wrong);
        // Nor for a session that is the one wanted (`show` says that one).
        let (mut flow, _) = begun();
        flow.handle(secret_prompt());
        flow.submit();
        assert!(!flow.handle(error(ErrorType::AuthError)).wrong);
    }
}
