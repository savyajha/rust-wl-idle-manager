//! The display: one GPU, an output per connected connector, each drawn by the Pixman renderer
//! into two DRM dumb buffers that are page-flipped on vblank. No GBM, EGL or GL.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::{
    DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEventMetadata, DrmEventTime, DrmSurface,
    PlaneClaim, PlaneConfig, PlaneState,
};
use smithay::backend::renderer::Bind;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::pixman::{PixmanError, PixmanRenderer};
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::desktop::space::render_output;
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode as OutputMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::drm::control::{
    Device as _, Mode, ModeTypeFlags, ResourceHandles, connector, crtc, dumbbuffer::DumbBuffer,
    framebuffer, plane,
};
use smithay::reexports::drm::{Device as _, DriverCapability};
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::reexports::{pixman, rustix};
use smithay::utils::{DeviceFd, Logical, Monotonic, Physical, Point, Rectangle, Size, Transform};
use smithay::wayland::presentation::Refresh;
use tracing::{info, warn};

use crate::{State, cursor};

/// The background where no client draws.
const CLEAR: [f32; 4] = [0.12, 0.12, 0.16, 1.0];

/// A DRM dumb buffer with a framebuffer, mapped while it lives. An output's frame buffers
/// (XRGB) also get a Pixman image over the mapping, which the renderer draws into.
struct Dumb {
    image: Option<pixman::Image<'static, 'static>>,
    /// The frame (`Head::frames`) last drawn into it, for its age.
    drawn_at: Option<u64>,
    fd: DrmDeviceFd,
    buffer: Option<DumbBuffer>,
    fb: framebuffer::Handle,
    ptr: *mut u8,
    len: usize,
    pitch: usize,
}

impl Dumb {
    fn new(fd: &DrmDeviceFd, (w, h): (u32, u32), format: Fourcc) -> anyhow::Result<Dumb> {
        let mut buffer = fd.create_dumb_buffer((w, h), format, 32)?;
        let depth = if format == Fourcc::Argb8888 { 32 } else { 24 };
        let fb = fd.add_framebuffer(&buffer, depth, 32)?;
        let pitch = smithay::reexports::drm::buffer::Buffer::pitch(&buffer) as usize;
        let mut mapping = fd.map_dumb_buffer(&mut buffer)?;
        let (ptr, len) = (mapping.as_mut_ptr(), mapping.len());
        // Unmapped in Drop.
        std::mem::forget(mapping);
        let image = (format == Fourcc::Xrgb8888).then(|| {
            let code = pixman::FormatCode::X8R8G8B8;
            // SAFETY: the mapping is pitch * h bytes; the image is dropped before it.
            unsafe { pixman::Image::from_raw_mut(code, w as _, h as _, ptr.cast(), pitch, false) }
        });
        Ok(Dumb {
            image: image
                .transpose()
                .map_err(|_| anyhow!("pixman image over a dumb buffer"))?,
            drawn_at: None,
            fd: fd.clone(),
            buffer: Some(buffer),
            fb,
            ptr,
            len,
            pitch,
        })
    }

