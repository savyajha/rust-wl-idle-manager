//! rust-wl-greeter-compositor: the login screen's own Wayland compositor.
//!
//! greetd runs `rust-wl-greeter-compositor [options] -- <greeter> [args...]`. It takes the seat,
//! opens one GPU, gives every connected connector an output, starts the greeter as its only
//! client once its socket is up, and exits when the greeter exits (with its status) or when
//! greetd sends SIGTERM to start the session. It draws with Smithay's Pixman renderer into DRM
//! dumb buffers: no GBM, EGL or GL, so no Mesa is loaded at boot.

mod cursor;
mod drm;
mod input;
mod shell;

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use calloop::signals::{Signal, Signals};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::{Seat, SeatState, keyboard::XkbConfig};
use smithay::output::Output;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::shell::xdg::{PopupSurface, XdgShellState};
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::viewporter::ViewporterState;
use tracing::{debug, error, info, warn};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Key repeat sent to the client: niri's defaults.
const REPEAT_DELAY_MS: i32 = 600;
const REPEAT_RATE: i32 = 25;

/// How long the greeter gets to exit after our SIGTERM before it is killed.
const CHILD_GRACE: Duration = Duration::from_millis(500);

/// Input opens after the client's first frame, or after this long if no frame comes, so
/// Ctrl+Alt+Fn still works with a greeter that never draws.
const INPUT_DEADLINE: Duration = Duration::from_secs(1);

const USAGE: &str = "usage: rust-wl-greeter-compositor [--drm-device PATH] \
                     [--scale CONNECTOR=SCALE]... [--log-timing] -- GREETER [ARGS...]";

#[derive(Debug, Default, PartialEq)]
struct Args {
    drm_device: Option<PathBuf>,
    scales: Vec<(String, f64)>,
    log_timing: bool,
    greeter: Vec<OsString>,
}

fn parse_args(mut args: impl Iterator<Item = OsString>) -> Result<Args, String> {
    let mut parsed = Args::default();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--") => {
                parsed.greeter = args.collect();
                break;
            }
            Some("--log-timing") => parsed.log_timing = true,
            Some("--drm-device") => {
                parsed.drm_device = Some(args.next().ok_or("--drm-device needs a path")?.into());
            }
            Some("--scale") => {
                let value = args.next().ok_or("--scale needs CONNECTOR=SCALE")?;
                let value = value.to_str().ok_or("--scale: not UTF-8")?;
                let (name, scale) = value
                    .split_once('=')
                    .ok_or_else(|| format!("--scale {value}: expected CONNECTOR=SCALE"))?;
                let scale = scale
                    .parse::<f64>()
                    .ok()
                    .filter(|s| (0.5..=4.0).contains(s))
                    .ok_or_else(|| format!("--scale {value}: not a number from 0.5 to 4"))?;
                parsed.scales.push((name.to_owned(), scale));
            }
            _ => return Err(format!("unexpected argument {arg:?}")),
        }
    }
    if parsed.greeter.is_empty() {
        return Err("no greeter command after --".into());
    }
    Ok(parsed)
}

pub struct ClientState {
    compositor: CompositorClientState,
}

impl smithay::reexports::wayland_server::backend::ClientData for ClientState {}

/// A toplevel and the output it is fullscreen on (none while there is no output).
pub struct Toplevel {
    window: Window,
    output: Option<Output>,
    /// Moved off an unplugged output, and not asked for one since.
    moved: bool,
}

pub struct State {
    dh: DisplayHandle,
    handle: LoopHandle<'static, State>,
    signal: LoopSignal,
    started: Instant,
    log_timing: bool,
    session: LibSeatSession,

    compositor: CompositorState,
    shm: ShmState,
    xdg_shell: XdgShellState,
    seat_state: SeatState<State>,
    seat: Seat<State>,
    _globals: (
        FractionalScaleManagerState,
        ViewporterState,
        PresentationState,
    ),

    space: Space<Window>,
    popups: PopupManager,
    toplevels: Vec<Toplevel>,
    /// The toplevel last clicked: it has the keyboard when no popup grab does.
    focus: Option<Window>,
    /// The popups holding a grab, bottom to top; the topmost has the keyboard.
    grab: Vec<PopupSurface>,

    renderer: PixmanRenderer,
    gpu: drm::Gpu,
    scales: Vec<(String, f64)>,
    pointer: Point<f64, Logical>,
    /// The cursor shows from the first pointer motion on.
    cursor_visible: bool,
    libinput: Option<Libinput>,
    /// Keys whose press was not sent to the client, so neither is their release.
    intercepted: Vec<smithay::input::keyboard::Keycode>,
    /// The same for the button that dismissed popups.
    swallowed_button: Option<u32>,
    first_frame: bool,

    child: Option<Child>,
    child_status: Option<ExitStatus>,
    /// When SIGTERM or SIGINT arrived.
    terminated: Option<Instant>,
}

