mod auth;
mod config;
mod logind;
mod password;
mod policy;
mod systemd;
mod wayland;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};
use std::time::Duration;

use anyhow::Context;
use futures_lite::StreamExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::{self, Instant};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::Connection;

use config::Config;
use logind::{LogindManagerProxy, idle_inhibited};
use policy::{Command, Input, Policy};
use systemd::SystemdManagerProxy;
use wayland::Wayland;

/// The transient user unit the locker runs as.
const LOCKER_UNIT: &str = "rust-wl-locker.service";

/// How long sleep waits for the lock: under logind's default `InhibitDelayMaxSec` of 5 s.
const LOCK_WAIT: Duration = Duration::from_secs(4);

/// The error when a system bus stream ends.
const BUS_LOST: &str = "lost the system bus";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Mode {
    /// `--config <path>`: run the daemon.
    Daemon(PathBuf),
    /// `--auth`: check the password on stdin (the lock screen's helper).
    Auth,
}

/// The mode from exactly `--config <path>` or `--auth`, or `None` for any other arguments.
fn parse_args(mut args: impl Iterator<Item = OsString>) -> Option<Mode> {
    match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--config" => Some(Mode::Daemon(path.into())),
        (Some(flag), None, None) if flag == "--auth" => Some(Mode::Auth),
        _ => None,
    }
}

/// Log to journald, or to stderr if journald is unavailable.
fn init_logging() {
    let journald = tracing_journald::layer()
        .inspect_err(|e| eprintln!("journald unavailable ({e}); falling back to stderr"))
        .ok();
    let stderr = journald.is_none().then(|| {
        tracing_subscriber::fmt::layer()
            .with_ansi(io::stderr().is_terminal())
            .with_writer(io::stderr)
    });
    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(journald)
        .with(stderr)
        .init();
}

/// Log `input` in words; `session` is our session's ID.
fn log_input(input: Input, session: &str, timeouts: &[(Duration, bool)]) {
    match input {
        Input::Idled(i) => info!("idle after {} s (timeout {i})", timeouts[i].0.as_secs()),
        Input::Resumed(i) => info!("resumed (timeout {i})"),
        Input::Inhibited(true) => info!("logind idle inhibitor held"),
        Input::Inhibited(false) => info!("no logind idle inhibitor"),
        Input::SessionActive(true) => info!("session {session} active"),
        Input::SessionActive(false) => info!("session {session} inactive"),
        Input::LockRequested => info!("lock requested for session {session}"),
        Input::UnlockRequested => info!("unlock requested for session {session}"),
        Input::Locked(true) => info!("session {session} locked"),
        Input::Locked(false) => info!("session {session} not locked"),
        Input::LidClosed(true) => info!("lid closed"),
        Input::LidClosed(false) => info!("lid open"),
        Input::PrepareForSleep(true) => info!("preparing for sleep"),
        Input::PrepareForSleep(false) => info!("back from sleep"),
        Input::LockWaitTimedOut => info!("lock wait timed out; releasing the sleep inhibitor"),
        Input::PasswordEntered => info!("checking the password"),
        Input::Authenticated(true) => info!("password accepted"),
        Input::Authenticated(false) => info!("password rejected"),
    }
}

/// Runs policy commands, and keeps what they need between runs.
struct Runner {
    systemd: SystemdManagerProxy<'static>,
    logind: LogindManagerProxy<'static>,
    /// The locker command; without one, the built-in lock screen locks.
    locker: Option<Vec<String>>,
    /// How many commands have been spawned, for unique unit names.
    spawned: u64,
    /// The sleep delay inhibitor; dropping it releases it.
    inhibitor: Option<OwnedFd>,
    /// When the wait for the lock before sleep runs out, while waiting.
    lock_wait: Option<Instant>,
    /// The password check under way.
    attempt: Option<auth::Attempt>,
}

