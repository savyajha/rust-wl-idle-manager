use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, bail};
use smithay_client_toolkit::output::OutputInfo;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockState, SessionLockSurface,
    SessionLockSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use tokio::time::Instant;
use tracing::{error, info, warn};
use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::WlKeyboard;
use wayland_client::protocol::wl_output::{Transform, WlOutput};
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use xkbcommon::xkb;

use super::State;
use crate::config::LockScreen;
use crate::draw::{self, Layout, Painter, Scene};
use crate::entry::{Entry, Look, Status};
use crate::policy::Input;
use crate::wallpaper::Blurred;

/// The most buffers an output's frames use: one the compositor shows, one to draw the next in.
const BUFFERS: usize = 2;

/// The built-in lock screen: its outputs, buffers, lock and keyboard.
pub struct Screen {
    painter: Painter,
    outputs: Vec<Output>,
    compositor: WlCompositor,
    scaling: Option<(WpFractionalScaleManagerV1, WpViewporter)>,
    shm: Shm,
    /// The memory lock surfaces draw into, kept from one lock to the next.
    pool: SlotPool,
    lock_manager: SessionLockState,
    lock: Option<Lock>,
    pub(super) entry: Entry,
    pub(super) submitted: bool,
    pub(super) xkb: xkb::Context,
    /// The keymap and modifiers, once the compositor has sent the keymap.
    pub(super) xkb_state: Option<xkb::State>,
    /// The first keyboard to appear, while the seat has one.
    pub(super) keyboard: Option<WlKeyboard>,
    /// The minute the clock was last rendered for.
    minute: i64,
    /// The lock's latencies, measured while dispatching and logged after the flush.
    drawn: Option<(&'static str, Duration)>,
    locked: Option<Duration>,
}

struct Lock {
    session: SessionLock,
    /// What the surfaces show of the password entry; `None` to draw them again.
    shown: Option<Look>,
    requested: Instant,
    /// logind asked to unlock before the compositor locked; unlock once it has.
    unlocking: bool,
}

struct Output {
    wl: WlOutput,
    info: Option<OutputInfo>,
    /// The scale (in 120ths) the compositor last preferred for its lock surface.
    preferred_scale: Option<u32>,
    prepared: Option<Prepared>,
    surface: Option<Surface>,
}

/// An output's lock screen, with the buffers its frames are drawn into. While unlocked, one
/// holds the first frame a lock shows, drawn ahead, so that a lock only attaches it.
struct Prepared {
    scene: Scene,
    buffers: Vec<Framebuffer>,
}

struct Framebuffer {
    buffer: Buffer,
    /// `None` once what it shows is out of date.
    shows: Option<Look>,
}

#[derive(Debug, PartialEq)]
enum Frame {
    Reused(usize),
    /// Drawn now; an index past the buffers is a new one.
    Painted(usize),
}

struct Surface {
    lock_surface: SessionLockSurface,
    /// Its logical size, once configured.
    size: Option<(i32, i32)>,
    drawn_at: Option<u32>,
    /// It is to be drawn again once the compositor releases one of its buffers.
    waiting: bool,
    scaling: Option<(WpFractionalScaleV1, WpViewport)>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        if let Some((fractional, viewport)) = self.scaling.take() {
            fractional.destroy();
            viewport.destroy();
        }
    }
}

impl State {
    /// The built-in lock screen, for events of objects only it creates.
    pub(super) fn screen(&mut self) -> &mut Screen {
        let screen = self.lock_screen.as_mut();
        screen.expect("only the lock screen creates these objects")
    }
}

impl Screen {
    pub(super) fn new(
        globals: &GlobalList,
        qh: &QueueHandle<State>,
        config: LockScreen,
    ) -> anyhow::Result<Self> {
        let can_lock = globals.contents().with_list(|list| {
            list.iter()
                .any(|global| global.interface == "ext_session_lock_manager_v1")
        });
        if !can_lock {
            bail!("the built-in lock screen needs ext-session-lock-v1, which the compositor lacks");
        }
        let shm = Shm::bind(globals, qh).context("binding wl_shm")?;
        Ok(Self {
            painter: Painter::new(config),
            outputs: Vec::new(),
            compositor: globals
                .bind(qh, 1..=4, ())
                .context("binding wl_compositor")?,
            scaling: globals
                .bind(qh, 1..=1, ())
                .ok()
                .zip(globals.bind(qh, 1..=1, ()).ok()),
            // Grown as the lock screen is prepared for each output.
            pool: SlotPool::new(1, &shm).context("creating the lock screen's buffer pool")?,
            shm,
            lock_manager: SessionLockState::new(globals, qh),
            lock: None,
            entry: Entry::new(),
            submitted: false,
            xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            keyboard: None,
            minute: draw::minute(),
            drawn: None,
            locked: None,
        })
    }

