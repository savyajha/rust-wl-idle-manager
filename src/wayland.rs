mod keyboard;
mod lock;

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::mem;
use std::os::fd::{AsFd, OwnedFd};

use anyhow::{Context, bail};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use tokio::io::unix::AsyncFd;
use tokio::time::{self, Instant};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_compositor::WlCompositor;
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

use crate::config::{LockScreen, Timeout};
use crate::entry::Entry;
use crate::policy::Input;
use crate::wallpaper::Blurred;
use lock::Screen;

/// The daemon's connection to the compositor: idle notifications and the lock screen.
pub struct Wayland {
    queue: EventQueue<State>,
    state: State,
    seat: WlSeat,
    notifier: ExtIdleNotifierV1,
    timeouts: Vec<Timeout>,
    notifications: Vec<ExtIdleNotificationV1>,
    fd: AsyncFd<OwnedFd>,
}

pub enum Next {
    Input(Input),
    /// A password was submitted on the lock screen; `Entry::submission` has it.
    Password,
}

struct State {
    /// Inputs dispatched from the queue but not yet returned by `next`.
    inputs: VecDeque<Input>,
    registry: RegistryState,
    output_state: OutputState,
    /// The built-in lock screen; `None` with a `locker`.
    lock_screen: Option<Screen>,
}

impl Wayland {
    /// Connect to the compositor and create one notification per timeout.
    pub fn connect(timeouts: &[Timeout], lock_screen: Option<LockScreen>) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).context("listing Wayland globals")?;
        let qh = queue.handle();
        // The first seat; niri has only one. Version 10 would have the compositor repeat
        // keys, which the entry does itself.
        let seat: WlSeat = globals.bind(&qh, 1..=9, ()).context("binding wl_seat")?;
        let notifier: ExtIdleNotifierV1 = globals
            .bind(&qh, 1..=2, ())
            .context("binding ext_idle_notifier_v1")?;
        if notifier.version() < 2 && timeouts.iter().any(|timeout| timeout.ignore_inhibit) {
            bail!("ignore-inhibit needs ext_idle_notifier_v1 version 2; the compositor has 1");
        }
        let state = State {
            inputs: VecDeque::new(),
            registry: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            lock_screen: lock_screen
                .map(|config| Screen::new(&globals, &qh, config))
                .transpose()?,
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

    fn notification(&self, index: usize) -> ExtIdleNotificationV1 {
        let timeout = &self.timeouts[index];
        let ms = u32::try_from(timeout.after.as_millis())
            .expect("the config keeps a timeout within ext-idle-notify's u32 milliseconds");
        let qh = self.queue.handle();
        if timeout.ignore_inhibit {
            self.notifier
                .get_input_idle_notification(ms, &self.seat, &qh, index)
        } else {
            self.notifier
                .get_idle_notification(ms, &self.seat, &qh, index)
        }
    }

    /// The password typed on the lock screen, if there is one.
    pub fn entry_mut(&mut self) -> Option<&mut Entry> {
        self.state
            .lock_screen
            .as_mut()
            .map(|screen| &mut screen.entry)
    }

    /// Lock with the built-in lock screen; `next` returns `Locked(true)` once locked.
    pub fn lock(&mut self) -> anyhow::Result<()> {
        let Some(screen) = &mut self.state.lock_screen else {
            bail!("there is no built-in lock screen to lock with");
        };
        screen.lock(&self.queue.handle())?;
        self.flush()
    }

    /// Unlock the built-in lock screen, or if the compositor has not locked yet, once it has.
    pub fn unlock(&mut self) {
        if let Some(screen) = &mut self.state.lock_screen {
            screen.unlock(&mut self.state.inputs);
        }
    }

    pub fn reload(&mut self, wallpaper: Option<Blurred>) {
        if let Some(screen) = &mut self.state.lock_screen {
            screen.reload(wallpaper);
        }
    }

    /// Wait for the next input from the compositor, or a password submitted; an error means
    /// the compositor is gone. Cancel-safe: what has been dispatched stays queued.
    pub async fn next(&mut self) -> anyhow::Result<Next> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatching Wayland events")?;
            if let Some(screen) = &mut self.state.lock_screen {
                screen.redraw();
            }
            // Before returning, so that lock surfaces drawn while dispatching go out at once.
            self.flush()?;
            if let Some(screen) = &mut self.state.lock_screen {
                screen.log_latencies();
                // After the flush: rendering a new minute takes milliseconds, which must
                // not hold back a lock's first frame.
                if screen.refresh_clock() {
                    continue;
                }
                if mem::take(&mut screen.submitted) {
                    return Ok(Next::Password);
                }
            }
            if let Some(input) = self.state.inputs.pop_front() {
                return Ok(Next::Input(input));
            }
            // Only the system backend returns None (events queued); dispatch_pending takes them.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let wake = self.state.lock_screen.as_ref().map(Screen::next_wake);
            let mut ready = tokio::select! {
                ready = self.fd.readable() => ready.context("waiting for the Wayland compositor")?,
                () = time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                    if let Some(screen) = &mut self.state.lock_screen {
                        screen.entry.tick(Instant::now());
                    }
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