    /// The mapping, `pitch` bytes a row, to write into: only for a buffer with no image (the
    /// cursor's), which nothing else points into.
    fn bytes_mut(&mut self) -> &mut [u8] {
        assert!(self.image.is_none(), "drawn through its image");
        // SAFETY: `ptr` is the read-write mapping of `len` bytes made in `new`, unmapped only
        // in Drop. Without an image nothing else points into it, and the borrow of `self`
        // makes the slice the only access while it lives.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Dumb {
    fn drop(&mut self) {
        self.image = None;
        // SAFETY: the mapping made in `new`, used by nothing else any more.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
        let _ = self.fd.destroy_framebuffer(self.fb);
        if let Some(buffer) = self.buffer.take() {
            let _ = self.fd.destroy_dumb_buffer(buffer);
        }
    }
}

/// The pointer's image, on an output's cursor plane.
struct CursorPlane {
    plane: plane::Handle,
    _claim: PlaneClaim,
    dumb: Dumb,
    size: (u32, u32),
    /// Where it was last committed (None: hidden).
    shown: Option<Point<i32, Physical>>,
}

/// An output: a connector driven by a CRTC.
pub struct Head {
    pub output: Output,
    connector: connector::Handle,
    /// The connector's preferred mode, which the output runs at.
    mode: Mode,
    crtc: crtc::Handle,
    surface: DrmSurface,
    buffers: [Dumb; 2],
    /// The buffer the next frame is drawn into.
    back: usize,
    /// How many frames have been drawn, for buffer ages.
    frames: u64,
    damage: OutputDamageTracker,
    /// When the flip being waited for was submitted (CLOCK_MONOTONIC).
    flip_pending: Option<Duration>,
    /// Something changed while a flip was pending: draw again on the next vblank.
    redraw: bool,
    /// A frame timer is set: frame callbacks, and the redraw if due, a refresh from now.
    frame_timer: bool,
    /// The last flip failed (logged once until one succeeds).
    flip_failed: bool,
    modeset_done: bool,
    /// The presentation feedback of the frame being flipped.
    feedback: Option<OutputPresentationFeedback>,
    /// Whether the frame being flipped shows a client surface.
    client_in_flip: bool,
    refresh: Duration,
    /// None without a usable cursor plane: then the cursor is drawn into the frame instead.
    /// (Legacy KMS, and virtual GPUs, which hide cursor planes from clients that cannot set
    /// a hotspot, as Smithay cannot.)
    cursor_plane: Option<CursorPlane>,
    /// The cursor drawn into the frame, and its logical size there.
    soft_cursor: (MemoryRenderBuffer, Size<i32, Logical>),
    /// The pixel of the cursor's image that points, in either.
    hotspot: Point<i32, Physical>,
    global: GlobalId,
}

pub struct Gpu {
    pub path: PathBuf,
    pub dev_id: libc::dev_t,
    fd: DrmDeviceFd,
    pub drm: DrmDevice,
    /// In layout order, left to right; the first is the primary output.
    pub heads: Vec<Head>,
    cursor_size: (u32, u32),
}

impl Gpu {
    /// Opens `path`, or else the seat's boot GPU (udev's `boot_vga`). No other GPU is opened.
    pub fn open(
        mut session: LibSeatSession,
        path: Option<PathBuf>,
    ) -> anyhow::Result<(Gpu, DrmDeviceNotifier)> {
        let path = match path {
            Some(path) => path,
            None => smithay::backend::udev::primary_gpu(session.seat())?
                .context("no GPU on this seat")?,
        };
        use rustix::fs::OFlags;
        let fd = session
            .open(
                &path,
                OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
            )
            .map_err(|e| anyhow!("opening {}: {e}", path.display()))?;
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));
        let (drm, events) = DrmDevice::new(fd.clone(), false)
            .with_context(|| format!("{} as a DRM device", path.display()))?;
        let cap = |cap| fd.get_driver_capability(cap).map_or(64, |v| v as u32);
        let gpu = Gpu {
            dev_id: fd.dev_id()?,
            cursor_size: (
                cap(DriverCapability::CursorWidth),
                cap(DriverCapability::CursorHeight),
            ),
            path,
            fd,
            drm,
            heads: Vec::new(),
        };
        Ok((gpu, events))
    }
}

impl State {
    /// Matches the outputs to the GPU's connected connectors: an output for each new one, none
    /// for each one gone; one whose preferred mode changed is replaced. With `probe` the kernel
    /// probes the connectors (reads their modes) first, as after a hotplug it does not do so by
    /// itself while the device has a master; at start-up its state from boot is used, which is
    /// faster. Returns whether anything changed; the caller draws.
    pub fn scan_connectors(&mut self, probe: bool) -> bool {
        self.try_scan_connectors(probe)
            .inspect_err(|e| warn!("scanning connectors: {e:#}"))
            .unwrap_or(false)
    }

    fn try_scan_connectors(&mut self, probe: bool) -> anyhow::Result<bool> {
        let fd = self.gpu.fd.clone();
        let resources = fd.resource_handles()?;
        let mut connected: Vec<connector::Info> = resources
            .connectors()
            .iter()
            .filter_map(|&c| fd.get_connector(c, probe).ok())
            .filter(|c| c.state() == connector::State::Connected && !c.modes().is_empty())
            .collect();
        // A built-in panel first: it becomes the primary output.
        connected.sort_by_key(|c| {
            use connector::Interface::*;
            !matches!(c.interface(), EmbeddedDisplayPort | LVDS | DSI)
        });

        let mut changed = false;
        while let Some(i) = self.gpu.heads.iter().position(|h| {
            let conn = connected.iter().find(|c| c.handle() == h.connector);
            conn.is_none_or(|c| preferred_mode(c) != h.mode)
        }) {
            self.remove_head(i);
            changed = true;
        }
        for conn in &connected {
            if !self.gpu.heads.iter().any(|h| h.connector == conn.handle()) {
                match self.add_head(&resources, conn) {
                    Ok(()) => changed = true,
                    Err(e) => warn!(
                        "{}-{}: {e:#}",
                        conn.interface().as_str(),
                        conn.interface_id()
                    ),
                }
            }
        }
        if changed {
            self.relayout();
        }
        Ok(changed)
    }

