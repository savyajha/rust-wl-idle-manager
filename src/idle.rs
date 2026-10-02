use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::unix::AsyncFd;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, backend::WaylandError, delegate_noop,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    Event, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

use crate::policy::Input;

/// Watches the compositor's idle notifications, one per configured timeout.
pub struct IdleWatcher {
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

/// `Idled` and `Resumed` inputs dispatched from the Wayland queue but not yet returned
/// by `next`.
struct State(VecDeque<Input>);

impl IdleWatcher {
    /// Connect to the compositor and create one notification per `(timeout, ignore_inhibit)`.
    pub fn connect(timeouts: &[(Duration, bool)]) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).context("listing Wayland globals")?;
        let qh = queue.handle();
        let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("binding wl_seat")?;
        let notifier: ExtIdleNotifierV1 = globals
            .bind(&qh, 1..=2, ())
            .context("binding ext_idle_notifier_v1")?;
        if notifier.version() < 2 && timeouts.iter().any(|&(_, ignore)| ignore) {
            bail!("ignore-inhibit needs ext_idle_notifier_v1 version 2; the compositor has 1");
        }
        let fd = conn.as_fd().try_clone_to_owned()?;
        let mut watcher = Self {
            fd: AsyncFd::new(fd)?,
            queue,
            state: State(VecDeque::new()),
            seat,
            notifier,
            timeouts: timeouts.to_vec(),
            notifications: Vec::new(),
        };
        watcher.notifications = (0..timeouts.len())
            .map(|i| watcher.notification(i))
            .collect();
        Ok(watcher)
    }

    /// Destroy the notifications at `indices` and create them again, so their timers
    /// start from now. Their events not yet returned by `next` are dropped.
    pub fn rearm(&mut self, indices: &[usize]) {
        self.state.0.retain(
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

    /// Wait for the next `Idled` or `Resumed`; an error means the compositor is gone.
    pub async fn next(&mut self) -> anyhow::Result<Input> {
        loop {
            self.queue
                .dispatch_pending(&mut self.state)
                .context("dispatching Wayland events")?;
            if let Some(input) = self.state.0.pop_front() {
                return Ok(input);
            }
            self.flush()?;
            // Only the system backend returns None (events queued); dispatch_pending takes them.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut ready = self
                .fd
                .readable()
                .await
                .context("waiting for the Wayland compositor")?;
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
        state.0.push_back(match wl_event {
            Event::Idled => Input::Idled(index),
            Event::Resumed => Input::Resumed(index),
            _ => return,
        });
    }
}

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ExtIdleNotifierV1);
