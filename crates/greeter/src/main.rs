//! rust-wl-greeter: a greetd greeter that looks like rust-wl-idle-manager's lock screen.
//!
//! greetd runs it on its own compositor (`rust-wl-greeter-compositor … -- rust-wl-greeter
//! --config <path>`). The user chooses who logs in (chooser.rs, users.rs), who is logged in
//! through greetd's prompts (flow.rs); it exits once greetd has the chosen session to start.
//! `--check <path>` validates a config.

mod battery;
mod chooser;
mod config;
mod flow;
mod focus;
mod greetd;
mod icons;
mod layouts;
mod login;
mod power;
mod sessions;
mod state;
mod theme;
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
use tokio::sync::mpsc;
use tokio::time::{self, Instant};
use tracing::{error, info, warn};
use tracing_subscriber::filter::LevelFilter;

use chooser::{Chooser, Screen};
use config::Config;
use flow::{Flow, Request, Show, Step};
use greetd::Greetd;
use login::{Login, Prompt};
use power::News;
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

/// Load the config at `path` as `greet` would, with its sessions and keyboard layouts.
fn check(path: &Path) -> anyhow::Result<()> {
    let config = Config::load(path)?;
    sessions::load(&config.sessions)?;
    for layout in &config.keyboard_layouts {
        Keyboard::with_layout(layout)?;
    }
    info!("{} is valid", path.display());
    Ok(())
}

fn greet(path: &Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting tokio")?;
    let greeted = runtime.block_on(greet_on(path));
    // Not waiting for a task blocked on a hung system bus: greetd starts the session only
    // once the greeter has exited.
    runtime.shutdown_background();
    greeted
}

async fn greet_on(path: &Path) -> anyhow::Result<()> {
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
    let layouts = layouts::new(&config.keyboard_layouts);
    let keyboard = match layouts.first() {
        Some(first) => Keyboard::with_layout(&first.name)?,
        None => Keyboard::default(),
    };
    let session_chip = sessions.len() > 1;
    let layout_chip = layouts.len() > 1;
    let chooser = Chooser::new(users, state.last_user(), session_chip, layout_chip);
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
        layouts,
        layout: 0,
        battery: None,
    };
    // The last user's theme, before the first frame. The name is set with the user.
    let theme = theme::load(state.last_user());
    let painter = Painter::given(
        config.look,
        theme.palette,
        &theme.font,
        theme.wallpaper,
        String::new(),
    );
    // What Pango draws with: the theme's font, or the one it falls back to.
    info!("font: {}", painter.font_family());
    let wayland = Wayland::connect(login, painter)?;
    // Alongside the rest: a slow bus, or a missing UPower or logind, holds up nothing.
    let (battery_sender, mut batteries) = mpsc::channel(1);
    tokio::spawn(battery::watch(battery_sender));
    let (news_sender, mut power_news) = mpsc::channel(1);
    let (power, actions) = mpsc::channel(1);
    tokio::spawn(power::serve(news_sender, actions));
    let mut greeter = Greeter {
        user: String::new(),
        wayland,
        greetd: Greetd::connect().await?,
        flow: Flow::new(),
        state,
        picked: State::default(),
        started: None,
        retry_at: None,
        power,
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
            Some(battery) = batteries.recv() => {
                greeter.wayland.login().battery = battery;
                Step::default()
            }
            Some(news) = power_news.recv() => {
                greeter.power_news(news);
                Step::default()
            }
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
    /// An unsaved `State`, used only as a map from each user to the session and layout in
    /// use when their login was given up in this greeter: theirs again if they are greeted
    /// again, before what they last logged in with.
    picked: State,
    started: Option<Started>,
    /// When to create the session again, after a failure.
    retry_at: Option<Instant>,
    /// To the power task: what to do.
    power: mpsc::Sender<power::Action>,
}

/// What a user chose before, as file and layout names.
#[derive(Debug, PartialEq)]
struct Earlier<'a> {
    session: Option<&'a str>,
    layout: Option<&'a str>,
}