    fn add_head(
        &mut self,
        resources: &ResourceHandles,
        conn: &connector::Info,
    ) -> anyhow::Result<()> {
        let fd = &self.gpu.fd;
        let name = format!("{}-{}", conn.interface().as_str(), conn.interface_id());
        let mode = preferred_mode(conn);
        let crtc = conn
            .encoders()
            .iter()
            .filter_map(|&e| fd.get_encoder(e).ok())
            .flat_map(|e| resources.filter_crtcs(e.possible_crtcs()))
            .find(|crtc| !self.gpu.heads.iter().any(|h| h.crtc == *crtc))
            .context("no free CRTC")?;
        let surface = self.gpu.drm.create_surface(crtc, mode, &[conn.handle()])?;
        let (w, h) = mode.size();
        let buffers = [
            Dumb::new(fd, (w.into(), h.into()), Fourcc::Xrgb8888)?,
            Dumb::new(fd, (w.into(), h.into()), Fourcc::Xrgb8888)?,
        ];
        let scale = self
            .scales
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(1.0, |(_, s)| *s);
        let image = cursor::pick(&self.cursor_theme, self.cursor_size, scale);
        let cursor_plane = cursor_plane(&self.gpu, &surface, &image);

        let output = Output::new(
            name.clone(),
            PhysicalProperties {
                size: conn
                    .size()
                    .map(|(w, h)| (w as i32, h as i32))
                    .unwrap_or_default()
                    .into(),
                subpixel: Subpixel::Unknown,
                make: "Unknown".into(),
                model: name.clone(),
            },
        );
        let output_mode = OutputMode::from(mode);
        output.change_current_state(
            Some(output_mode),
            Some(Transform::Normal),
            Some(Scale::Fractional(scale)),
            None,
        );
        output.set_preferred(output_mode);
        let global = output.create_global::<State>(&self.dh);
        info!(
            "output {name}: {w}x{h}@{} Hz, scale {scale}, {} cursor",
            mode.vrefresh(),
            if cursor_plane.is_some() {
                "hardware"
            } else {
                "software"
            }
        );
        self.gpu.heads.push(Head {
            damage: OutputDamageTracker::from_output(&output),
            output,
            connector: conn.handle(),
            mode,
            crtc,
            surface,
            buffers,
            back: 0,
            frames: 0,
            flip_pending: None,
            redraw: false,
            frame_timer: false,
            flip_failed: false,
            modeset_done: false,
            feedback: None,
            client_in_flip: false,
            refresh: Duration::from_nanos(1_000_000_000_000 / output_mode.refresh.max(1) as u64),
            cursor_plane,
            soft_cursor: soft_cursor(&image, scale),
            hotspot: (image.hotspot.0 as i32, image.hotspot.1 as i32).into(),
            global,
        });
        Ok(())
    }

    fn remove_head(&mut self, i: usize) {
        let head = self.gpu.heads.remove(i);
        info!("output {} disconnected", head.output.name());
        // Its toplevels leave it (unmapped, which sends wl_surface.leave) and go to the
        // primary output in `relayout`, beneath that output's own (they lose the keyboard);
        // a popup grab on one of them is dismissed.
        let grab_parent = self.grab.first().and_then(|p| p.get_parent_surface());
        let mut dismiss = false;
        for t in &mut self.toplevels {
            if t.output.as_ref() == Some(&head.output) {
                self.space.unmap_elem(&t.window);
                t.output = None;
                t.moved = true;
                if self.focus.as_ref() == Some(&t.window) {
                    self.focus = None;
                }
                dismiss |= grab_parent.as_ref() == t.window.toplevel().map(|t| t.wl_surface());
            }
        }
        if dismiss {
            self.dismiss_popups(true);
        }
        self.space.unmap_output(&head.output);
        // Disabled now, removed once clients had time to see that.
        self.dh.disable_global::<State>(head.global.clone());
        let global = head.global.clone();
        let _ = self.handle.insert_source(
            Timer::from_duration(Duration::from_secs(5)),
            move |_, _, state| {
                state.dh.remove_global::<State>(global.clone());
                TimeoutAction::Drop
            },
        );
    }