    pub(super) fn lock(&mut self, qh: &QueueHandle<State>) -> anyhow::Result<()> {
        if let Some(lock) = &mut self.lock {
            lock.unlocking = false;
            return Ok(());
        }
        self.entry.reset();
        self.lock = Some(Lock {
            session: self
                .lock_manager
                .lock(qh)
                .context("requesting a session lock")?,
            shown: None,
            requested: Instant::now(),
            unlocking: false,
        });
        for i in 0..self.outputs.len() {
            self.add_surface(i, qh);
        }
        Ok(())
    }

    /// Unlock, or if the compositor has not locked yet, once it has.
    pub(super) fn unlock(&mut self, inputs: &mut VecDeque<Input>) {
        match &mut self.lock {
            None => info!("not locked; nothing to unlock"),
            // Destroying the lock now would be a protocol error if `locked` is on its way.
            Some(lock) if !lock.session.is_locked() => {
                info!("unlock deferred until locked");
                lock.unlocking = true;
            }
            Some(_) => self.end_lock(inputs),
        }
    }

    /// Read GTK's colors again, and show `wallpaper` from now on if there is one.
    pub(super) fn reload(&mut self, wallpaper: Option<Blurred>) {
        self.painter.reload(wallpaper);
        for output in &mut self.outputs {
            output.prepared = None;
        }
        self.prepare();
        if let Some(lock) = &mut self.lock {
            lock.shown = None;
        }
    }

    pub(super) fn is_locked(&self) -> bool {
        self.lock.is_some()
    }

    /// When the loop must wake for the entry's timers or the clock's next minute.
    pub(super) fn next_wake(&self) -> Instant {
        let now = Instant::now();
        let minute = now + draw::until_next_minute();
        self.entry.deadline(now).map_or(minute, |at| at.min(minute))
    }

    /// The index of `wl` in `outputs`, added if missing, with its `info` updated.
    fn output(&mut self, wl: WlOutput, info: Option<OutputInfo>) -> usize {
        let i = match self.outputs.iter().position(|output| output.wl == wl) {
            Some(i) => i,
            None => {
                self.outputs.push(Output {
                    wl,
                    info: None,
                    preferred_scale: None,
                    prepared: None,
                    surface: None,
                });
                self.outputs.len() - 1
            }
        };
        self.outputs[i].info = info;
        i
    }

    fn surface(&self, wl_surface: &WlSurface) -> Option<usize> {
        self.outputs.iter().position(|output| {
            let surface = output.surface.as_ref();
            surface.is_some_and(|surface| surface.lock_surface.wl_surface() == wl_surface)
        })
    }

    fn add_surface(&mut self, i: usize, qh: &QueueHandle<State>) {
        let (Some(lock), output) = (&self.lock, &mut self.outputs[i]) else {
            return;
        };
        if output.surface.is_some() {
            return;
        }
        let surface = self.compositor.create_surface(qh, ());
        let scaling = self.scaling.as_ref().map(|(fractional, viewporter)| {
            (
                fractional.get_fractional_scale(&surface, qh, surface.clone()),
                viewporter.get_viewport(&surface, qh, ()),
            )
        });
        let lock_surface = lock.session.create_lock_surface(surface, &output.wl, qh);
        output.surface = Some(Surface {
            lock_surface,
            size: None,
            drawn_at: None,
            waiting: false,
            scaling,
        });
    }

