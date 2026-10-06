mod keyboard;
mod lock;

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use anyhow::{Context, bail};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::session_lock::SessionLockState;
use smithay_client_toolkit::shm::{Shm, slot::SlotPool};
use tokio::io::unix::AsyncFd;
use tokio::time::{self, Instant};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, backend::WaylandError, delegate_noop,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    Event, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use xkbcommon::xkb;

use crate::config::LockScreen;
use crate::draw::{self, Painter};
use crate::password::Entry;
use crate::policy::Input;
use lock::{Lock, Prepared};

/// The globals for fractional scaling, when the compositor has both.
type Scaling = (WpFractionalScaleManagerV1, WpViewporter);

/// The daemon's one connection to the compositor: an idle notification per configured
/// timeout, and the built-in lock screen with its keyboard.
pub struct Wayland {
    queue: EventQueue<State>,
    state: State,
    seat: WlSeat,
    notifier: ExtIdleNotifierV1,
    /// Each timeout's `(timeout, ignore_inhibit)`, as given to `connect`.
    timeouts: Vec<(Duration, bool)>,
    /// The notification for each timeout, by index.
    notifications: Vec<ExtIdleNotificationV1>,
    /// A duplicate of the connection's fd, registered with tokio.
    fd: AsyncFd<OwnedFd>,
}

