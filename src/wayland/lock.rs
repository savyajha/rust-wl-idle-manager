use std::time::Instant;

use anyhow::Context;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockSurface, SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use tracing::{error, info, warn};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

use super::{Scaling, State, Wayland};
use crate::draw::{self, Scene};
use crate::password::{Look, Status};
use crate::policy::Input;
use crate::wallpaper::Blurred;

/// A session lock we requested, with a surface on each output.
pub struct Lock {
    session: SessionLock,
    surfaces: Vec<Surface>,
    /// What the surfaces show of the password entry; `None` to draw them again.
    shown: Option<Look>,
    /// When the lock was requested, for the latency logs.
    requested: Instant,
    /// Whether a surface has been drawn yet.
    drawn: bool,
    /// logind asked to unlock before the compositor locked; unlock once it has.
    unlocking: bool,
}

/// A lock surface on one output.
struct Surface {
    output: WlOutput,
    lock_surface: SessionLockSurface,
    /// Its logical size once configured (until then zero).
    size: (u32, u32),
    /// The scale it was last drawn at, once it has been.
    drawn_at: Option<u32>,
    /// It is to be drawn again once the compositor releases one of its buffers.
    waiting: bool,
    /// With fractional scaling: its scale object and viewport.
    scaling: Option<(WpFractionalScaleV1, WpViewport)>,
}

/// The most buffers an output's frames take from the pool: one the compositor shows and
/// one to draw the next frame in. A frame waits for the compositor to release one.
const BUFFERS: usize = 2;

/// An output's lock screen, prepared, with the buffers its frames are drawn into. While
/// unlocked, one is the first frame a lock shows, drawn ahead of time, so that a lock only
/// attaches it.
pub(super) struct Prepared {
    output: WlOutput,
    scene: Scene,
    /// Each buffer, with what it shows (`None` once that is out of date).
    buffers: Vec<(Buffer, Option<Look>)>,
}

/// An output's logical size, scale (in 120ths) and frame size in physical pixels.
type Layout = ((u32, u32), u32, (u32, u32));

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some((fractional, viewport)) = self.scaling.take() {
            fractional.destroy();
            viewport.destroy();
        }
    }
}

impl Lock {
    /// Give `output` a lock surface, unless it has one.
    fn add_surface(
        &mut self,
        compositor: &WlCompositor,
        scaling: Option<&Scaling>,
        output: WlOutput,
        qh: &QueueHandle<State>,
    ) {
        if self.surfaces.iter().any(|s| s.output == output) {
            return;
        }
        let surface = compositor.create_surface(qh, ());
        let scaling = scaling.map(|(fractional, viewporter)| {
            (
                fractional.get_fractional_scale(&surface, qh, surface.clone()),
                viewporter.get_viewport(&surface, qh, ()),
            )
        });
        let lock_surface = self.session.create_lock_surface(surface, &output, qh);
        self.surfaces.push(Surface {
            output,
            lock_surface,
            size: (0, 0),
            drawn_at: None,
            waiting: false,
            scaling,
        });
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
            lock.add_surface(&state.compositor, state.scaling.as_ref(), output, &qh);
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

    /// Show `wallpaper` from now on (keep the one shown without), and read GTK's colours
    /// again: every output's lock screen is prepared again, and redrawn if locked.
    pub fn set_wallpaper(&mut self, wallpaper: Option<Blurred>) {
        let Some(painter) = &mut self.state.painter else {
            return;
        };
        if let Some(wallpaper) = wallpaper {
            painter.set_wallpaper(wallpaper);
        }
        painter.reload_colours();
        self.state.scenes.clear();
        self.state.prepare();
        if let Some(lock) = &mut self.state.lock {
            lock.shown = None;
        }
    }
}

impl State {
    /// Drop the lock and its surfaces, report the session unlocked, and prepare the next
    /// lock's first frames in a new pool: the old one grew for the frames drawn while
    /// locked, and a pool cannot shrink.
    fn end_lock(&mut self) {
        if let Some(lock) = self.lock.take() {
            // unlock_and_destroy if it was locked; otherwise dropping it destroys it, as the
            // protocol asks.
            lock.session.unlock();
            self.inputs.push_back(Input::Locked(false));
        }
        self.entry.reset();
        let len = self
            .scenes
            .iter()
            .map(|p| p.scene.pixels.0 * p.scene.pixels.1 * 4);
        match SlotPool::new(len.sum::<u32>().max(1) as usize, &self.shm) {
            Ok(pool) => {
                self.scenes.iter_mut().for_each(|p| p.buffers.clear());
                self.pool = pool;
            }
            Err(e) => error!("creating a new buffer pool: {e}"),
        }
        self.prepare();
    }