    /// End the lock, if there is one, and prepare the next one's first frames in a new pool,
    /// since a pool cannot shrink.
    fn end_lock(&mut self, inputs: &mut VecDeque<Input>) {
        if let Some(lock) = self.lock.take() {
            // unlock_and_destroy if locked; otherwise dropping it destroys it, as it must be.
            lock.session.unlock();
            inputs.push_back(Input::Locked(false));
        }
        for output in &mut self.outputs {
            output.surface = None;
        }
        self.entry.reset();
        let prepared = self.outputs.iter().filter_map(|o| o.prepared.as_ref());
        let len: i32 = prepared
            .map(|p| p.scene.layout.pixels)
            .map(|(w, h)| w * h * 4)
            .sum();
        match SlotPool::new(len.max(1) as usize, &self.shm) {
            Ok(pool) => {
                for output in &mut self.outputs {
                    if let Some(prepared) = &mut output.prepared {
                        prepared.buffers.clear();
                    }
                }
                self.pool = pool;
            }
            Err(e) => error!("creating a new buffer pool: {e}"),
        }
        self.prepare();
    }

    /// Draw each lock surface again if the entry's look changed, or if it waits for a buffer.
    pub(super) fn redraw(&mut self) {
        let look = self.entry.look(Instant::now());
        let Some(lock) = &mut self.lock else { return };
        let changed = lock.shown.replace(look) != Some(look);
        let due: Vec<_> = (0..self.outputs.len())
            .filter(|&i| {
                let surface = self.outputs[i].surface.as_ref();
                surface.is_some_and(|surface| changed || surface.waiting)
            })
            .collect();
        for i in due {
            self.draw(i, look);
        }
    }

