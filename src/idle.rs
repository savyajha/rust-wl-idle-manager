use std::collections::VecDeque;
use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, backend::WaylandError, delegate_noop,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::ExtIdleNotifierV1;

/// A change in idleness, for the timeout at this index in the list given to `connect`.
#[derive(Debug, PartialEq)]
pub enum IdleEvent {
    Idled(usize),
    Resumed(usize),
}

/// Watches the compositor's idle notifications, one per configured timeout.
pub struct IdleWatcher {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    /// A duplicate of the connection's fd, registered with tokio.
    fd: AsyncFd<OwnedFd>,
}

/// Events dispatched from the Wayland queue but not yet returned by `next`.
struct State {
    pending: VecDeque<IdleEvent>,
}

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
        for (index, &(after, ignore_inhibit)) in timeouts.iter().enumerate() {
            let ms = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
            if ignore_inhibit {
                notifier.get_input_idle_notification(ms, &seat, &qh, index);
            } else {
                notifier.get_idle_notification(ms, &seat, &qh, index);
            }
        }
        let fd = conn.as_fd().try_clone_to_owned()?;
        Ok(Self {
            fd: AsyncFd::with_interest(fd, Interest::READABLE)?,
            conn,
            queue,
            state: State {
                pending: VecDeque::new(),
            },
        })
    }

    /// Wait for the next idle or resume event; an error means the compositor is gone.
    pub async fn next(&mut self) -> anyhow::Result<IdleEvent> {
        loop {
            self.queue.dispatch_pending(&mut self.state)?;
            if let Some(idle) = self.state.pending.pop_front() {
                return Ok(idle);
            }
            self.flush()?;
            // Only the system backend returns None (events queued); dispatch_pending takes them.
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let mut ready = self.fd.readable().await?;
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
        match self.conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            result => result.context("writing to the Wayland compositor"),
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, usize> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        wl_event: ext_idle_notification_v1::Event,
        &index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match wl_event {
            ext_idle_notification_v1::Event::Idled => {
                state.pending.push_back(IdleEvent::Idled(index))
            }
            ext_idle_notification_v1::Event::Resumed => {
                state.pending.push_back(IdleEvent::Resumed(index))
            }
            _ => {}
        }
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
