//! rust-wl-greeter: a greetd greeter that looks like rust-wl-idle-manager's lock screen.
//!
//! greetd runs it on its own compositor (`rust-wl-greeter-compositor … -- rust-wl-greeter
//! --config <path>`). The user chooses who logs in (chooser.rs, users.rs), who is logged in
//! through greetd's prompts (flow.rs); it exits once greetd has the chosen session to start.
//! `--check <path>` validates a config.

mod chooser;
mod config;
mod flow;
mod focus;
mod greetd;
mod login;
mod sessions;
mod state;
mod users;
mod wayland;

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use greetd_ipc as ipc;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{self, Instant};
use tracing::{error, info, warn};
use tracing_subscriber::filter::LevelFilter;

use chooser::{Chooser, Screen};
use config::Config;
use flow::{Flow, Request, Show, Step};
use greetd::Greetd;
use login::{Login, Prompt};
use state::State;
use ui::draw::Painter;
use ui::entry::Entry;
use ui::keyboard::Keyboard;
use users::User;
use wayland::{Event, Wayland};

/// The last user and each user's last session (see state.rs); the NixOS module creates its
/// directory.
const STATE: &str = "/var/lib/rust-wl-greeter/state";

/// tuigreet's last user, which seeds ours. greetd's NixOS module creates its directory, the
/// greeter user's, as tuigreet runs as that user too.
const TUIGREET_LAST_USER: &str = "/var/cache/tuigreet/lastuser";

/// How long after a failure with no answer the session is created again.
const RETRY_PAUSE: Duration = Duration::from_secs(2);

#[derive(Debug, PartialEq)]
enum Mode {
    Greet(PathBuf),
    Check(PathBuf),
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Option<Mode> {
    match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--config" => Some(Mode::Greet(path.into())),
        (Some(flag), Some(path), None) if flag == "--check" => Some(Mode::Check(path.into())),
        _ => None,
    }
}