    /// Render the date and clock again once the minute changes; returns whether any did.
    pub(super) fn refresh_clock(&mut self) -> bool {
        let minute = draw::minute();
        if self.minute == minute {
            return false;
        }
        self.minute = minute;
        let mut changed = false;
        for prepared in self.outputs.iter_mut().filter_map(|o| o.prepared.as_mut()) {
            if self.painter.refresh(&mut prepared.scene) {
                for framebuffer in &mut prepared.buffers {
                    framebuffer.shows = None;
                }
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

    pub(super) fn log_latencies(&mut self) {
        let ms = |after: Duration| after.as_secs_f64() * 1000.0;
        if let Some((how, after)) = self.drawn.take() {
            info!(
                "lock screen drawn ({how}) {:.3} ms after the request",
                ms(after)
            );
        }
        if let Some(after) = self.locked.take() {
            info!("locked {:.3} ms after the request", ms(after));
        }
    }

    /// How output `i` is drawn: at its lock surface's size, or else the output's, and at the
    /// scale the compositor last preferred, or until it has said, the one it likely will.
    fn layout(&self, i: usize) -> Option<Layout> {
        let output = &self.outputs[i];
        let info = output.info.as_ref()?;
        let size = output.surface.as_ref().and_then(|surface| surface.size);
        let logical = size.or(info.logical_size)?;
        let sideways = matches!(
            info.transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        );
        let mode = info.modes.iter().find(|mode| mode.current).map(|mode| {
            let (w, h) = mode.dimensions;
            if sideways { (h, w) } else { (w, h) }
        });
        let scale = match (&self.scaling, output.preferred_scale, mode) {
            (None, ..) => info.scale_factor.max(1) as u32 * 120,
            (Some(_), Some(scale), _) => scale,
            (Some(_), None, Some(mode)) => draw::scale_of(mode.0, logical.0),
            (Some(_), None, None) => 120,
        };
        let mode = mode.filter(|_| self.scaling.is_some());
        Some(Layout::new(logical, scale, mode))
    }

    /// Prepare output `i`'s lock screen, unless it is prepared for its layout already.
    /// Returns whether it is prepared.
    fn prepare_output(&mut self, i: usize, at_lock_time: bool) -> bool {
        let Some(layout) = self.layout(i) else {
            return false;
        };
        let output = &mut self.outputs[i];
        if output
            .prepared
            .as_ref()
            .is_some_and(|p| p.scene.layout == layout)
        {
            return true;
        }
        let started = Instant::now();
        let scene = self.painter.scene(layout).unwrap_or_else(|e| {
            error!("preparing the lock screen: {e}; drawing it plain");
            self.painter.plain(layout)
        });
        let Layout {
            logical,
            scale,
            pixels,
        } = layout;
        let when = if at_lock_time { " at lock time" } else { "" };
        info!(
            "lock screen prepared{when} for {}×{} at scale {} ({}×{} px) in {:.1} ms",
            logical.0,
            logical.1,
            f64::from(scale) / 120.0,
            pixels.0,
            pixels.1,
            started.elapsed().as_secs_f64() * 1000.0
        );
        output.prepared = Some(Prepared {
            scene,
            buffers: Vec::new(),
        });
        true
    }

    /// Prepare each output's lock screen, and while unlocked, the first frame a lock shows.
    fn prepare(&mut self) {
        for i in 0..self.outputs.len() {
            // An output without a logical size (no xdg-output) is prepared at lock time.
            if self.prepare_output(i, false) && self.lock.is_none() {
                self.frame_showing(i, self.entry.look(Instant::now()));
            }
        }
    }

    /// A buffer of output `i` that shows `look`, drawn now unless one shows it already.
    fn frame_showing(&mut self, i: usize, look: Look) -> Option<Frame> {
        let prepared = self.outputs[i].prepared.as_mut()?;
        let pool = &mut self.pool;
        let shows: Vec<_> = prepared.buffers.iter().map(|b| b.shows).collect();
        let released = |at: usize| prepared.buffers[at].buffer.canvas(pool).is_some();
        let frame = choose_buffer(&shows, released, look)?;
        let Frame::Painted(at) = frame else {
            return Some(frame);
        };
        if at == prepared.buffers.len() {
            let (width, height) = prepared.scene.layout.pixels;
            let created = pool.create_buffer(width, height, width * 4, Format::Xrgb8888);
            let (buffer, _) = created
                .inspect_err(|e| error!("allocating a buffer for the lock screen: {e}"))
                .ok()?;
            prepared.buffers.push(Framebuffer {
                buffer,
                shows: None,
            });
        }
        let framebuffer = &mut prepared.buffers[at];
        let canvas = framebuffer.buffer.canvas(pool).expect("a released buffer");
        prepared.scene.draw(canvas, look);
        framebuffer.shows = Some(look);
        Some(frame)
    }

    /// Draw `look` on output `i`'s configured lock surface, or once it releases a buffer.
    fn draw(&mut self, i: usize, look: Look) {
        let configured = self.outputs[i].surface.as_ref().and_then(|s| s.size);
        if configured.is_none() || !self.prepare_output(i, true) {
            return;
        }
        if let (Status::Cooldown(seconds), Some(prepared)) =
            (look.status, &mut self.outputs[i].prepared)
        {
            self.painter.countdown(&mut prepared.scene, seconds);
        }
        let first =
            (self.outputs.iter()).all(|o| o.surface.as_ref().is_none_or(|s| s.drawn_at.is_none()));
        let frame = self.frame_showing(i, look);
        let (Some(lock), output) = (&self.lock, &mut self.outputs[i]) else {
            return;
        };
        let (Some(surface), Some(prepared)) = (&mut output.surface, &output.prepared) else {
            return;
        };
        surface.waiting = frame.is_none();
        let Some(frame) = frame else { return };
        let (Frame::Reused(at) | Frame::Painted(at)) = frame;
        let layout = prepared.scene.layout;
        let wl_surface = surface.lock_surface.wl_surface();
        if let Err(e) = prepared.buffers[at].buffer.attach_to(wl_surface) {
            error!("drawing the lock screen: {e}");
            return;
        }
        surface.drawn_at = Some(layout.scale);
        match &surface.scaling {
            Some((_, viewport)) => viewport.set_destination(layout.logical.0, layout.logical.1),
            None => wl_surface.set_buffer_scale(layout.scale as i32 / 120),
        }
        wl_surface.damage_buffer(0, 0, layout.pixels.0, layout.pixels.1);
        wl_surface.commit();
        if first {
            let how = match frame {
                Frame::Reused(_) => "attached",
                Frame::Painted(_) => "painted",
            };
            self.drawn = Some((how, lock.requested.elapsed()));
        }
    }

    /// The compositor prefers `scale` (in 120ths) for `surface`, now and in later locks.
    fn prefer_scale(&mut self, surface: &WlSurface, scale: u32) {
        let i = self.surface(surface);
        let (Some(lock), Some(i)) = (&mut self.lock, i) else {
            return;
        };
        let output = &mut self.outputs[i];
        output.preferred_scale = Some(scale);
        let drawn_at = output.surface.as_ref().and_then(|s| s.drawn_at);
        if drawn_at.is_some_and(|drawn| drawn != scale) {
            lock.shown = None;
        }
    }
}

/// The buffer for a frame showing `look`: a released one that shows it already, else any
/// released one, else a new one up to `BUFFERS`; `None` while the compositor holds them all.
fn choose_buffer(
    shows: &[Option<Look>],
    mut released: impl FnMut(usize) -> bool,
    look: Look,
) -> Option<Frame> {
    if let Some(at) = (0..shows.len()).find(|&at| shows[at] == Some(look) && released(at)) {
        return Some(Frame::Reused(at));
    }
    match (0..shows.len()).find(|&at| released(at)) {
        Some(at) => Some(Frame::Painted(at)),
        None if shows.len() < BUFFERS => Some(Frame::Painted(shows.len())),
        None => None,
    }
}

impl SessionLockHandler for State {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        let State {
            inputs,
            lock_screen: Some(screen),
            ..
        } = self
        else {
            return;
        };
        let Some(lock) = &screen.lock else { return };
        let unlocking = lock.unlocking;
        screen.locked = Some(lock.requested.elapsed());
        if unlocking {
            screen.end_lock(inputs);
        } else {
            inputs.push_back(Input::Locked(true));
        }
    }

    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        warn!("the compositor refused or ended our lock; another lock screen may hold it");
        let State {
            inputs,
            lock_screen: Some(screen),
            ..
        } = self
        else {
            return;
        };
        screen.end_lock(inputs);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let screen = self.screen();
        let look = screen.entry.look(Instant::now());
        let Some(lock) = &mut screen.lock else { return };
        lock.shown = Some(look);
        let Some(i) = screen.surface(surface.wl_surface()) else {
            return;
        };
        let (width, height) = configure.new_size;
        if let Some(surface) = &mut screen.outputs[i].surface {
            surface.size = Some((width as i32, height as i32));
        }
        screen.draw(i, look);
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
            state.screen().prefer_scale(surface, scale);
        }
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    /// A new output gets a lock surface while locked; until then the compositor blanks it.
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, wl: WlOutput) {
        let info = self.output_state.info(&wl);
        let Some(screen) = &mut self.lock_screen else {
            return;
        };
        let i = screen.output(wl, info);
        if screen.lock.is_some() {
            screen.add_surface(i, qh);
        } else {
            screen.prepare();
        }
    }

