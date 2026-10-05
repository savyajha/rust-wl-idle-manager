use std::time::Instant;

use anyhow::Context;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockSurface, SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler, slot::SlotPool};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use tracing::{error, info, warn};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, QueueHandle};

use super::{State, Wayland};
use crate::policy::Input;

/// The lock screen's colour, #203040, as one pixel in `Xrgb8888`'s byte order.
const BACKGROUND: [u8; 4] = [0x40, 0x30, 0x20, 0xff];

/// A session lock we requested, with a surface on each output.
pub struct Lock {
    session: SessionLock,
    surfaces: Vec<(WlOutput, SessionLockSurface)>,
    /// When the lock was requested, for the latency logs.
    requested: Instant,
    /// Whether a surface has been drawn yet.
    drawn: bool,
    /// logind asked to unlock before the compositor locked; unlock once it has.
    unlocking: bool,
}

impl Lock {
    /// Give `output` a lock surface, unless it has one.
    fn add_surface(
        &mut self,
        compositor: &WlCompositor,
        output: WlOutput,
        qh: &QueueHandle<State>,
    ) {
        if self.surfaces.iter().any(|(o, _)| *o == output) {
            return;
        }
        let surface = compositor.create_surface(qh, ());
        let lock_surface = self.session.create_lock_surface(surface, &output, qh);
        self.surfaces.push((output, lock_surface));
    }
}

impl Wayland {
    /// Lock the session with the built-in lock screen, unless a lock is already requested;
    /// `next` returns `Locked(true)` once the compositor has locked it.
    pub fn lock(&mut self) -> anyhow::Result<()> {
        if let Some(lock) = &mut self.state.lock {
            lock.unlocking = false;
            return Ok(());
        }
        let qh = self.queue.handle();
        let state = &mut self.state;
        let mut lock = Lock {
            session: state
                .lock_manager
                .lock(&qh)
                .context("requesting a session lock")?,
            surfaces: Vec::new(),
            requested: Instant::now(),
            drawn: false,
            unlocking: false,
        };
        for output in state.outputs.outputs() {
            lock.add_surface(&state.compositor, output, &qh);
        }
        state.lock = Some(lock);
        self.flush()
    }

    /// Unlock the built-in lock screen, or, if the compositor has not locked yet, unlock
    /// once it has.
    pub fn unlock(&mut self) {
        match &mut self.state.lock {
            None => info!("not locked; nothing to unlock"),
            // Destroying the lock now would be a protocol error if `locked` is on its way.
            Some(lock) if !lock.session.is_locked() => {
                info!("unlock deferred until locked");
                lock.unlocking = true;
            }
            Some(_) => self.state.end_lock(),
        }
    }
}

impl State {
    /// Drop the lock and its surfaces, and report the session unlocked.
    fn end_lock(&mut self) {
        if let Some(lock) = self.lock.take() {
            // unlock_and_destroy if it was locked; otherwise dropping it destroys it, as the
            // protocol asks.
            lock.session.unlock();
            self.inputs.push_back(Input::Locked(false));
        }
    }

    /// Log the latencies measured since the last call.
    pub(super) fn log_latencies(&mut self) {
        for (what, after) in self.latencies.drain(..) {
            info!(
                "{what} {:.3} ms after the request",
                after.as_secs_f64() * 1000.0
            );
        }
    }

    /// Grow the pool to hold a lock surface for every output, and draw the background
    /// into it once, so that the first lock draws into pages already in memory.
    fn prepare_pool(&mut self) {
        if !self.lock_screen || self.lock.is_some() {
            return;
        }
        // The lock surfaces' sizes, rounded up as the pool rounds its slots. An output
        // without a logical size (no xdg-output) is left out; its first lock is slower.
        let len: usize = self
            .outputs
            .outputs()
            .filter_map(|output| self.outputs.info(&output)?.logical_size)
            .map(|(width, height)| (width as usize * height as usize * 4).next_multiple_of(64))
            .sum();
        match self.pool.new_slot(len) {
            Ok(slot) => self
                .pool
                .raw_data_mut(&slot)
                .as_chunks_mut()
                .0
                .fill(BACKGROUND),
            Err(e) => error!("preparing the lock screen's buffers: {e}"),
        }
    }
}

/// Fill a `width` × `height` buffer from `pool` with the background and show it on `surface`.
fn draw(
    pool: &mut SlotPool,
    surface: &WlSurface,
    (width, height): (u32, u32),
) -> anyhow::Result<()> {
    let (width, height) = (i32::try_from(width)?, i32::try_from(height)?);
    let (buffer, canvas) = pool.create_buffer(width, height, width * 4, Format::Xrgb8888)?;
    canvas.as_chunks_mut().0.fill(BACKGROUND);
    buffer.attach_to(surface)?;
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    // Dropping the buffer destroys it only once the compositor has released it.
    Ok(())
}

impl SessionLockHandler for State {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        let Some(lock) = &self.lock else { return };
        self.latencies.push(("locked", lock.requested.elapsed()));
        if lock.unlocking {
            self.end_lock();
        } else {
            self.inputs.push_back(Input::Locked(true));
        }
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        warn!("the compositor refused or ended our lock; another lock screen may hold it");
        self.end_lock();
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let Some(lock) = &mut self.lock else { return };
        if let Err(e) = draw(&mut self.pool, surface.wl_surface(), configure.new_size) {
            error!("drawing the lock screen: {e:#}");
        } else if !lock.drawn {
            lock.drawn = true;
            self.latencies
                .push(("lock screen drawn", lock.requested.elapsed()));
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    /// A new output gets a lock surface while locked; until then the compositor blanks it.
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: WlOutput) {
        match &mut self.lock {
            Some(lock) => lock.add_surface(&self.compositor, output, qh),
            None => self.prepare_pool(),
        }
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {
        self.prepare_pool();
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        if let Some(lock) = &mut self.lock {
            lock.surfaces.retain(|(o, _)| *o != output);
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_registry!(State);
delegate_dispatch2!(State);