    /// Outputs side by side, in order; every toplevel fullscreen on its output, the pointer
    /// on an output.
    fn relayout(&mut self) {
        let mut x = 0;
        for head in &self.gpu.heads {
            head.output
                .change_current_state(None, None, None, Some((x, 0).into()));
            self.space.map_output(&head.output, (x, 0));
            x += self
                .space
                .output_geometry(&head.output)
                .map_or(0, |g| g.size.w);
        }
        // Not those still before their initial commit: that commit configures them.
        for i in 0..self.toplevels.len() {
            if self.toplevels[i].surface().is_initial_configure_sent() {
                self.place(i);
            }
        }
        self.pointer = self.clamp_to_layout(self.pointer);
        self.update_pointer_focus(self.started.elapsed().as_millis() as u32);
    }

    pub fn render_all(&mut self) {
        for i in 0..self.gpu.heads.len() {
            self.render(i);
        }
    }

    /// Where the cursor's image goes on an output, in its physical pixels (its hotspot at the
    /// pointer); None where it doesn't show. (The logical size is rounded up, so the last
    /// logical pixel can reach past the mode.)
    fn cursor_position(&self, head: &Head) -> Option<Point<i32, Physical>> {
        let geometry = self.space.output_geometry(&head.output)?;
        (self.cursor_visible && geometry.to_f64().contains(self.pointer)).then(|| {
            let p = (self.pointer - geometry.loc.to_f64())
                .to_physical(head.output.current_scale().fractional_scale())
                .to_i32_floor::<i32>();
            let (w, h) = head.mode.size();
            Point::from((p.x.min(w as i32 - 1), p.y.min(h as i32 - 1))) - head.hotspot
        })
    }