impl Runner {
    /// Run one policy command.
    async fn execute(&mut self, command: Command, wayland: &mut Wayland) -> anyhow::Result<()> {
        match command {
            Command::Lock if let Some(locker) = &self.locker => {
                info!("starting the locker");
                let started = self
                    .systemd
                    .start_service(LOCKER_UNIT, "rust-wl-idle-manager: locker", locker)
                    .await
                    .context("starting the locker")?;
                if !started {
                    info!("locker already running");
                }
            }
            // Logged once requested, so that logging adds nothing to the lock's latency.
            Command::Lock => {
                wayland.lock()?;
                info!("lock requested from the compositor");
            }
            // Never a stop: a session-lock client that dies without unlocking leaves the
            // session locked.
            Command::Unlock if self.locker.is_some() => {
                info!("unlocking the locker");
                let usr1 = SignalKind::user_defined1().as_raw_value();
                let sent = self
                    .systemd
                    .signal_main(LOCKER_UNIT, usr1)
                    .await
                    .context("unlocking the locker")?;
                if !sent {
                    info!("no locker running to unlock");
                }
            }
            Command::Unlock => {
                info!("unlocking the lock screen");
                wayland.unlock();
            }
            Command::Spawn(argv) => {
                let line = argv.join(" ");
                info!("spawning {line}");
                let unit = format!(
                    "rust-wl-idle-spawn-{}-{}.service",
                    process::id(),
                    self.spawned
                );
                self.spawned += 1;
                let description = format!("rust-wl-idle-manager: {line}");
                let started = self
                    .systemd
                    .start_service(&unit, &description, &argv)
                    .await
                    .with_context(|| format!("spawning {line}"))?;
                if !started {
                    error!("spawn unit {unit} already exists; not running {line}");
                }
            }
            // A new attempt replaces (and kills) any left from a lock that has ended.
            Command::Authenticate => {
                let entry = wayland.entry_mut();
                // Dropping the submission wipes the password, on every path.
                let password = entry.submission();
                // Nothing is submitted any more after a reset, such as an unlock.
                if password.bytes().is_empty() {
                    return Ok(());
                }
                let started = auth::start(password.bytes()).await;
                drop(password);
                match started {
                    Ok(attempt) => self.attempt = Some(attempt),
                    Err(e) => {
                        entry.checked(false, std::time::Instant::now());
                        return Err(e).context("starting the authentication helper");
                    }
                }
            }
            Command::Suspend => {
                info!("suspending");
                self.logind.suspend(false).await.context("suspending")?;
            }
            Command::SuspendThenHibernate => {
                info!("suspending, then hibernating");
                self.logind
                    .suspend_then_hibernate(false)
                    .await
                    .context("suspending, then hibernating")?;
            }
            Command::Hibernate => {
                info!("hibernating");
                self.logind.hibernate(false).await.context("hibernating")?;
            }
            Command::WaitForLock => {
                info!("waiting up to {} s for the lock", LOCK_WAIT.as_secs());
                self.lock_wait = Some(Instant::now() + LOCK_WAIT);
            }
            // Clearing the deadline matters: tokio's clock stops during sleep, so a timer
            // left over would release the next sleep's inhibitor early. A password typed
            // on the lock screen is wiped before sleep, which may end in hibernation.
            Command::ReleaseSleepInhibitor => {
                wayland.entry_mut().wipe();
                self.lock_wait = None;
                if self.inhibitor.take().is_some() {
                    info!("sleep inhibitor released");
                }
            }
            // Assigning drops (releases) any inhibitor still held, once the new one is held.
            Command::TakeSleepInhibitor => {
                self.inhibitor = Some(self.logind.sleep_inhibitor().await?);
                info!("sleep inhibitor taken");
            }
            Command::Rearm(indices) => {
                info!("rearming timeouts {indices:?}");
                wayland.rearm(&indices);
            }
        }
        Ok(())
    }
}