fn main() -> ExitCode {
    // No core dumps, ptrace or /proc/PID/mem by the greeter user's other processes: the
    // greeter holds the typed password.
    let not_dumpable: libc::c_ulong = 0;
    // SAFETY: PR_SET_DUMPABLE takes only integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, not_dumpable) } != 0 {
        eprintln!("prctl(PR_SET_DUMPABLE, 0): {}", io::Error::last_os_error());
    }
    let Some(mode) = parse_args(env::args_os().skip(1)) else {
        eprintln!("usage: rust-wl-greeter --config <path> | --check <path>");
        return ExitCode::from(2);
    };
    tracing_subscriber::fmt()
        .with_max_level(LevelFilter::INFO)
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .with_writer(io::stderr)
        .init();
    let result = match mode {
        Mode::Greet(path) => greet(&path),
        Mode::Check(path) => check(&path),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Load the config at `path` as `greet` would, with its sessions and keyboard layout.
fn check(path: &Path) -> anyhow::Result<()> {
    let config = Config::load(path)?;
    sessions::load(&config.sessions)?;
    if let Some(layout) = &config.keyboard_layout {
        Keyboard::with_layout(layout)?;
    }
    info!("{} is valid", path.display());
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn greet(path: &Path) -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let config = Config::load(path)?;
    let sessions = sessions::load(&config.sessions)?;
    let users = users::load();
    let mut state = State::load(Path::new(STATE));
    let seeded = state.seed_last_user(Path::new(TUIGREET_LAST_USER), &users);
    if let Some(last) = state.last_user() {
        let how = if seeded {
            "from tuigreet"
        } else {
            "remembered"
        };
        info!("last user {last} ({how})");
    }
    let session_chip = sessions.len() > 1;
    let chooser = Chooser::new(users, state.last_user(), session_chip);
    let keyboard = match &config.keyboard_layout {
        Some(layout) => Keyboard::with_layout(layout)?,
        None => Keyboard::default(),
    };
    let login = Login {
        entry: Entry::new(),
        keyboard,
        prompt: Prompt {
            secret: true,
            text: String::new(),
        },
        message: None,
        sessions,
        chosen: 0,
        chooser,
    };
    // No theme yet: GTK's built-in colours and font. The name is set with the user.
    let painter = Painter::new(config.look, None, String::new());
    let mut greeter = Greeter {
        user: String::new(),
        wayland: Wayland::connect(login, painter)?,
        greetd: Greetd::connect().await?,
        flow: Flow::new(),
        state,
        started: None,
        retry_at: None,
    };
    match greeter.wayland.login().chooser.screen.clone() {
        Screen::Login(user) => {
            let step = greeter.begin(user);
            greeter.perform(step).await?;
        }
        Screen::UserName => greeter.ask_user_name(),
        Screen::Row => {}
    }
    loop {
        let retry_at = greeter.retry_at.unwrap_or_else(Instant::now);
        let step = tokio::select! {
            biased;
            event = greeter.wayland.next() => greeter.event(event?),
            response = greeter.greetd.receive() => greeter.flow.handle(response?),
            () = time::sleep_until(retry_at), if greeter.retry_at.is_some() => {
                greeter.retry_at = None;
                greeter.flow.retry()
            }
            _ = sigterm.recv() => {
                info!("SIGTERM: exiting");
                return Ok(());
            }
        };
        if greeter.perform(step).await? {
            return Ok(());
        }
    }
}

/// The login: what the user does on the screen, and the conversation with greetd.
struct Greeter {
    /// The user being logged in, whose session is created.
    user: String,
    wayland: Wayland,
    greetd: Greetd,
    flow: Flow,
    state: State,
    started: Option<Started>,
    /// When to create the session again, after a failure.
    retry_at: Option<Instant>,
}

/// What `start_session` was sent for.
struct Started {
    user: String,
    /// In `Login::sessions`.
    session: usize,
}

impl Greeter {
    fn event(&mut self, event: Event) -> Step {
        let login = self.wayland.login();
        login.message = None;
        match event {
            Event::Choose(i) => match login.chooser.choose(i) {
                Some(user) => self.begin(user),
                None => {
                    if login.chooser.screen == Screen::UserName {
                        self.ask_user_name();
                    }
                    Step::default()
                }
            },
            Event::Submit if login.chooser.screen == Screen::UserName => {
                let typed = String::from_utf8_lossy(login.entry.submission().bytes()).into_owned();
                login.entry.answered();
                match login.chooser.name_typed(&typed) {
                    Some(user) => self.begin(user),
                    None => Step::default(),
                }
            }
            Event::Submit => self.flow.submit(),
            Event::Cancel if login.chooser.back() => {
                if login.chooser.screen == Screen::UserName {
                    info!("cancelled; back to the user name");
                    self.ask_user_name();
                } else {
                    info!("cancelled; back to the users");
                }
                self.flow.leave()
            }
            // On the row, or at the user name with no row: nothing to go back to.
            Event::Cancel => Step::default(),
        }
    }

    /// Show "Other…"'s field for a user name.
    fn ask_user_name(&mut self) {
        info!("asking for a user name");
        self.wayland.set_name(chooser::OTHER.to_owned());
    }

    /// Log `user` in, with their last session preselected.
    fn begin(&mut self, user: User) -> Step {
        let login = self.wayland.login();
        login.chosen = sessions::preselect(&login.sessions, self.state.session(&user.name));
        login.prompt = Prompt {
            secret: true,
            text: String::new(),
        };
        // What is typed for "Other…" may be a password typed in the wrong place.
        if login.chooser.is_listed(&user) {
            info!("greeting {} ({})", user.name, user.full_name);
        } else {
            info!("greeting a user who is not listed");
        }
        self.wayland.set_name(user.full_name);
        self.user = user.name;
        self.flow.begin()
    }

    /// Show and send what `step` says; returns whether the session is started.
    async fn perform(&mut self, step: Step) -> anyhow::Result<bool> {
        let login = self.wayland.login();
        match step.show {
            None => {}
            Some(Show::Prompt { secret, text }) => {
                info!("prompt: {}", if secret { "secret" } else { "visible" });
                login.prompt = Prompt { secret, text };
                // An answer sent before was taken; one submitted ahead answers this one.
                if !matches!(step.request, Some(Request::Answer { .. })) {
                    login.entry.answered();
                }
            }
            // PAM's text; greetd's own descriptions are never shown or logged.
            Some(Show::Message(text)) => {
                info!("message from PAM");
                login.message = Some(text);
            }
            Some(Show::WrongPassword) => {
                info!("wrong password");
                login.entry.checked(/* ok */ false, Instant::now());
                login.message = Some("Wrong password".into());
            }
            Some(Show::Failed) => {
                warn!("greetd could not log in; starting over");
                // An answer submitted ahead never reached PAM: not a failure to count.
                login.entry.answered();
                login.entry.wipe();
                login.message = Some("Could not log in; try again".into());
            }
            Some(Show::Started) => {
                let started = self.started.take();
                let started = started.expect("greetd starts only the session sent");
                let session = &login.sessions[started.session];
                self.state.set_last_user(&started.user);
                self.state.set_session(&started.user, &session.file);
                if let Err(e) = self.state.save(Path::new(STATE)) {
                    warn!("not remembering the session: {e:#}");
                }
                info!("session {} started; exiting", session.name);
                return Ok(true);
            }
        }
        if step.discard {
            login.entry.answered();
            login.entry.wipe();
        }
        if step.wrong {
            info!("wrong password, for a login given up");
            login.entry.count_failure(Instant::now());
        }
        // A failure with no answer (a locked account) would come back at once, every time.
        if step.pause {
            self.retry_at = Some(Instant::now() + RETRY_PAUSE);
        }
        if let Some(request) = step.request {
            self.send(request).await?;
        }
        Ok(false)
    }

    async fn send(&mut self, request: Request) -> anyhow::Result<()> {
        let login = self.wayland.login();
        let request = match request {
            Request::Answer { secret } => {
                // Wiped when dropped.
                let answer = login.entry.submission();
                return self.greetd.answer(answer.bytes(), secret).await;
            }
            Request::Create => ipc::Request::CreateSession {
                username: self.user.clone(),
            },
            Request::Acknowledge => ipc::Request::PostAuthMessageResponse { response: None },
            Request::Start => {
                self.started = Some(Started {
                    user: self.user.clone(),
                    session: login.chosen,
                });
                let session = &login.sessions[login.chosen];
                info!("starting session {} ({})", session.name, session.file);
                ipc::Request::StartSession {
                    cmd: session.command(),
                    env: session.env(),
                }
            }
            Request::Cancel => ipc::Request::CancelSession,
        };
        self.greetd.request(&request).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_config_or_check_with_a_path_parses() {
        let parse = |args: &[&str]| parse_args(args.iter().map(OsString::from));
        assert_eq!(parse(&["--config", "a"]), Some(Mode::Greet("a".into())));
        assert_eq!(parse(&["--check", "a"]), Some(Mode::Check("a".into())));
        assert_eq!(parse(&["--config"]), None);
        assert_eq!(parse(&["--check", "a", "b"]), None);
        assert_eq!(parse(&[]), None);
    }
}