    /// Draws output `i`'s damage into its back buffer and flips to it.
    pub fn render(&mut self, i: usize) {
        // Outputs other than the primary one wait for the greeter's first frame (or input,
        // opened without one after 1 s): their first commit is a blocking modeset.
        let waiting = i > 0 && !self.first_frame && self.libinput.is_none();
        if waiting && !self.gpu.heads[i].modeset_done {
            return;
        }
        if !self.session.is_active() || self.gpu.heads[i].flip_pending.is_some() {
            self.gpu.heads[i].redraw = true;
            return;
        }
        let cursor = self.cursor_position(&self.gpu.heads[i]);
        let head = &mut self.gpu.heads[i];
        head.redraw = false;

        let scale = head.output.current_scale().fractional_scale();
        let soft_cursor = match (&head.cursor_plane, cursor) {
            (None, Some(position)) => {
                soft_cursor_element(&mut self.renderer, &head.soft_cursor, position, scale)
                    .inspect_err(|e| warn!("cursor: {e}"))
                    .ok()
            }
            _ => None,
        };
        let buffer = &mut head.buffers[head.back];
        head.frames += 1;
        let age = buffer.drawn_at.map_or(0, |at| (head.frames - at) as usize);
        let damaged = {
            let image = buffer.image.as_mut().expect("frame buffers are XRGB");
            let mut target = match self.renderer.bind(image) {
                Ok(target) => target,
                Err(e) => return warn!("binding a buffer: {e}"),
            };
            match render_output(
                &head.output,
                &mut self.renderer,
                &mut target,
                1.0,
                age,
                [&self.space],
                soft_cursor.as_slice(),
                &mut head.damage,
                CLEAR,
            ) {
                Ok(result) => result.damage.is_some(),
                Err(e) => return warn!("drawing {}: {e}", head.output.name()),
            }
        };
        buffer.drawn_at = Some(head.frames);

        let cursor_moved = head
            .cursor_plane
            .as_ref()
            .is_some_and(|c| c.shown != cursor);
        // After a failed flip the frame is drawn but not shown: flipped again even undamaged.
        if !damaged && !cursor_moved && head.modeset_done && !head.flip_failed {
            // Nothing to show: no flip, but the clients may be waiting for a frame callback.
            self.schedule_frame(i);
            return;
        }

        let mut feedback = OutputPresentationFeedback::new(&head.output);
        let mut client = false;
        for window in self.space.elements_for_output(&head.output) {
            client = true;
            window.take_presentation_feedback(
                &mut feedback,
                |_, _| Some(head.output.clone()),
                |_, _| wp_presentation_feedback::Kind::empty(),
            );
        }

        let (w, h) = head.surface.pending_mode().size();
        let mut planes = vec![PlaneState {
            handle: head.surface.plane(),
            config: Some(PlaneConfig {
                src: Rectangle::from_size((w as f64, h as f64).into()),
                dst: Rectangle::from_size((w as i32, h as i32).into()),
                transform: Transform::Normal,
                alpha: 1.0,
                damage_clips: None,
                fb: buffer.fb,
                fence: None,
            }),
        }];
        if let Some(c) = &head.cursor_plane {
            planes.push(PlaneState {
                handle: c.plane,
                config: cursor.map(|position| PlaneConfig {
                    src: Rectangle::from_size((c.size.0 as f64, c.size.1 as f64).into()),
                    dst: Rectangle::new(position, (c.size.0 as i32, c.size.1 as i32).into()),
                    transform: Transform::Normal,
                    alpha: 1.0,
                    damage_clips: None,
                    fb: c.dumb.fb,
                    fence: None,
                }),
            });
        }
        let submitted = now();
        let result = if head.modeset_done {
            head.surface.page_flip(planes, true)
        } else {
            head.surface.commit(planes, true)
        };
        match result {
            Ok(()) => {
                head.modeset_done = true;
                head.flip_pending = Some(submitted);
                head.flip_failed = false;
                head.back ^= 1;
                head.feedback = Some(feedback);
                head.client_in_flip = client;
                if let Some(c) = &mut head.cursor_plane {
                    c.shown = cursor;
                }
                self.watch_flip(i, submitted);
            }
            Err(e) => {
                feedback.discarded();
                if !std::mem::replace(&mut head.flip_failed, true) {
                    warn!(
                        "flipping {}: {e}; trying again every frame",
                        head.output.name()
                    );
                }
                // The cursor plane is not tested in the VM test (none there), so it must not
                // be able to stop the frames: switched off, and drawn into them from now on.
                if cursor.is_some()
                    && let Some(c) = head.cursor_plane.take()
                {
                    warn!("{}: drawing the cursor instead", head.output.name());
                    if let Err(e) = head.surface.clear_plane(c.plane) {
                        warn!(
                            "{}: switching the cursor plane off: {e}",
                            head.output.name()
                        );
                    }
                }
                head.redraw = true;
                self.schedule_frame(i);
            }
        }
    }

    /// If the flip submitted at `submitted` has no accepted vblank event within four refresh
    /// intervals, it is taken as done: an event dropped as stale must not stop the output.
    fn watch_flip(&mut self, i: usize, submitted: Duration) {
        let head = &self.gpu.heads[i];
        let connector = head.connector;
        let timer = Timer::from_duration(head.refresh * 4);
        let _ = self.handle.insert_source(timer, move |_, _, state| {
            if let Some(i) = state
                .gpu
                .heads
                .iter()
                .position(|h| h.connector == connector && h.flip_pending == Some(submitted))
            {
                warn!("no vblank for {}'s flip", state.gpu.heads[i].output.name());
                state.gpu.heads[i].flip_pending = None;
                state.send_frames(i);
                state.render(i);
            }
            TimeoutAction::Drop
        });
    }

    /// Frame callbacks for output `i`, and its redraw if one is due, a refresh interval from
    /// now: when no flip paces them, so that a client drawing on every callback does not spin.
    fn schedule_frame(&mut self, i: usize) {
        let head = &mut self.gpu.heads[i];
        if std::mem::replace(&mut head.frame_timer, true) {
            return;
        }
        let connector = head.connector;
        let timer = Timer::from_duration(head.refresh);
        let _ = self.handle.insert_source(timer, move |_, _, state| {
            if let Some(i) = state
                .gpu
                .heads
                .iter()
                .position(|h| h.connector == connector)
            {
                state.gpu.heads[i].frame_timer = false;
                state.send_frames(i);
                if state.gpu.heads[i].redraw {
                    state.render(i);
                }
            }
            TimeoutAction::Drop
        });
    }