impl State {
    /// Start-up times: at info with `--log-timing`, else at debug.
    fn timing(&self, what: &str) {
        let ms = self.started.elapsed().as_secs_f64() * 1e3;
        if self.log_timing {
            info!("+{ms:.1} ms: {what}");
        } else {
            debug!("+{ms:.1} ms: {what}");
        }
    }
}

fn init_logging() {
    // Smithay logs every connector and mode at info; only its warnings are kept.
    let filter = Targets::new()
        .with_default(LevelFilter::INFO)
        .with_target("smithay", LevelFilter::WARN);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(false)
                .with_writer(std::io::stderr),
        )
        .init();
}

fn main() {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("rust-wl-greeter-compositor: {e}\n{USAGE}");
            std::process::exit(2);
        }
    };
    init_logging();
    let Err(e) = run(args);
    error!("{e:#}");
    std::process::exit(1);
}

/// Runs until the greeter exits or a signal comes, and exits; returns only start-up errors.
fn run(args: Args) -> anyhow::Result<Infallible> {
    let started = Instant::now();
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let handle = event_loop.handle();

    // First, so a SIGTERM during start-up is handled too. This blocks the signals in this
    // thread; the greeter gets an empty mask again (see `spawn`).
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT])?;
    handle
        .insert_source(signals, |event, _, state| {
            info!("{:?}: exiting", event.signal());
            state.terminated.get_or_insert_with(Instant::now);
            state.signal.stop();
        })
        .map_err(|e| e.error)?;

    // The seat, through logind or seatd; fails without one (e.g. over ssh).
    let (session, notifier) =
        LibSeatSession::new().map_err(|e| anyhow!("no seat session (libseat): {e:?}"))?;
    let (gpu, drm_events) = drm::Gpu::open(session.clone(), args.drm_device)?;

    let display: Display<State> = Display::new()?;
    let dh = display.handle();
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, session.seat());
    // XkbConfig's empty names leave the choice to xkbcommon, which reads XKB_DEFAULT_*.
    seat.add_keyboard(XkbConfig::default(), REPEAT_DELAY_MS, REPEAT_RATE)?;
    seat.add_pointer();

    let mut state = State {
        handle: handle.clone(),
        signal: event_loop.get_signal(),
        started,
        log_timing: args.log_timing,
        session,
        compositor: CompositorState::new_v6::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        xdg_shell: XdgShellState::new::<State>(&dh),
        seat_state,
        seat,
        _globals: (
            FractionalScaleManagerState::new::<State>(&dh),
            ViewporterState::new::<State>(&dh),
            PresentationState::new::<State>(&dh, rustix::time::ClockId::Monotonic as u32),
        ),
        dh: dh.clone(),
        space: Space::default(),
        popups: PopupManager::default(),
        toplevels: Vec::new(),
        focus: None,
        grab: Vec::new(),
        renderer: PixmanRenderer::new()?,
        gpu,
        scales: args.scales,
        pointer: Point::default(),
        cursor_visible: false,
        libinput: None,
        intercepted: Vec::new(),
        swallowed_button: None,
        first_frame: false,
        child: None,
        child_status: None,
        terminated: None,
    };
    state.timing(&format!("opened {}", state.gpu.path.display()));
    state.watch_hotplug();
    state.scan_connectors(false);
    // The primary output's modeset (blocking) before the greeter starts; the others' after its
    // first frame (see `render`), which goes to the primary output.
    if !state.gpu.heads.is_empty() {
        state.render(0);
    }

    handle
        .insert_source(drm_events, |event, metadata, state| match event {
            smithay::backend::drm::DrmEvent::VBlank(crtc) => state.vblank(crtc, metadata),
            smithay::backend::drm::DrmEvent::Error(e) => warn!("DRM: {e}"),
        })
        .map_err(|e| e.error)?;
    handle
        .insert_source(notifier, |event, _, state| state.session_event(event))
        .map_err(|e| e.error)?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                // SAFETY: the display is only dispatched, not dropped or replaced, so the fd
                // the loop polls stays open.
                unsafe { display.get_mut().dispatch_clients(state)? };
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| e.error)?;
    let socket = ListeningSocketSource::new_auto()?;
    let socket_name = socket.socket_name().to_os_string();
    handle
        .insert_source(socket, |stream, _, state| {
            let client = ClientState {
                compositor: CompositorClientState::default(),
            };
            if let Err(e) = state.dh.insert_client(stream, Arc::new(client)) {
                warn!("adding a client: {e}");
            }
        })
        .map_err(|e| e.error)?;

    let child = spawn(&args.greeter, &socket_name)?;
    state.timing(&format!(
        "started the greeter on {}",
        socket_name.to_string_lossy()
    ));
    let pidfd = pidfd(&child)?;
    state.child = Some(child);
    handle
        .insert_source(
            Generic::new(pidfd, Interest::READ, Mode::Level),
            |_, _, state| {
                if let Some(mut child) = state.child.take() {
                    state.child_status = Some(child.wait()?);
                }
                state.signal.stop();
                Ok(PostAction::Remove)
            },
        )
        .map_err(|e| e.error)?;
    handle
        .insert_source(Timer::from_duration(INPUT_DEADLINE), |_, _, state| {
            state.start_input();
            TimeoutAction::Drop
        })
        .map_err(|e| e.error)?;

    event_loop.run(None, &mut state, |state| {
        state.space.refresh();
        state.popups.cleanup();
        let _ = state.dh.flush_clients();
    })?;

    let code = match state.child_status {
        Some(status) => {
            info!("greeter exited: {status}");
            status.code().unwrap_or(1)
        }
        None => {
            if let Some(child) = state.child.take() {
                stop_child(child);
            }
            0
        }
    };
    // The session's compositor needs the GPU: drop DRM master if we hold it ourselves (under
    // logind, logind holds it for us and drops it when it releases our devices, below).
    state.gpu.drm.pause();
    if let Some(t) = state.terminated.filter(|_| state.log_timing) {
        let ms = t.elapsed().as_secs_f64() * 1e3;
        info!("exiting {ms:.1} ms after the signal");
    }
    // Without dropping the rest: the kernel closes our fds, and logind releases the session's
    // devices when our bus connection closes. Dropping libinput and libseat first costs a
    // D-Bus round trip per device (30 ms in the VM test).
    std::process::exit(code)
}

