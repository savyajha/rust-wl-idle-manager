mod config;
mod idle;
mod logind;
mod policy;
mod systemd;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{self, ExitCode};

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};
use zbus::Connection;

use config::Config;
use idle::{IdleEvent, IdleWatcher};
use logind::LogindManagerProxy;
use policy::{Command, Input, Policy};
use systemd::SystemdManagerProxy;

/// The transient user unit the locker runs as.
const LOCKER_UNIT: &str = "rust-wl-locker.service";

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

/// Run one policy command. The commands that need logind inputs are only logged so far.
async fn execute(
    command: Command,
    systemd: &SystemdManagerProxy<'_>,
    logind: &LogindManagerProxy<'_>,
    locker: &[String],
    spawned: &mut u64,
) -> anyhow::Result<()> {
    match command {
        Command::StartLocker => {
            info!("starting the locker");
            let started = systemd
                .start_service(LOCKER_UNIT, "rust-wl-idle-manager: locker", locker)
                .await
                .context("starting the locker")?;
            if !started {
                info!("locker already running");
            }
        }
        Command::Spawn(argv) => {
            let line = argv.join(" ");
            info!("spawning {line}");
            let unit = format!("rust-wl-idle-spawn-{}-{spawned}.service", process::id());
            *spawned += 1;
            let description = format!("rust-wl-idle-manager: {line}");
            let started = systemd
                .start_service(&unit, &description, &argv)
                .await
                .with_context(|| format!("spawning {line}"))?;
            if !started {
                error!("spawn unit {unit} already exists; not running {line}");
            }
        }
        Command::Suspend => {
            info!("suspending");
            logind.suspend(false).await.context("suspending")?;
        }
        Command::SuspendThenHibernate => {
            info!("suspending, then hibernating");
            logind
                .suspend_then_hibernate(false)
                .await
                .context("suspending, then hibernating")?;
        }
        Command::Hibernate => {
            info!("hibernating");
            logind.hibernate(false).await.context("hibernating")?;
        }
        command => info!("command (not run yet): {command:?}"),
    }
    Ok(())
}

/// Load the config, then run the commands that idle events lead to until SIGTERM;
/// an error ends the run, but a failed command is only logged.
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
    let timeouts: Vec<_> = config
        .timeouts
        .iter()
        .map(|timeout| (timeout.after, timeout.ignore_inhibit))
        .collect();
    let mut watcher = IdleWatcher::connect(&timeouts)?;
    info!(
        "watching {} timeouts from {}",
        timeouts.len(),
        config_path.display()
    );
    let mut policy = Policy::new(config.timeouts);
    let mut spawned = 0;
    loop {
        let input = tokio::select! {
            idle = watcher.next() => match idle? {
                IdleEvent::Idled(i) => {
                    info!("idle after {} s (timeout {i})", timeouts[i].0.as_secs());
                    Input::Idled(i)
                }
                IdleEvent::Resumed(i) => {
                    info!("resumed (timeout {i})");
                    Input::Resumed(i)
                }
            },
            _ = sigterm.recv() => return Ok(()),
        };
        for command in policy.handle(input) {
            if let Err(e) = execute(command, &systemd, &logind, &config.locker, &mut spawned).await
            {
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