/// Load the config, then run the commands that idle and logind events lead to until
/// SIGTERM; an error ends the run, but a failed command is only logged.
async fn run(config_path: &Path) -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let config = Config::load(config_path)?;
    let session = Connection::session()
        .await
        .context("connecting to the session bus")?;
    let systemd = SystemdManagerProxy::new(&session).await?;
    let system = Connection::system()
        .await
        .context("connecting to the system bus")?;
    let logind = LogindManagerProxy::new(&system).await?;
    let (session_id, logind_session) = logind.our_session().await?;
    // Subscribed before the sleep inhibitor is taken, so no PrepareForSleep is missed.
    let mut sleeps = logind.receive_prepare_for_sleep().await?;
    let mut locks = logind_session.receive_lock().await?;
    let mut unlocks = logind_session.receive_unlock().await?;
    // A property stream yields the current value first, then the value on each change.
    let mut block_inhibited = logind.receive_block_inhibited_changed().await;
    let mut active = logind_session.receive_active_changed().await;
    let mut locked_hint = logind_session.receive_locked_hint_changed().await;
    let mut lid_closed = logind.receive_lid_closed_changed().await;
    // A failed read here is fatal; zbus would otherwise leave those streams silent forever.
    logind
        .block_inhibited()
        .await
        .context("reading logind's properties")?;
    logind_session
        .active()
        .await
        .context("reading the session's properties")?;
    let timeouts: Vec<_> = config
        .timeouts
        .iter()
        .map(|timeout| (timeout.after, timeout.ignore_inhibit))
        .collect();
    let lock_screen = config.locker.is_none();
    let mut wayland = Wayland::connect(&timeouts, lock_screen)?;
    let inhibitor = logind.sleep_inhibitor().await?;
    let mut runner = Runner {
        systemd,
        logind,
        locker: config.locker,
        spawned: 0,
        inhibitor: Some(inhibitor),
        lock_wait: None,
        attempt: None,
    };
    info!(
        "watching {} timeouts from {} in session {session_id}",
        timeouts.len(),
        config_path.display()
    );
    // A previous run that died while locked left the session locked; the compositor lets
    // a new lock replace the dead one.
    if lock_screen && logind_session.locked_hint().await? {
        info!("session {session_id} is still locked; locking again");
        wayland.lock()?;
    }
    let mut policy = Policy::new(config.timeouts);
    loop {
        let input = tokio::select! {
            // Wayland first: after an unlock, its `Locked(false)` is already queued, and
            // must reach the policy before a lock request that would otherwise be dropped.
            biased;
            wayland_input = wayland.next() => wayland_input?,
            sleep = sleeps.next() => Input::PrepareForSleep(sleep.context(BUS_LOST)?.args()?.start),
            lock = locks.next() => lock.map(|_| Input::LockRequested).context(BUS_LOST)?,
            unlock = unlocks.next() => unlock.map(|_| Input::UnlockRequested).context(BUS_LOST)?,
            // Property values come from zbus's cache, which every PropertiesChanged updates.
            new = block_inhibited.next() => {
                Input::Inhibited(idle_inhibited(&new.context(BUS_LOST)?.get().await?))
            }
            new = active.next() => Input::SessionActive(new.context(BUS_LOST)?.get().await?),
            // With the built-in lock screen, the compositor's `locked` says it instead.
            new = locked_hint.next(), if !lock_screen => {
                Input::Locked(new.context(BUS_LOST)?.get().await?)
            }
            // Like logind, which ignores the lid while docked (HandleLidSwitchDocked).
            // A failed read counts as not docked: exiting now could let the machine
            // sleep unlocked, since logind may be about to suspend.
            new = lid_closed.next() => {
                let closed = new.context(BUS_LOST)?.get().await?;
                let docked = closed
                    && runner.logind.docked().await.unwrap_or_else(|e| {
                        error!("reading logind's Docked: {e}; locking anyway");
                        false
                    });
                Input::LidClosed(closed && !docked)
            }
            ok = auth::finished(&mut runner.attempt) => {
                // An answer for a lock that has ended since is dropped.
                if !wayland.entry_mut().checked(ok, std::time::Instant::now()) {
                    continue;
                }
                Input::Authenticated(ok)
            }
            () = time::sleep_until(runner.lock_wait.unwrap_or_else(Instant::now)),
                if runner.lock_wait.is_some() =>
            {
                runner.lock_wait = None;
                Input::LockWaitTimedOut
            }
            _ = sigterm.recv() => return Ok(()),
        };
        log_input(input, &session_id, &timeouts);
        for command in policy.handle(input) {
            if let Err(e) = runner.execute(command, &mut wayland).await {
                error!("{e:#}");
            }
        }
    }
}

fn main() -> ExitCode {
    // No core dumps, and no ptrace or /proc/PID/mem by other processes of the user: the
    // daemon holds the typed password, the helper a copy. Both take this path.
    // SAFETY: PR_SET_DUMPABLE takes only integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) } != 0 {
        eprintln!("prctl(PR_SET_DUMPABLE, 0): {}", io::Error::last_os_error());
    }
    let mode = parse_args(env::args_os().skip(1));
    init_logging();
    match mode {
        Some(Mode::Daemon(config_path)) => daemon(&config_path),
        // The helper needs no runtime.
        Some(Mode::Auth) => auth::helper(),
        None => {
            eprintln!("usage: rust-wl-idle-manager --config <path>");
            ExitCode::from(2)
        }
    }
}

/// Run the daemon until SIGTERM or an error.
#[tokio::main(flavor = "current_thread")]
async fn daemon(config_path: &Path) -> ExitCode {
    match run(config_path).await {
        Ok(()) => {
            info!("SIGTERM received, exiting");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_config_with_a_path_or_auth_parses() {
        let parse = |args: &[&str]| parse_args(args.iter().map(OsString::from));
        assert_eq!(
            parse(&["--config", "a"]),
            Some(Mode::Daemon(PathBuf::from("a")))
        );
        assert_eq!(parse(&["--auth"]), Some(Mode::Auth));
        assert_eq!(parse(&["--auth", "x"]), None);
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["--config"]), None);
        assert_eq!(parse(&["--bogus", "x"]), None);
        assert_eq!(parse(&["--config", "a", "b"]), None);
    }
}
