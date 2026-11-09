use std::io::{self, ErrorKind};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream as StdUnixStream;

use anyhow::Context;
use tokio::io::Interest;
use tokio::net::UnixStream;
use tokio::time::{self, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::{Connection, EventQueue, QueueHandle};

/// A Wayland event queue on tokio: its connection's socket is waited for through a duplicate
/// registered as a `UnixStream`, and events are read with wayland-client's `prepare_read` and `read`, so no second event
/// loop is needed.
pub struct Queue<S> {
    queue: EventQueue<S>,
    socket: UnixStream,
}

impl<S: 'static> Queue<S> {
    pub fn new(conn: &Connection, queue: EventQueue<S>) -> anyhow::Result<Self> {
        let socket = StdUnixStream::from(conn.as_fd().try_clone_to_owned()?);
        // tokio requires this. wayland-client already sends and receives without blocking.
        socket.set_nonblocking(true)?;
        Ok(Self {
            queue,
            socket: UnixStream::from_std(socket)?,
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
        tokio::select! {
            ready = self.socket.readable() => ready.context("waiting for the Wayland compositor")?,
            () = time::sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {
                return Ok(false);
            }
        }
        // try_io forgets the socket's readiness if the read would block.
        let read = self.socket.try_io(Interest::READABLE, || {
            guard.read().map_err(|e| match e {
                WaylandError::Io(e) => e,
                e => io::Error::other(e),
            })
        });
        match read {
            Err(e) if e.kind() != ErrorKind::WouldBlock => {
                return Err(e).context("reading from the Wayland compositor");
            }
            _ => {}
        }
        Ok(true)
    }
}
