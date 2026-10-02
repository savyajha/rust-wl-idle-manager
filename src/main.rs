mod config;
mod idle;
mod logind;
mod policy;
mod systemd;

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
use idle::IdleWatcher;
use logind::{LogindManagerProxy, idle_inhibited};
use policy::{Command, Input, Policy};
use systemd::SystemdManagerProxy;

/// The transient user unit the locker runs as.
const LOCKER_UNIT: &str = "rust-wl-locker.service";

/// How long sleep waits for the lock: under logind's default `InhibitDelayMaxSec` of 5 s.
const LOCK_WAIT: Duration = Duration::from_secs(4);

/// The error when a system bus stream ends.
const BUS_LOST: &str = "lost the system bus";

/// The path from exactly `--config <path>`, or `None` for any other arguments.
fn parse_args(mut args: impl Iterator<Item = OsString>) -> Option<PathBuf> {
    match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(path), None) if flag == "--config" => Some(path.into()),
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
        Input::LockedHint(true) => info!("session {session} locked"),
        Input::LockedHint(false) => info!("session {session} not locked"),
        Input::PrepareForSleep(true) => info!("preparing for sleep"),
        Input::PrepareForSleep(false) => info!("back from sleep"),
        Input::LockWaitTimedOut => info!("lock wait timed out; releasing the sleep inhibitor"),
    }
}

/// Runs policy commands, and keeps what they need between runs.
struct Runner {
    systemd: SystemdManagerProxy<'static>,
    logind: LogindManagerProxy<'static>,
    /// The locker command.
    locker: Vec<String>,
    /// How many commands have been spawned, for unique unit names.
    spawned: u64,
    /// The sleep delay inhibitor; dropping it releases it.
    inhibitor: Option<OwnedFd>,
    /// When the wait for the lock before sleep runs out, while waiting.
    lock_wait: Option<Instant>,
}

impl Runner {
    /// Run one policy command.
    async fn execute(&mut self, command: Command, watcher: &mut IdleWatcher) -> anyhow::Result<()> {
        match command {
            Command::StartLocker => {
                info!("starting the locker");
                let started = self
                    .systemd
                    .start_service(LOCKER_UNIT, "rust-wl-idle-manager: locker", &self.locker)
                    .await
                    .context("starting the locker")?;
                if !started {
                    info!("locker already running");
                }
            }
            // Never a stop: a session-lock client that dies without unlocking leaves the
            // session locked.
            Command::UnlockLocker => {
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
            // left over would release the next sleep's inhibitor early.
            Command::ReleaseSleepInhibitor => {
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
                watcher.rearm(&indices);
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
    let mut watcher = IdleWatcher::connect(&timeouts)?;
    let inhibitor = logind.sleep_inhibitor().await?;
    let mut runner = Runner {
        systemd,
        logind,
        locker: config.locker,
        spawned: 0,
        inhibitor: Some(inhibitor),
        lock_wait: None,
    };
    info!(
        "watching {} timeouts from {} in session {session_id}",
        timeouts.len(),
        config_path.display()
    );
    let mut policy = Policy::new(config.timeouts);
    loop {
        let input = tokio::select! {
            idle = watcher.next() => idle?,
            sleep = sleeps.next() => Input::PrepareForSleep(sleep.context(BUS_LOST)?.args()?.start),
            lock = locks.next() => lock.map(|_| Input::LockRequested).context(BUS_LOST)?,
            unlock = unlocks.next() => unlock.map(|_| Input::UnlockRequested).context(BUS_LOST)?,
            // Property values come from zbus's cache, which every PropertiesChanged updates.
            new = block_inhibited.next() => {
                Input::Inhibited(idle_inhibited(&new.context(BUS_LOST)?.get().await?))
            }
            new = active.next() => Input::SessionActive(new.context(BUS_LOST)?.get().await?),
            new = locked_hint.next() => Input::LockedHint(new.context(BUS_LOST)?.get().await?),
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
            if let Err(e) = runner.execute(command, &mut watcher).await {
                error!("{e:#}");
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Some(config_path) = parse_args(env::args_os().skip(1)) else {
        eprintln!("usage: rust-wl-idle-manager --config <path>");
        return ExitCode::from(2);
    };
    init_logging();

    match run(&config_path).await {
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
    fn only_config_with_a_path_parses() {
        let parse = |args: &[&str]| parse_args(args.iter().map(OsString::from));
        assert_eq!(parse(&["--config", "a"]), Some(PathBuf::from("a")));
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&["--config"]), None);
        assert_eq!(parse(&["--bogus", "x"]), None);
        assert_eq!(parse(&["--config", "a", "b"]), None);
    }
}