    pub fn vblank(&mut self, crtc: crtc::Handle, metadata: &mut Option<DrmEventMetadata>) {
        let Some(i) = self.gpu.heads.iter().position(|h| h.crtc == crtc) else {
            return;
        };
        let time = metadata
            .as_ref()
            .and_then(|m| match m.time {
                DrmEventTime::Monotonic(t) if !t.is_zero() => Some(t),
                _ => None,
            })
            .unwrap_or_else(now);
        let head = &mut self.gpu.heads[i];
        if head
            .flip_pending
            .is_none_or(|submitted| stale(time, submitted, head.refresh))
        {
            return;
        }
        head.flip_pending = None;
        let sequence = metadata.as_ref().map_or(0, |m| m.sequence as u64);
        if let Some(mut feedback) = head.feedback.take() {
            feedback.presented::<_, Monotonic>(
                time,
                Refresh::fixed(head.refresh),
                sequence,
                wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion,
            );
        }
        let first_frame = std::mem::take(&mut head.client_in_flip) && !self.first_frame;
        let name = head.output.name();
        let redraw = head.redraw;
        self.send_frames(i);
        if redraw {
            self.render(i);
        }
        if first_frame {
            self.first_frame = true;
            self.timing(&format!("the greeter's first frame is on {name}"));
            // Input is needed only once something is on screen to type into. Opening it takes
            // 100+ ms, so the presentation events go out first, and the other outputs'
            // modesets come after it.
            let _ = self.dh.flush_clients();
            self.start_input();
        }
    }

    /// Frame callbacks for the surfaces on output `i`.
    fn send_frames(&self, i: usize) {
        let output = &self.gpu.heads[i].output;
        for window in self.space.elements_for_output(output) {
            window.send_frame(output, now(), None, |_, _| Some(output.clone()));
        }
    }

    pub fn session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::PauseSession => {
                info!("session paused");
                if let Some(libinput) = &mut self.libinput {
                    libinput.suspend();
                }
                self.gpu.drm.pause();
            }
            SessionEvent::ActivateSession => {
                info!(
                    "session active: redrawing {} outputs in full",
                    self.gpu.heads.len()
                );
                if let Some(libinput) = &mut self.libinput
                    && libinput.resume().is_err()
                {
                    warn!("libinput did not resume");
                }
                if let Err(e) = self.gpu.drm.activate(false) {
                    warn!("taking the GPU back: {e}");
                }
                for head in &mut self.gpu.heads {
                    head.flip_pending = None;
                    head.modeset_done = false;
                    if let Some(mut feedback) = head.feedback.take() {
                        feedback.discarded();
                    }
                    for buffer in &mut head.buffers {
                        buffer.drawn_at = None;
                    }
                }
                // Monitors may have come or gone meanwhile.
                self.scan_connectors(true);
                self.render_all();
            }
        }
    }
}

/// Whether a vblank event at `time` is for an earlier flip than the one submitted at
/// `submitted`: one from before a VT switch, or of an output removed from the same CRTC.
/// Drivers may stamp an event with the vblank before the submission, and the stamp is in µs,
/// so only an event more than a refresh interval earlier counts (the watchdog in `watch_flip`
/// covers any event wrongly dropped).
fn stale(time: Duration, submitted: Duration, refresh: Duration) -> bool {
    time + refresh < submitted
}

fn now() -> Duration {
    rustix::time::clock_gettime(rustix::time::ClockId::Monotonic)
        .try_into()
        .expect("the monotonic clock is not negative")
}

fn preferred_mode(conn: &connector::Info) -> Mode {
    *conn
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .unwrap_or(&conn.modes()[0])
}

/// The cursor's image, to be drawn into the frame, with the logical size to draw it at. The
/// renderer sizes it in logical pixels: the image is padded to a logical size that is a whole
/// number of pixels at this scale, so that it is drawn pixel for pixel, not stretched and
/// filtered.
fn soft_cursor(image: &cursor::Image, scale: f64) -> (MemoryRenderBuffer, Size<i32, Logical>) {
    let (w, h) = (image.width, image.height);
    let whole = |px: usize| {
        let least = (px as f64 / scale).ceil() as i32;
        (least..least + 120)
            .find(|l| (*l as f64 * scale).fract().abs() < 1e-6)
            .unwrap_or(least)
    };
    let logical = Size::<i32, Logical>::from((whole(w), whole(h)));
    let (w, h) = (
        (logical.w as f64 * scale).round() as usize,
        (logical.h as f64 * scale).round() as usize,
    );
    let bytes: Vec<u8> = image
        .padded(w, h)
        .iter()
        .flat_map(|p| p.to_ne_bytes())
        .collect();
    let size = (w as i32, h as i32);
    let buffer =
        MemoryRenderBuffer::from_slice(&bytes, Fourcc::Argb8888, size, 1, Transform::Normal, None);
    (buffer, logical)
}

