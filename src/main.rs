mod config;
mod idle;

use std::env;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info};
use tracing_subscriber::{filter::LevelFilter, layer::SubscriberExt, util::SubscriberInitExt};

use config::Config;
use idle::{IdleEvent, IdleWatcher};

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

/// Load the config and log idle events until SIGTERM; an error ends the run.
async fn run(config_path: &Path) -> anyhow::Result<()> {
    let mut sigterm = signal(SignalKind::terminate()).context("listening for SIGTERM")?;
    let config = Config::load(config_path)?;
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
    loop {
        tokio::select! {
            idle = watcher.next() => match idle? {
                IdleEvent::Idled(i) => info!("idle after {} s (timeout {i})", timeouts[i].0.as_secs()),
                IdleEvent::Resumed(i) => info!("resumed (timeout {i})"),
            },
            _ = sigterm.recv() => return Ok(()),
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
