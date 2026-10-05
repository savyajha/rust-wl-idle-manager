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
use crate::password::{Look, Status};
use crate::policy::Input;

/// `0xRRGGBB` as one pixel in `Xrgb8888`'s byte order.
const fn xrgb(rgb: u32) -> [u8; 4] {
    let [_, r, g, b] = rgb.to_be_bytes();
    [b, g, r, 0xff]
}

const BACKGROUND: [u8; 4] = xrgb(0x203040);
/// The password field, idle or typing; its dots; checking; failed; the caps lock bar.
const FIELD: [u8; 4] = xrgb(0x304860);
const DOT: [u8; 4] = xrgb(0xe0e8f0);
const CHECKING: [u8; 4] = xrgb(0x3070c0);
const FAILED: [u8; 4] = xrgb(0xc03030);
const CAPS_LOCK: [u8; 4] = xrgb(0xe0a020);

/// The password field's width and height, centred on each output, in pixels.
const FIELD_SIZE: (usize, usize) = (300, 50);
/// The side of a dot, which is also the gap between dots.
const DOT_SIZE: usize = 10;
/// The most dots the field shows; a longer password shows this many.
const MAX_DOTS: usize = (FIELD_SIZE.0 - DOT_SIZE) / (2 * DOT_SIZE);
/// The caps lock bar's height and its gap below the field.
const CAPS_LOCK_BAR: (usize, usize) = (6, 10);

/// A session lock we requested, with a surface on each output.
pub struct Lock {
    session: SessionLock,
    /// Each output's surface, with its size once configured (until then zero).
    surfaces: Vec<(WlOutput, SessionLockSurface, (u32, u32))>,
    /// What the surfaces show of the password entry.
    shown: Option<Look>,
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
        if self.surfaces.iter().any(|(o, ..)| *o == output) {
            return;
        }
        let surface = compositor.create_surface(qh, ());
        let lock_surface = self.session.create_lock_surface(surface, &output, qh);
        self.surfaces.push((output, lock_surface, (0, 0)));
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
            shown: None,
            requested: Instant::now(),
            drawn: false,
            unlocking: false,
        };
        for output in state.outputs.outputs() {
            lock.add_surface(&state.compositor, output, &qh);
        }
        state.lock = Some(lock);
        let flushed = self.flush();
        // After the flush, to stay out of the lock's latency.
        self.state.entry.reset();
        flushed
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
        self.entry.reset();
    }

    /// Draw every configured lock surface again, if what the entry shows has changed.
    pub(super) fn redraw(&mut self) {
        let look = self.entry.look();
        let Some(lock) = &mut self.lock else { return };
        if lock.shown.replace(look) == Some(look) {
            return;
        }
        for (_, surface, size) in &lock.surfaces {
            if *size != (0, 0)
                && let Err(e) = draw(&mut self.pool, surface.wl_surface(), *size, look)
            {
                error!("drawing the lock screen: {e:#}");
            }
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

/// Draw `look` into a `width` × `height` buffer from `pool` and show it on `surface`: the
/// background, the password field in the colour of its status with a dot per character,
/// and a bar below it while caps lock is on.
fn draw(
    pool: &mut SlotPool,
    surface: &WlSurface,
    (width, height): (u32, u32),
    look: Look,
) -> anyhow::Result<()> {
    let (width, height) = (i32::try_from(width)?, i32::try_from(height)?);
    let (buffer, canvas) = pool.create_buffer(width, height, width * 4, Format::Xrgb8888)?;
    let pixels = canvas.as_chunks_mut().0;
    pixels.fill(BACKGROUND);
    let (w, h) = (width as usize, height as usize);
    let (field_w, field_h) = FIELD_SIZE;
    let (x, y) = (w.saturating_sub(field_w) / 2, h.saturating_sub(field_h) / 2);
    let field = match look.status {
        Status::Idle | Status::Typing => FIELD,
        Status::Checking => CHECKING,
        Status::Failed => FAILED,
    };
    fill(pixels, w, (x, y, field_w, field_h), field);
    let dots = look.chars.min(MAX_DOTS);
    let row = (2 * dots).saturating_sub(1) * DOT_SIZE;
    let (dots_x, dots_y) = (
        (w / 2).saturating_sub(row / 2),
        (h / 2).saturating_sub(DOT_SIZE / 2),
    );
    for i in 0..dots {
        let dot = (dots_x + 2 * i * DOT_SIZE, dots_y, DOT_SIZE, DOT_SIZE);
        fill(pixels, w, dot, DOT);
    }
    if look.caps_lock {
        let (bar_h, gap) = CAPS_LOCK_BAR;
        fill(pixels, w, (x, y + field_h + gap, field_w, bar_h), CAPS_LOCK);
    }
    buffer.attach_to(surface)?;
    surface.damage_buffer(0, 0, width, height);
    surface.commit();
    // Dropping the buffer destroys it only once the compositor has released it.
    Ok(())
}

/// Fill the rectangle `(x, y, width, height)` of `pixels`, rows `stride` pixels long,
/// clipped to them.
fn fill(
    pixels: &mut [[u8; 4]],
    stride: usize,
    (x, y, width, height): (usize, usize, usize, usize),
    colour: [u8; 4],
) {
    let end = (x + width).min(stride);
    for row in pixels.chunks_exact_mut(stride).skip(y).take(height) {
        row[x.min(end)..end].fill(colour);
    }
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
        let look = self.entry.look();
        lock.shown = Some(look);
        if let Some((.., size)) = lock
            .surfaces
            .iter_mut()
            .find(|(_, s, _)| s.wl_surface() == surface.wl_surface())
        {
            *size = configure.new_size;
        }
        if let Err(e) = draw(
            &mut self.pool,
            surface.wl_surface(),
            configure.new_size,
            look,
        ) {
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
            lock.surfaces.retain(|(o, ..)| *o != output);
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