    /// Draw every lock surface again if what the entry shows has changed or a redraw was
    /// asked for, and each one waiting for a buffer (one may have been released since).
    pub(super) fn redraw(&mut self) {
        let look = self.entry.look();
        let Some(lock) = &mut self.lock else { return };
        let changed = lock.shown.replace(look) != Some(look);
        for i in 0..lock.surfaces.len() {
            if changed
                || self
                    .lock
                    .as_ref()
                    .is_some_and(|lock| lock.surfaces[i].waiting)
            {
                self.draw(i, look);
            }
        }
    }

    /// Render the date and clock again once the minute has changed, if what they show has;
    /// returns whether it did, and then the lock screen is drawn again, or its first frames
    /// while unlocked.
    pub(super) fn refresh_clock(&mut self) -> bool {
        let Some(painter) = &self.painter else {
            return false;
        };
        let minute = draw::minute();
        if self.minute == minute {
            return false;
        }
        self.minute = minute;
        let mut changed = false;
        for prepared in &mut self.scenes {
            if painter.refresh(&mut prepared.scene) {
                prepared
                    .buffers
                    .iter_mut()
                    .for_each(|(_, shows)| *shows = None);
                changed = true;
            }
        }
        if !changed {
            return false;
        }
        match &mut self.lock {
            Some(lock) => lock.shown = None,
            None => self.prepare(),
        }
        true
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

    /// How `output` is drawn, at its logical `size` (or, without one, the size it says it
    /// has): at the scale the compositor last preferred for it, or until it has said, the
    /// one it most likely will (its mode's width over its logical width, with fractional
    /// scaling; its integer scale without).
    fn layout(&self, output: &WlOutput, size: Option<(u32, u32)>) -> Option<Layout> {
        let info = self.outputs.info(output)?;
        let logical = info.logical_size.map(|(w, h)| (w as u32, h as u32));
        let size = size.or(logical)?;
        let sideways = matches!(
            info.transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        );
        let mode = info.modes.iter().find(|mode| mode.current).map(|mode| {
            let (w, h) = (mode.dimensions.0 as u32, mode.dimensions.1 as u32);
            if sideways { (h, w) } else { (w, h) }
        });
        let preferred = self.preferred.iter().find(|(o, _)| o == output);
        let scale = match (&self.scaling, preferred, mode) {
            (None, ..) => info.scale_factor.max(1) as u32 * 120,
            (Some(_), Some((_, scale)), _) => *scale,
            (Some(_), None, Some(mode)) => draw::scale_of(mode.0, size.0),
            (Some(_), None, None) => 120,
        };
        let mode = mode.filter(|_| self.scaling.is_some());
        Some((size, scale, draw::pixels(size, scale, mode)))
    }

    /// The index in `scenes` of `output`'s lock screen with `layout`, prepared now if it is
    /// missing or out of date; `when` says when, in the log.
    fn scene(&mut self, output: &WlOutput, layout: Layout, when: &str) -> Option<usize> {
        let (size, scale, pixels) = layout;
        let current = self.scenes.iter().position(|p| p.output == *output);
        if let Some(i) = current
            && (self.scenes[i].scene.size, self.scenes[i].scene.scale) == (size, scale)
        {
            return Some(i);
        }
        let started = Instant::now();
        let scene = self.painter.as_ref()?.scene(size, scale, pixels);
        info!(
            "lock screen prepared{when} for {}×{} at scale {} ({}×{} px) in {:.1} ms",
            size.0,
            size.1,
            f64::from(scale) / 120.0,
            pixels.0,
            pixels.1,
            started.elapsed().as_secs_f64() * 1000.0
        );
        self.scenes.retain(|p| p.output != *output);
        self.scenes.push(Prepared {
            output: output.clone(),
            scene,
            buffers: Vec::new(),
        });
        Some(self.scenes.len() - 1)
    }

    /// Prepare each output's lock screen that is missing or out of date, and while
    /// unlocked, the first frame a lock will show.
    pub(super) fn prepare(&mut self) {
        if self.painter.is_none() {
            return;
        }
        for output in self.outputs.outputs() {
            // An output without a logical size (no xdg-output) is prepared at lock time.
            let layout = self.layout(&output, None);
            if let Some(i) = layout.and_then(|layout| self.scene(&output, layout, ""))
                && self.lock.is_none()
            {
                self.frame(i, self.entry.look());
            }
        }
    }

    /// The index of a buffer of `scenes[i]` that shows `look` and that the compositor
    /// has released: one showing it already (such as the first frame, drawn ahead), or one
    /// drawn now; `None` while the compositor holds all `BUFFERS` of them.
    fn frame(&mut self, i: usize, look: Look) -> Option<usize> {
        let prepared = &mut self.scenes[i];
        let pool = &mut self.pool;
        let buffers = &mut prepared.buffers;
        let free = |pool: &mut SlotPool, buffer: &Buffer| buffer.canvas(pool).is_some();
        if let Some(at) =
            (0..buffers.len()).find(|&at| buffers[at].1 == Some(look) && free(pool, &buffers[at].0))
        {
            return Some(at);
        }
        let at = match (0..buffers.len()).find(|&at| free(pool, &buffers[at].0)) {
            Some(at) => at,
            None if buffers.len() < BUFFERS => {
                let (width, height) = (
                    prepared.scene.pixels.0 as i32,
                    prepared.scene.pixels.1 as i32,
                );
                match pool.create_buffer(width, height, width * 4, Format::Xrgb8888) {
                    Ok((buffer, _)) => buffers.push((buffer, None)),
                    Err(e) => {
                        error!("allocating a buffer for the lock screen: {e}");
                        return None;
                    }
                }
                buffers.len() - 1
            }
            None => return None,
        };
        let (buffer, shows) = &mut buffers[at];
        prepared
            .scene
            .draw(buffer.canvas(pool).expect("a released buffer"), look);
        *shows = Some(look);
        Some(at)
    }

    /// Draw `look` on the lock surface at `index`, once configured, with a buffer that
    /// shows it (the first time, normally the frame drawn ahead); if the compositor holds
    /// all of its output's buffers, once it releases one.
    fn draw(&mut self, index: usize, look: Look) {
        let Some(lock) = &self.lock else { return };
        let surface = &lock.surfaces[index];
        let (output, size) = (surface.output.clone(), surface.size);
        if size == (0, 0) {
            return;
        }
        let Some(layout) = self.layout(&output, Some(size)) else {
            return;
        };
        let Some(i) = self.scene(&output, layout, " at lock time") else {
            return;
        };
        if let (Status::Cooldown(seconds), Some(painter)) = (look.status, &self.painter) {
            painter.countdown(&mut self.scenes[i].scene, seconds);
        }
        let frame = self.frame(i, look);
        let Some(lock) = &mut self.lock else { return };
        let surface = &mut lock.surfaces[index];
        surface.waiting = frame.is_none();
        let Some(at) = frame else { return };
        let (buffer, _) = &self.scenes[i].buffers[at];
        let scene = &self.scenes[i].scene;
        let wl_surface = surface.lock_surface.wl_surface();
        if let Err(e) = buffer.attach_to(wl_surface) {
            return error!("drawing the lock screen: {e}");
        }
        surface.drawn_at = Some(scene.scale);
        match &surface.scaling {
            Some((_, viewport)) => viewport.set_destination(size.0 as i32, size.1 as i32),
            None => wl_surface.set_buffer_scale((scene.scale / 120) as i32),
        }
        let (width, height) = (scene.pixels.0 as i32, scene.pixels.1 as i32);
        wl_surface.damage_buffer(0, 0, width, height);
        wl_surface.commit();
        if !lock.drawn {
            lock.drawn = true;
            self.latencies
                .push(("lock screen drawn", lock.requested.elapsed()));
        }
    }

    /// The compositor prefers `scale` (in 120ths) for `surface`: draw its output at that
    /// scale, now and in later locks.
    fn prefer_scale(&mut self, surface: &WlSurface, scale: u32) {
        let Some(lock) = &mut self.lock else { return };
        let surfaces = lock.surfaces.iter();
        let Some(s) = surfaces
            .into_iter()
            .find(|s| s.lock_surface.wl_surface() == surface)
        else {
            return;
        };
        self.preferred.retain(|(output, _)| *output != s.output);
        self.preferred.push((s.output.clone(), scale));
        if s.drawn_at.is_some_and(|drawn| drawn != scale) {
            lock.shown = None;
        }
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
        let surfaces = lock.surfaces.iter_mut();
        let Some(i) = surfaces
            .into_iter()
            .position(|s| s.lock_surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        lock.surfaces[i].size = configure.new_size;
        self.draw(i, look);
    }
}

impl Dispatch<WpFractionalScaleV1, WlSurface> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        wl_event: wp_fractional_scale_v1::Event,
        surface: &WlSurface,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = wl_event {
            state.prefer_scale(surface, scale);
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
            Some(lock) => lock.add_surface(&self.compositor, self.scaling.as_ref(), output, qh),
            None => self.prepare(),
        }
    }

    /// The scale the compositor preferred may have changed with the output.
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.preferred.retain(|(o, _)| *o != output);
        self.prepare();
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        self.scenes.retain(|p| p.output != output);
        self.preferred.retain(|(o, _)| *o != output);
        if let Some(lock) = &mut self.lock {
            lock.surfaces.retain(|s| s.output != output);
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
