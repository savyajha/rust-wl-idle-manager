use std::io::ErrorKind;
use std::os::fd::{AsFd, OwnedFd};

use anyhow::Context;
use tokio::io::unix::AsyncFd;
use tokio::time::{self, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::{Connection, EventQueue, QueueHandle};

/// A Wayland event queue on tokio: its connection's socket is waited for through `AsyncFd`,
/// and events are read with wayland-client's `prepare_read` and `read`, so no second event
/// loop is needed.
pub struct Queue<S> {
    queue: EventQueue<S>,
    fd: AsyncFd<OwnedFd>,
}

impl<S: 'static> Queue<S> {
    pub fn new(conn: &Connection, queue: EventQueue<S>) -> anyhow::Result<Self> {
        let fd = conn.as_fd().try_clone_to_owned()?;
        Ok(Self {
            queue,
            fd: AsyncFd::new(fd)?,
        })
    }

    pub fn handle(&self) -> QueueHandle<S> {
        self.queue.handle()
    }

    /// Dispatch the events read so far to `state`.
    pub fn dispatch(&mut self, state: &mut S) -> anyhow::Result<()> {
        self.queue
            .dispatch_pending(state)
            .context("dispatching Wayland events")?;
        Ok(())
    }

    /// Send the requests made so far; those the socket cannot take now go with the next.
    pub fn flush(&self) -> anyhow::Result<()> {
        match self.queue.flush() {
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            result => result.context("writing to the Wayland compositor"),
        }
    }

    /// Wait for events to dispatch, and read them, or for `wake`; returns false if `wake`
    /// came first. An error means the compositor is gone. Cancel-safe.
    pub async fn wait(&mut self, wake: Option<Instant>) -> anyhow::Result<bool> {
        // Only the system backend returns None (events queued); `dispatch` takes them.
        let Some(guard) = self.queue.prepare_read() else {
            return Ok(true);
        };
        let mut ready = tokio::select! {
            ready = self.fd.readable() => ready.context("waiting for the Wayland compositor")?,
            () = time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                return Ok(false);
            }
        };
        match guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                ready.clear_ready();
            }
            Err(e) => return Err(e).context("reading from the Wayland compositor"),
        }
        Ok(true)
    }
}