/// Starts the greeter on our socket.
fn spawn(greeter: &[OsString], socket_name: &OsStr) -> anyhow::Result<Child> {
    let mut command = Command::new(&greeter[0]);
    command
        .args(&greeter[1..])
        .env("WAYLAND_DISPLAY", socket_name);
    // std passes on the signal mask, in which SIGTERM and SIGINT are blocked for the signal
    // source; the greeter must be able to die of them.
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            let mut set = std::mem::zeroed::<libc::sigset_t>();
            libc::sigemptyset(&mut set);
            libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
            Ok(())
        });
    }
    command
        .spawn()
        .with_context(|| format!("starting {:?}", greeter[0]))
}

fn pidfd(child: &Child) -> anyhow::Result<OwnedFd> {
    let pid = rustix::process::Pid::from_child(child);
    Ok(rustix::process::pidfd_open(
        pid,
        rustix::process::PidfdFlags::empty(),
    )?)
}

/// SIGTERM the greeter and wait for it a little; SIGKILL it if it is still there.
fn stop_child(mut child: Child) {
    let Ok(fd) = pidfd(&child) else {
        let _ = child.kill();
        let _ = child.wait();
        return;
    };
    let _ = rustix::process::pidfd_send_signal(&fd, rustix::process::Signal::TERM);
    let mut poll = [rustix::event::PollFd::new(
        &fd,
        rustix::event::PollFlags::IN,
    )];
    let timeout =
        rustix::time::Timespec::try_from(CHILD_GRACE).expect("CHILD_GRACE fits a timespec");
    if !matches!(rustix::event::poll(&mut poll, Some(&timeout)), Ok(1)) {
        warn!("the greeter did not exit on SIGTERM; killing it");
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(OsString::from))
    }

    #[test]
    fn full_command_line() {
        let args = parse(&[
            "--drm-device",
            "/dev/dri/card1",
            "--scale",
            "eDP-1=1.5",
            "--scale",
            "HDMI-A-1=2",
            "--log-timing",
            "--",
            "greeter",
            "--flag",
        ])
        .unwrap();
        assert_eq!(
            args,
            Args {
                drm_device: Some("/dev/dri/card1".into()),
                scales: vec![("eDP-1".into(), 1.5), ("HDMI-A-1".into(), 2.0)],
                log_timing: true,
                greeter: vec!["greeter".into(), "--flag".into()],
            }
        );
    }

    #[test]
    fn options_after_the_separator_belong_to_the_greeter() {
        let args = parse(&["--", "greeter", "--log-timing"]).unwrap();
        assert!(!args.log_timing);
        assert_eq!(args.greeter, ["greeter", "--log-timing"]);
    }

    #[test]
    fn bad_command_lines() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--"]).is_err());
        assert!(parse(&["greeter"]).is_err());
        assert!(parse(&["--scale", "eDP-1", "--", "g"]).is_err());
        assert!(parse(&["--scale", "eDP-1=0", "--", "g"]).is_err());
        assert!(parse(&["--scale", "eDP-1=0.4", "--", "g"]).is_err());
        assert!(parse(&["--scale", "eDP-1=4.5", "--", "g"]).is_err());
        assert!(parse(&["--scale", "eDP-1=4", "--", "g"]).is_ok());
        assert!(parse(&["--scale", "eDP-1=nan", "--", "g"]).is_err());
        assert!(parse(&["--drm-device"]).is_err());
    }
}