fn soft_cursor_element(
    renderer: &mut PixmanRenderer,
    (buffer, size): &(MemoryRenderBuffer, Size<i32, Logical>),
    position: Point<i32, Physical>,
    scale: f64,
) -> Result<MemoryRenderBufferRenderElement<PixmanRenderer>, PixmanError> {
    // All of the image: by default the crop would be its logical size in image pixels.
    let src = Rectangle::from_size(size.to_f64().upscale(scale));
    let location = position.to_f64();
    MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        location,
        buffer,
        None,
        Some(src),
        Some(*size),
        Kind::Cursor,
    )
}

/// The output's cursor plane (atomic KMS only), with `image` on it, if it fits.
fn cursor_plane(gpu: &Gpu, surface: &DrmSurface, image: &cursor::Image) -> Option<CursorPlane> {
    if surface.is_legacy() {
        return None;
    }
    let size = gpu.cursor_size;
    if image.width > size.0 as usize || image.height > size.1 as usize {
        let (w, h) = (image.width, image.height);
        info!("cursor image {w}x{h} larger than the cursor plane's {size:?}");
        return None;
    }
    let (plane, claim) = surface
        .planes()
        .cursor
        .iter()
        .find_map(|p| Some((p.handle, surface.claim_plane(p.handle)?)))?;
    let mut dumb = Dumb::new(&gpu.fd, size, Fourcc::Argb8888)
        .inspect_err(|e| warn!("cursor buffer: {e:#}"))
        .ok()?;
    let (w, h) = (size.0 as usize, size.1 as usize);
    let pixels = image.padded(w, h);
    let pitch = dumb.pitch;
    let mapping = dumb.bytes_mut();
    for (y, row) in pixels.chunks(w).enumerate() {
        for (x, pixel) in row.iter().enumerate() {
            let at = y * pitch + x * 4;
            mapping[at..at + 4].copy_from_slice(&pixel.to_ne_bytes());
        }
    }
    Some(CursorPlane {
        plane,
        _claim: claim,
        dumb,
        size,
        shown: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_vblank_events() {
        let ms = Duration::from_millis;
        let refresh = ms(16);
        // For the flip: after it, or stamped with the vblank just before it.
        assert!(!stale(ms(1010), ms(1000), refresh));
        assert!(!stale(ms(995), ms(1000), refresh));
        // From before a VT switch or an output's removal: long before.
        assert!(stale(ms(500), ms(1000), refresh));
    }

    /// The software cursor at scale 1.5, drawn pixel for pixel (no grey from filtering): the
    /// whole arrow, and its edge where it is cut off by the bottom and the right of the output.
    #[test]
    fn soft_cursor_is_not_stretched() {
        let cursor = soft_cursor(&cursor::arrow(2), 1.5);
        let (w, h) = (1024, 768);
        for ((x, y), outline) in [((450, 450), 172), ((450, 766), 4), ((1023, 450), 30)] {
            let mut renderer = PixmanRenderer::new().unwrap();
            let mut image = pixman::Image::new(pixman::FormatCode::X8R8G8B8, w, h, true).unwrap();
            let mut damage = OutputDamageTracker::new((w as i32, h as i32), 1.5, Transform::Normal);
            let element = soft_cursor_element(&mut renderer, &cursor, (x, y).into(), 1.5).unwrap();
            let mut target = renderer.bind(&mut image).unwrap();
            let white = [1.0, 1.0, 1.0, 1.0];
            damage
                .render_output(&mut renderer, &mut target, 0, &[element], white)
                .unwrap();
            drop(target);
            // SAFETY: the image's own w * h pixels.
            let pixels = unsafe { std::slice::from_raw_parts(image.data(), w * h) };
            let count = |colour| pixels.iter().filter(|&&p| p & 0xff_ffff == colour).count();
            assert_eq!(count(0), outline, "black at {x},{y}");
            assert_eq!(count(0) + count(0xff_ffff), w * h, "grey at {x},{y}");
        }
    }
}