/// What the Wayland queue dispatches to.
struct State {
    /// Inputs dispatched from the queue but not yet returned by `next`.
    inputs: VecDeque<Input>,
    registry: RegistryState,
    outputs: OutputState,
    /// The first keyboard to appear, while the seat has one (lock screen only).
    keyboard: Option<WlKeyboard>,
    xkb: xkb::Context,
    /// The keyboard's keymap and modifiers, once the compositor has sent the keymap.
    xkb_state: Option<xkb::State>,
    /// The password typed on the lock screen.
    entry: Entry,
    compositor: WlCompositor,
    shm: Shm,
    /// The memory lock surfaces draw into, kept from one lock to the next.
    pool: SlotPool,
    lock_manager: SessionLockState,
    scaling: Option<Scaling>,
    /// What draws the built-in lock screen, if it is in use.
    painter: Option<Painter>,
    /// Each output's lock screen, prepared ahead of the lock.
    scenes: Vec<Prepared>,
    /// The scale the compositor last preferred for each output's lock surface.
    preferred: Vec<(WlOutput, u32)>,
    /// The minute the clock was last rendered for.
    minute: i64,
    /// The lock we requested, until it is unlocked or the compositor ends it.
    lock: Option<Lock>,
    /// Lock latencies measured while dispatching, to log once requests are flushed.
    latencies: Vec<(&'static str, Duration)>,
}

impl Wayland {
    /// Connect to the compositor and create one notification per `(timeout, ignore_inhibit)`.
    /// With a `lock_screen`, the compositor must support `ext-session-lock-v1`.
    pub fn connect(
        timeouts: &[(Duration, bool)],
        lock_screen: Option<LockScreen>,
    ) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).context("listing Wayland globals")?;
        let qh = queue.handle();
        // The first seat, for the idle notifications and the keyboard; niri has only one.
        // Version 10 would have the compositor repeat keys, which the entry does itself.
        let seat: WlSeat = globals.bind(&qh, 1..=9, ()).context("binding wl_seat")?;
        let notifier: ExtIdleNotifierV1 = globals
            .bind(&qh, 1..=2, ())
            .context("binding ext_idle_notifier_v1")?;
        if notifier.version() < 2 && timeouts.iter().any(|&(_, ignore)| ignore) {
            bail!("ignore-inhibit needs ext_idle_notifier_v1 version 2; the compositor has 1");
        }
        let can_lock = globals.contents().with_list(|list| {
            list.iter()
                .any(|global| global.interface == "ext_session_lock_manager_v1")
        });
        if lock_screen.is_some() && !can_lock {
            bail!("the built-in lock screen needs ext-session-lock-v1, which the compositor lacks");
        }
        let shm = Shm::bind(&globals, &qh).context("binding wl_shm")?;
        let state = State {
            inputs: VecDeque::new(),
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            keyboard: None,
            xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            entry: Entry::new(),
            compositor: globals
                .bind(&qh, 1..=4, ())
                .context("binding wl_compositor")?,
            // Grown as the lock screen is prepared for each output.
            pool: SlotPool::new(1, &shm).context("creating the lock screen's buffer pool")?,
            shm,
            lock_manager: SessionLockState::new(&globals, &qh),
            scaling: globals
                .bind(&qh, 1..=1, ())
                .ok()
                .zip(globals.bind(&qh, 1..=1, ()).ok()),
            painter: lock_screen.map(Painter::new),
            scenes: Vec::new(),
            preferred: Vec::new(),
            minute: draw::minute(),
            lock: None,
            latencies: Vec::new(),
        };
        let fd = conn.as_fd().try_clone_to_owned()?;
        let mut wayland = Self {
            fd: AsyncFd::new(fd)?,
            queue,
            state,
            seat,
            notifier,
            timeouts: timeouts.to_vec(),
            notifications: Vec::new(),
        };
        wayland.notifications = (0..timeouts.len())
            .map(|i| wayland.notification(i))
            .collect();
        Ok(wayland)
    }

    /// Destroy the notifications at `indices` and create them again, so their timers
    /// start from now. Their events not yet returned by `next` are dropped.
    pub fn rearm(&mut self, indices: &[usize]) {
        self.state.inputs.retain(
            |input| !matches!(input, Input::Idled(i) | Input::Resumed(i) if indices.contains(i)),
        );
        for &i in indices {
            self.notifications[i].destroy();
            self.notifications[i] = self.notification(i);
        }
    }

    /// Ask for a notification for the timeout at `index`; it is sent on the next flush.
    fn notification(&self, index: usize) -> ExtIdleNotificationV1 {
        let (after, ignore_inhibit) = self.timeouts[index];
        let ms = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
        let qh = self.queue.handle();
        if ignore_inhibit {
            self.notifier
                .get_input_idle_notification(ms, &self.seat, &qh, index)
        } else {
            self.notifier
                .get_idle_notification(ms, &self.seat, &qh, index)
        }
    }

    /// Wait for the next input from the compositor; an error means the compositor is gone.
    pub async fn next(&mut self) -> anyhow::Result<Input> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatching Wayland events")?;
            // Whatever changed the password entry (keys, its timers, the helper's answer).
            self.state.redraw();
            // Before returning, so that lock surfaces drawn while dispatching go out at once.
            self.flush()?;
            self.state.log_latencies();
            // After the flush, so that a new minute never delays a lock.
            if self.state.refresh_clock() {
                continue;
            }
            if let Some(input) = self.state.inputs.pop_front() {
                return Ok(input);
            }
            // Only the system backend returns None (events queued); dispatch_pending takes them.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            // The lock screen's timers: key repeat, the failure shown, the cooldown, forgetting
            // the password.
            let deadline = self.state.entry.deadline();
            let mut ready = tokio::select! {
                ready = self.fd.readable() => ready.context("waiting for the Wayland compositor")?,
                () = time::sleep_until(deadline.map_or_else(Instant::now, Instant::from_std)),
                    if deadline.is_some() =>
                {
                    self.state.entry.tick(std::time::Instant::now());
                    continue;
                }
                // The clock's next minute.
                () = time::sleep(draw::until_next_minute()), if self.state.painter.is_some() => {
                    continue;
                }
            };
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    ready.clear_ready();
                }
                Err(e) => return Err(e).context("reading from the Wayland compositor"),
            }
        }
    }

    /// Send queued requests; any the socket cannot take yet stay queued for the next flush.
    fn flush(&self) -> anyhow::Result<()> {
        match self.queue.flush() {
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            result => result.context("writing to the Wayland compositor"),
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        wl_event: Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.inputs.push_back(match wl_event {
            Event::Idled => Input::Idled(index),
            Event::Resumed => Input::Resumed(index),
            _ => return,
        });
    }
}

delegate_noop!(State: ExtIdleNotifierV1);
delegate_noop!(State: WpFractionalScaleManagerV1);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
delegate_noop!(State: WlCompositor);
// A lock surface covers its one output, so its enter, leave and scale events change nothing.
delegate_noop!(State: ignore WlSurface);
