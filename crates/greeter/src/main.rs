//! rust-wl-greeter: a greetd greeter that looks like rust-wl-idle-manager's lock screen.
//!
//! greetd runs it on its own compositor (`rust-wl-greeter-compositor … -- rust-wl-greeter
//! --config <path>`). It logs the configured user in through greetd's prompts (flow.rs) and
//! exits once greetd has the chosen session to start. `--check <path>` validates a config.

mod config;
mod flow;
mod greetd;
mod login;
mod sessions;
mod state;
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

use config::Config;
use flow::{Flow, Request, Show, Step};
use greetd::Greetd;
use login::{Login, Prompt};
use state::State;
use ui::draw::Painter;
use ui::entry::Entry;
use ui::keyboard::Keyboard;
use wayland::{Event, Wayland};

/// Each user's last session (see state.rs); the NixOS module creates its directory.
const STATE: &str = "/var/lib/rust-wl-greeter/state";

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
    let state = State::load(Path::new(STATE));
    let remembered = state.session(&config.user);
    let remembered = remembered.and_then(|file| sessions.iter().position(|s| s.file == file));
    let chosen = remembered.unwrap_or(0);
    let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
    let how = if remembered.is_some() {
        "remembered"
    } else {
        "first"
    };
    let chosen_name = &sessions[chosen].name;
    info!(
        "sessions: {}; {chosen_name} chosen ({how})",
        names.join(", ")
    );
    let name = full_name(&config.user);
    info!("greeting {} ({name})", config.user);
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
        chosen,
    };
    // No theme yet: GTK's built-in colours and font.
    let wayland = Wayland::connect(login, Painter::new(config.look, None, name))?;
    let (flow, step) = Flow::start();
    let mut greeter = Greeter {
        user: config.user,
        wayland,
        greetd: Greetd::connect().await?,
        flow,
        state,
        started: None,
        retry_at: None,
    };
    greeter.perform(step).await?;
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
    user: String,
    wayland: Wayland,
    greetd: Greetd,
    flow: Flow,
    state: State,
    /// The session `start_session` was sent for, in `Login::sessions`.
    started: Option<usize>,
    /// When to create the session again, after a failure.
    retry_at: Option<Instant>,
}

impl Greeter {
    fn event(&mut self, event: Event) -> Step {
        let login = self.wayland.login();
        login.message = None;
        match event {
            Event::Submit => self.flow.submit(),
            Event::Cancel => {
                if self.flow.at_prompt() {
                    info!("cancelled; starting over");
                    login.entry.wipe();
                }
                self.flow.cancel()
            }
        }
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
                let started = self.started.expect("greetd starts only the session sent");
                let session = &login.sessions[started];
                self.state.set_session(&self.user, &session.file);
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
                self.started = Some(login.chosen);
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

/// `user`'s full name: the passwd database's GECOS field up to its first comma, or without
/// one, the user name.
fn full_name(user: &str) -> String {
    let entry = nix::unistd::User::from_name(user).ok().flatten();
    let gecos = entry.map(|entry| entry.gecos.to_string_lossy().into_owned());
    let gecos = gecos.unwrap_or_default();
    let name = gecos.split(',').next().unwrap_or_default().trim();
    if name.is_empty() {
        user.to_owned()
    } else {
        name.to_owned()
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

    #[test]
    fn a_user_without_a_full_name_is_called_by_their_user_name() {
        assert_eq!(full_name("no-such-user-here"), "no-such-user-here");
        // root exists everywhere, with or without a GECOS name.
        assert!(!full_name("root").is_empty());
    }
}