/// What `user` chose before: what was in use when they left a login in this greeter
/// (`picked`), else what they last logged in with (`state`); each on its own.
fn earlier<'a>(picked: &'a State, state: &'a State, user: &str) -> Earlier<'a> {
    Earlier {
        session: picked.session(user).or(state.session(user)),
        layout: picked.layout(user).or(state.layout(user)),
    }
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
        // Leaving a login (Escape or Cancel always goes back from one).
        if matches!(event, Event::Cancel) && matches!(login.chooser.screen, Screen::Login(_)) {
            self.picked
                .set_session(&self.user, &login.sessions[login.chosen].file);
            if let Some(layout) = login.layouts.get(login.layout) {
                self.picked.set_layout(&self.user, &layout.name);
            }
        }
        if event.clears_message() {
            login.message = None;
        }
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
            Event::Power(i) => {
                if let Some(&action) = login.chooser.power.get(i) {
                    if let Some(question) = action.question() {
                        info!("dialog: {question}");
                        login.chooser.open_dialog(action);
                    } else {
                        self.act(action);
                    }
                }
                Step::default()
            }
            Event::Confirm => {
                if let Some(action) = login.chooser.close_dialog() {
                    self.act(action);
                }
                Step::default()
            }
            Event::Dismiss => {
                if login.chooser.close_dialog().is_some() {
                    info!("dialog cancelled");
                }
                Step::default()
            }
        }
    }

    /// Ask the power task to do `action`, unless it is busy with one.
    fn act(&mut self, action: power::Action) {
        if self.power.try_send(action).is_err() {
            warn!("power: busy, or no logind: not asking to {}", action.name());
        }
    }

    fn power_news(&mut self, news: News) {
        let login = self.wayland.login();
        match news {
            News::Buttons(actions) => login.chooser.power = actions,
            // The error is logged; D-Bus's text is not for the screen.
            News::Failed(action) => login.message = Some(format!("Could not {}", action.name())),
            News::Resumed => {
                login.resumed(Instant::now());
                info!("resumed; what was typed is wiped");
            }
        }
    }

    /// Show "Other…"'s field for a user name.
    fn ask_user_name(&mut self) {
        info!("asking for a user name");
        self.wayland.set_name(chooser::OTHER.to_owned());
    }

    /// Log `user` in, with the session and layout picked for them earlier in this greeter
    /// preselected, else those they last logged in with (see `earlier`).
    fn begin(&mut self, user: User) -> Step {
        let login = self.wayland.login();
        let earlier = earlier(&self.picked, &self.state, &user.name);
        login.chosen = sessions::preselect(&login.sessions, earlier.session);
        let layout = layouts::preselect(&login.layouts, earlier.layout, login.layout);
        if layout != login.layout {
            login.set_layout(layout);
            info!("layout {} (remembered)", login.layouts[login.layout].name);
        }
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
                if let Some(layout) = login.layouts.get(login.layout) {
                    self.state.set_layout(&started.user, &layout.name);
                }
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

    #[test]
    fn what_was_picked_before_going_back_wins_over_the_last_login() {
        let mut state = State::default();
        state.set_session("alice", "niri.desktop");
        state.set_layout("alice", "us");
        state.set_session("bob", "sway.desktop");
        let mut picked = State::default();
        picked.set_session("alice", "beta.desktop");
        let alice = earlier(&picked, &state, "alice");
        let want = Earlier {
            session: Some("beta.desktop"),
            layout: Some("us"),
        };
        assert_eq!(alice, want);
        picked.set_layout("alice", "de");
        assert_eq!(earlier(&picked, &state, "alice").layout, Some("de"));
        // Another user: their own.
        let bob = earlier(&picked, &state, "bob");
        assert_eq!((bob.session, bob.layout), (Some("sway.desktop"), None));
        let carol = earlier(&picked, &state, "carol");
        assert_eq!((carol.session, carol.layout), (None, None));
    }
}