    /// The scale the compositor preferred may have changed with the output.
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, wl: WlOutput) {
        let info = self.output_state.info(&wl);
        let Some(screen) = &mut self.lock_screen else {
            return;
        };
        let i = screen.output(wl, info);
        screen.outputs[i].preferred_scale = None;
        screen.prepare();
    }

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, wl: WlOutput) {
        if let Some(screen) = &mut self.lock_screen {
            screen.outputs.retain(|output| output.wl != wl);
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.screen().shm
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

#[cfg(test)]
mod tests {
    use super::*;

    const TYPING: Look = Look {
        status: Status::Typing,
        chars: 1,
        caps_lock: false,
    };
    const MORE: Look = Look { chars: 2, ..TYPING };

    #[test]
    fn a_frame_reuses_a_released_buffer_that_shows_it() {
        let shows = [Some(MORE), Some(TYPING)];
        assert_eq!(
            choose_buffer(&shows, |_| true, TYPING),
            Some(Frame::Reused(1))
        );
        // One the compositor holds is painted over in the other.
        assert_eq!(
            choose_buffer(&shows, |at| at == 0, TYPING),
            Some(Frame::Painted(0))
        );
    }

    #[test]
    fn a_second_buffer_is_added_then_frames_wait_for_a_release() {
        assert_eq!(
            choose_buffer(&[], |_| true, TYPING),
            Some(Frame::Painted(0))
        );
        let shows = [Some(TYPING)];
        assert_eq!(
            choose_buffer(&shows, |_| false, MORE),
            Some(Frame::Painted(1))
        );
        let shows = [Some(TYPING), Some(MORE)];
        assert_eq!(choose_buffer(&shows, |_| false, MORE), None);
        assert_eq!(choose_buffer(&shows, |_| false, TYPING), None);
    }
}
