//! The connection to the greeter's compositor: a fullscreen toplevel on each output, with the
//! login screen on the primary one (the first the compositor announced) and the background
//! alone on the others; the session and layout popups; the keyboard and the pointer.

use std::collections::VecDeque;
use std::mem;

use anyhow::Context;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use tokio::time::Instant;
use tracing::{error, info};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::shell::client::xdg_positioner::{
    Anchor, ConstraintAdjustment, Gravity,
};
use xkbcommon::xkb::Keysym;

use crate::chooser::Escape;
use crate::focus::{self, Action, Widget};
use crate::login::{self, Extras, Login, PopupList, View};
use ui::draw::{self, Layout, Painter, Scene};
use ui::entry::{Key, Status};
use ui::frames::{Frame, Frames};
use ui::queue::Queue;

/// The left button's evdev code.
const BTN_LEFT: u32 = 0x110;

pub enum Event {
    /// The answer was submitted, by Enter or the Log In button; `Entry::submission` has it.
    Submit,
    /// Escape or the "‹ Users" chip: give up this login.
    Cancel,
    /// The avatar at this place in the row was activated.
    Choose(usize),
    /// The power button at this place in the row was activated.
    Power(usize),
    /// The dialog's button: do what it asks.
    Confirm,
    /// The dialog's Cancel, Escape on it, or a click outside it.
    Dismiss,
}

impl Event {
    /// Whether the message line is cleared: by anything the user does about the login, not
    /// by the power row and its dialog, which leave a "Wrong password" or PAM's message
    /// showing.
    pub fn clears_message(&self) -> bool {
        !matches!(self, Event::Power(_) | Event::Confirm | Event::Dismiss)
    }
}

pub struct Wayland {
    queue: Queue<State>,
    state: State,
}

struct State {
    registry: RegistryState,
    output_state: OutputState,
    compositor: WlCompositor,
    xdg_shell: XdgShell,
    fractional: WpFractionalScaleManagerV1,
    viewporter: WpViewporter,
    shm: Shm,
    pool: SlotPool,
    seat: WlSeat,
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    pointer_at: Option<PointerAt>,
    /// The last key or button press's, for the popup's grab.
    serial: u32,
    painter: Painter,
    toplevels: Vec<Toplevel>,
    popup: Option<ListPopup>,
    login: Login,
    /// The minute the clocks were last rendered for.
    minute: i64,
    events: VecDeque<Event>,
}

/// Where the pointer is: on `surface`, at `(x, y)` logical pixels.
struct PointerAt {
    surface: WlSurface,
    x: f64,
    y: f64,
}

// Fields drop in order: each surface's viewport before its surface.
struct Toplevel {
    surface: Surface<Shown>,
    output: WlOutput,
    /// Its output's name in the registry: the lowest is the primary output's.
    id: u32,
    name: String,
    /// The output's current mode, which the frames fit when within a pixel of it.
    mode: Option<(i32, i32)>,
    fractional: WpFractionalScaleV1,
    window: Window,
    prepared: Option<Prepared>,
}

/// An output's lock screen, and the greeter's widgets on it.
struct Prepared {
    scene: Scene,
    extras: Option<Extras>,
}

#[derive(Clone, Debug, PartialEq)]
enum Shown {
    Login(Box<View>),
    Background,
}

/// The session or layout popup.
struct ListPopup {
    /// The chip it opened from: `Widget::Session` or `Widget::Layout`.
    picker: Widget,
    /// Showing the row highlighted.
    surface: Surface<usize>,
    popup: Popup,
    list: PopupList,
    selected: usize,
}

/// What `picker`'s popup is called in the log.
fn name(picker: Widget) -> &'static str {
    if picker == Widget::Session {
        "session"
    } else {
        "layout"
    }
}

/// A surface the greeter draws on: its viewport, logical size and scale, its buffers, and
/// what it shows (a `T`).
struct Surface<T> {
    viewport: WpViewport,
    /// Its logical size, once configured.
    size: Option<(i32, i32)>,
    /// In 120ths.
    scale: u32,
    frames: Frames<T>,
    /// `None` to draw it again.
    shown: Option<T>,
    /// Whether its first frame was committed.
    drawn: bool,
}

impl<T: Clone + PartialEq> Surface<T> {
    fn new(viewport: WpViewport, scale: u32) -> Self {
        Self {
            viewport,
            size: None,
            scale,
            frames: Frames::default(),
            shown: None,
            drawn: false,
        }
    }

    /// Commit to `wl` a frame of `layout` that shows `shows`, painted by `paint` unless a
    /// buffer shows it already. Nothing if the surface shows it, or while the compositor
    /// holds both buffers (a release wakes the loop). Returns whether it was the first frame.
    fn draw(
        &mut self,
        wl: &WlSurface,
        pool: &mut SlotPool,
        layout: Layout,
        format: Format,
        shows: T,
        paint: impl FnOnce(&mut [u8]),
    ) -> bool {
        let Some(size) = self.size.filter(|_| self.shown.as_ref() != Some(&shows)) else {
            return false;
        };
        let Some(frame) = self.frames.frame(pool, layout.pixels, format, &shows) else {
            return false;
        };
        let (Frame::Reused(at) | Frame::Painted(at)) = frame;
        if let Frame::Painted(_) = frame {
            paint(self.frames.canvas(at, pool));
        }
        if let Err(e) = self.frames.attach(at, wl, layout.pixels) {
            error!("drawing: {e:#}");
            return false;
        }
        self.viewport.set_destination(size.0, size.1);
        wl.commit();
        self.shown = Some(shows);
        !mem::replace(&mut self.drawn, true)
    }

    /// Draw it again, its buffers' contents out of date too.
    fn forget(&mut self) {
        self.shown = None;
        self.frames.forget();
    }
}

impl Drop for Toplevel {
    fn drop(&mut self) {
        self.fractional.destroy();
    }
}

impl<T> Drop for Surface<T> {
    fn drop(&mut self) {
        self.viewport.destroy();
    }
}

impl Wayland {
    pub fn connect(login: Login, painter: Painter) -> anyhow::Result<Self> {
        let conn = Connection::connect_to_env().context("connecting to the compositor")?;
        let (globals, queue) =
            registry_queue_init::<State>(&conn).context("listing Wayland globals")?;
        let qh = queue.handle();
        let shm = Shm::bind(&globals, &qh).context("binding wl_shm")?;
        let state = State {
            registry: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            compositor: globals
                .bind(&qh, 4..=6, ())
                .context("binding wl_compositor")?,
            xdg_shell: XdgShell::bind(&globals, &qh).context("binding xdg_wm_base")?,
            fractional: globals
                .bind(&qh, 1..=1, ())
                .context("binding wp_fractional_scale_manager_v1")?,
            viewporter: globals
                .bind(&qh, 1..=1, ())
                .context("binding wp_viewporter")?,
            pool: SlotPool::new(1, &shm).context("creating a buffer pool")?,
            shm,
            // Version 10 would have the compositor repeat keys, which the entry does itself.
            seat: globals.bind(&qh, 1..=9, ()).context("binding wl_seat")?,
            keyboard: None,
            pointer: None,
            pointer_at: None,
            serial: 0,
            painter,
            toplevels: Vec::new(),
            popup: None,
            login,
            minute: draw::minute(),
            events: VecDeque::new(),
        };
        Ok(Self {
            queue: Queue::new(&conn, queue)?,
            state,
        })
    }

    pub fn login(&mut self) -> &mut Login {
        &mut self.state.login
    }

    /// Greet `name`: the avatar's initial and the name under it, on every output again.
    pub fn set_name(&mut self, name: String) {
        self.state.painter.set_name(name);
        for toplevel in &mut self.state.toplevels {
            toplevel.prepared = None;
            toplevel.surface.forget();
        }
    }

    /// Wait for the next event from the login screen; an error means the compositor is gone.
    /// Cancel-safe: what has been dispatched stays queued.
    pub async fn next(&mut self) -> anyhow::Result<Event> {
        loop {
            self.queue.dispatch(&mut self.state)?;
            self.state.refresh_clocks();
            self.state.redraw();
            self.queue.flush()?;
            if let Some(event) = self.state.events.pop_front() {
                return Ok(event);
            }
            let now = Instant::now();
            let minute = now + draw::until_next_minute();
            let entry = &self.state.login.entry;
            let wake = entry.deadline(now).map_or(minute, |at| at.min(minute));
            if !self.queue.wait(Some(wake)).await? {
                self.state.login.entry.tick(Instant::now());
            }
        }
    }
}

impl State {
    /// The toplevel with the login screen: the primary output's.
    fn login_toplevel(&self) -> Option<usize> {
        (0..self.toplevels.len()).min_by_key(|&i| self.toplevels[i].id)
    }

    /// Render the clocks again once the minute changes.
    fn refresh_clocks(&mut self) {
        let minute = draw::minute();
        if self.minute == minute {
            return;
        }
        self.minute = minute;
        for toplevel in &mut self.toplevels {
            if let Some(prepared) = &mut toplevel.prepared
                && self.painter.refresh(&mut prepared.scene)
            {
                toplevel.surface.forget();
            }
        }
    }

    /// Draw every surface whose content changed.
    fn redraw(&mut self) {
        let view = self.login.view(Instant::now());
        let login = self.login_toplevel();
        for i in 0..self.toplevels.len() {
            let shows = if Some(i) == login {
                Shown::Login(Box::new(view.clone()))
            } else {
                Shown::Background
            };
            self.draw_toplevel(i, shows);
        }
        self.draw_popup();
    }

    fn draw_toplevel(&mut self, i: usize, shows: Shown) {
        let State {
            toplevels,
            painter,
            pool,
            ..
        } = self;
        let Toplevel {
            surface,
            name,
            mode,
            window,
            prepared,
            ..
        } = &mut toplevels[i];
        let Some(size) = surface.size else { return };
        let layout = Layout::new(size, surface.scale, *mode);
        let prepared = match prepared {
            Some(prepared) if prepared.scene.layout == layout => prepared,
            other => {
                surface.frames.forget();
                other.insert(prepare(painter, layout))
            }
        };
        if let (Shown::Login(view), Some(extras)) = (&shows, &mut prepared.extras) {
            if let Status::Cooldown(seconds) = view.look.status {
                painter.countdown(&mut prepared.scene, seconds);
            }
            painter.set_hint(&mut prepared.scene, &view.hint);
            painter.set_text(&mut prepared.scene, view.text.as_deref());
            if let Err(e) = extras.update(painter, view) {
                error!("rendering the login screen: {e}");
            }
        }
        let paint = |canvas: &mut [u8]| match &shows {
            Shown::Login(view) => {
                if view.row.is_some() {
                    prepared.scene.draw_times(canvas);
                } else {
                    prepared.scene.draw(canvas, view.look);
                }
                if let Some(extras) = &prepared.extras {
                    let drawn = draw::draw_on(canvas, layout.pixels, cairo::Format::Rgb24, |cr| {
                        extras.paint(cr)
                    });
                    if let Err(e) = drawn {
                        error!("drawing the login screen: {e}");
                    }
                }
            }
            Shown::Background => prepared.scene.draw_background(canvas),
        };
        let first = surface.draw(
            window.wl_surface(),
            pool,
            layout,
            Format::Xrgb8888,
            shows.clone(),
            paint,
        );
        if first {
            let scale = f64::from(layout.scale) / 120.0;
            info!(
                "first frame on {name} ({}x{} at scale {scale})",
                size.0, size.1
            );
        }
    }

    fn draw_popup(&mut self) {
        let State { popup, pool, .. } = self;
        let Some(ListPopup {
            picker,
            surface,
            popup,
            list,
            selected,
        }) = popup
        else {
            return;
        };
        let Some(size) = surface.size else { return };
        let layout = Layout::new(size, surface.scale, None);
        let paint = |canvas: &mut [u8]| {
            if let Err(e) = list.draw(canvas, layout, *selected) {
                error!("drawing the {} popup: {e}", name(*picker));
            }
        };
        let wl = popup.wl_surface();
        if surface.draw(wl, pool, layout, Format::Argb8888, *selected, paint) {
            info!("{} popup shown", name(*picker));
        }
    }

    /// Open `picker`'s popup (the session chip's or the layout's) under it, if it is shown.
    fn open_popup(&mut self, picker: Widget, qh: &QueueHandle<Self>) -> anyhow::Result<()> {
        let Some(i) = self.login_toplevel().filter(|_| self.popup.is_none()) else {
            return Ok(());
        };
        let toplevel = &self.toplevels[i];
        let Some(prepared) = &toplevel.prepared else {
            return Ok(());
        };
        let Some(chip) = prepared
            .extras
            .as_ref()
            .and_then(|extras| extras.place(picker))
        else {
            return Ok(());
        };
        let layout = prepared.scene.layout;
        let f = layout.factor();
        let login = &self.login;
        let (names, selected): (Vec<_>, _) = if picker == Widget::Session {
            let names = login.sessions.iter().map(|s| &*s.name);
            (names.collect(), login.chosen)
        } else {
            (
                login.layouts.iter().map(|l| &*l.name).collect(),
                login.layout,
            )
        };
        let list = PopupList::new(&self.painter, &names, f)?;
        let positioner = XdgPositioner::new(&self.xdg_shell)?;
        positioner.set_size(list.size.0, list.size.1);
        let logical = |v: f64| (v / f).round() as i32;
        let (x, y) = (logical(chip.x), logical(chip.y));
        let (right, bottom) = (logical(chip.x + chip.w), logical(chip.y + chip.h));
        positioner.set_anchor_rect(x, y, right - x, bottom - y);
        positioner.set_anchor(Anchor::Bottom);
        positioner.set_gravity(Gravity::Bottom);
        positioner.set_offset(0, login::POPUP_GAP);
        let adjust = ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY;
        positioner.set_constraint_adjustment(adjust | ConstraintAdjustment::FlipY);
        let surface = self.compositor.create_surface(qh, ());
        let viewport = self.viewporter.get_viewport(&surface, qh, ());
        let parent = Some(toplevel.window.xdg_surface());
        let popup = Popup::from_surface(parent, &positioner, qh, surface, &self.xdg_shell)?;
        popup.xdg_popup().grab(&self.seat, self.serial);
        popup.wl_surface().commit();
        self.popup = Some(ListPopup {
            picker,
            surface: Surface::new(viewport, layout.scale),
            popup,
            list,
            selected,
        });
        Ok(())
    }

    /// Row `i` of the popup was chosen; it closes.
    fn choose(&mut self, i: usize) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        let login = &mut self.login;
        if popup.picker == Widget::Session {
            login.chosen = i;
            info!("session {} chosen", login.sessions[i].name);
        } else {
            login.set_layout(i);
            info!("layout {} chosen", login.layouts[login.layout].name);
        }
    }

    fn click(&mut self, qh: &QueueHandle<Self>) {
        let Some(at) = &self.pointer_at else { return };
        if let Some(popup) = &self.popup
            && popup.popup.wl_surface() == &at.surface
        {
            if let Some(row) = popup.list.row_at(at.y) {
                self.choose(row);
            }
            return;
        }
        let Some(i) = self.login_toplevel() else {
            return;
        };
        let toplevel = &self.toplevels[i];
        let extras = toplevel.prepared.as_ref().and_then(|p| p.extras.as_ref());
        let (Some(extras), true) = (extras, toplevel.window.wl_surface() == &at.surface) else {
            return;
        };
        let f = f64::from(toplevel.surface.scale) / 120.0;
        let (x, y) = (at.x * f, at.y * f);
        if let Some(widget) = extras.hit(x, y) {
            self.login.chooser.focus.click(widget);
            self.act(Action::Activate(widget), qh);
        } else if self.login.chooser.dialog.is_some() && !extras.in_dialog(x, y) {
            self.events.push_back(Event::Dismiss);
        }
    }

    /// Do what a key or click on a widget does, but type.
    fn act(&mut self, action: Action, qh: &QueueHandle<Self>) {
        let Action::Activate(widget) = action else {
            return;
        };
        match widget {
            Widget::Avatar(i) => self.events.push_back(Event::Choose(i)),
            Widget::Pill => {}
            Widget::Users => self.events.push_back(Event::Cancel),
            Widget::Power(i) => self.events.push_back(Event::Power(i)),
            Widget::Cancel => self.events.push_back(Event::Dismiss),
            Widget::Confirm => self.events.push_back(Event::Confirm),
            Widget::Session | Widget::Layout => {
                if let Err(e) = self.open_popup(widget, qh) {
                    error!("opening the {} popup: {e:#}", name(widget));
                }
            }
            // As Enter in the field; the "key" is the button, which does not repeat.
            Widget::LogIn => {
                if self.login.entry.press(Key::Enter, BTN_LEFT, Instant::now()) {
                    self.events.push_back(Event::Submit);
                }
            }
        }
    }

    /// Act on the key with evdev `code`, pressed, unless it is for the field: in a popup,
    /// Escape, or as the focus has it. Returns whether it is for the field.
    fn route(&mut self, code: u32, qh: &QueueHandle<Self>) -> bool {
        let keyboard = &self.login.keyboard;
        let keysym = keyboard.keysym(code);
        if self.popup.is_some() {
            self.popup_key(keysym);
            return false;
        }
        let focus_key = match keysym {
            Some(Keysym::Escape) => {
                let event = match self.login.chooser.escape() {
                    Escape::CloseDialog => Event::Dismiss,
                    Escape::Cancel => Event::Cancel,
                };
                self.events.push_back(event);
                return false;
            }
            Some(Keysym::Tab) => focus::Key::Tab,
            Some(Keysym::ISO_Left_Tab) => focus::Key::BackTab,
            Some(Keysym::Left) => focus::Key::Left,
            Some(Keysym::Right) => focus::Key::Right,
            Some(Keysym::Return | Keysym::KP_Enter) => focus::Key::Activate,
            Some(Keysym::space) => focus::Key::Space,
            _ if keyboard.key(code).is_some() => focus::Key::Text,
            // Modifiers and keys that do nothing, which the field's keyboard follows.
            _ => return true,
        };
        let chooser = &mut self.login.chooser;
        let shown = chooser.shown();
        match chooser.focus.key(focus_key, &shown) {
            Action::Type => true,
            action => {
                self.act(action, qh);
                false
            }
        }
    }

    /// Act on a key pressed while a popup is open: arrows move, Enter or Space chooses.
    fn popup_key(&mut self, keysym: Option<Keysym>) {
        let Some(popup) = &mut self.popup else { return };
        let last = popup.list.rows() - 1;
        match keysym {
            Some(Keysym::Up) => popup.selected = popup.selected.saturating_sub(1),
            Some(Keysym::Down) => popup.selected = (popup.selected + 1).min(last),
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::space) => {
                let selected = popup.selected;
                self.choose(selected);
            }
            _ => {}
        }
    }
}

/// Prepare the lock screen and the greeter's widgets for `layout`; if that fails, the
/// background's plain colour.
fn prepare(painter: &Painter, layout: Layout) -> Prepared {
    let scene = painter.scene(layout).unwrap_or_else(|e| {
        error!("preparing the login screen: {e}; drawing it plain");
        painter.plain(layout)
    });
    let extras = Extras::new(painter, &scene).unwrap_or_else(|e| {
        error!("preparing the login screen's buttons: {e}");
        None
    });
    Prepared { scene, extras }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    /// A fullscreen toplevel on it, configured by the compositor.
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: WlOutput) {
        let Some(info) = self.output_state.info(&output) else {
            return;
        };
        let surface = self.compositor.create_surface(qh, ());
        let fractional = self
            .fractional
            .get_fractional_scale(&surface, qh, surface.clone());
        let viewport = self.viewporter.get_viewport(&surface, qh, ());
        let window = self
            .xdg_shell
            .create_window(surface, WindowDecorations::None, qh);
        window.set_fullscreen(Some(&output));
        window.wl_surface().commit();
        let mode = info.modes.iter().find(|mode| mode.current);
        self.toplevels.push(Toplevel {
            surface: Surface::new(viewport, 120),
            output,
            id: info.id,
            name: info.name.unwrap_or_default(),
            mode: mode.map(|mode| mode.dimensions),
            fractional,
            window,
            prepared: None,
        });
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        let info = self.output_state.info(&output);
        let mode = info
            .as_ref()
            .and_then(|info| info.modes.iter().find(|m| m.current));
        if let Some(toplevel) = self.toplevels.iter_mut().find(|t| t.output == output) {
            toplevel.mode = mode.map(|mode| mode.dimensions);
            toplevel.surface.shown = None;
        }
    }

    /// Its toplevel goes; the next output by the compositor's order gets the login screen.
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: WlOutput) {
        let login = self.login_toplevel();
        if let Some(i) = self.toplevels.iter().position(|t| t.output == output) {
            if Some(i) == login {
                self.popup = None;
            }
            self.toplevels.remove(i);
        }
    }
}

impl WindowHandler for State {
    /// Only the compositor could ask, and the greeter has nothing to close.
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {}

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let Some(toplevel) = self.toplevels.iter_mut().find(|t| &t.window == window) else {
            return;
        };
        if let (Some(width), Some(height)) = configure.new_size {
            toplevel.surface.size = Some((width.get() as i32, height.get() as i32));
        }
        toplevel.surface.shown = None;
    }
}

impl PopupHandler for State {
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        popup: &Popup,
        configure: PopupConfigure,
    ) {
        if let Some(ours) = &mut self.popup
            && &ours.popup == popup
        {
            ours.surface.size = Some((configure.width, configure.height));
            ours.surface.shown = None;
        }
    }

    /// Escape or a click outside it.
    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        if let Some(ours) = self.popup.take_if(|ours| &ours.popup == popup) {
            info!("{} popup dismissed", name(ours.picker));
        }
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
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = wl_event else {
            return;
        };
        let toplevels = state.toplevels.iter_mut();
        if let Some(toplevel) = toplevels
            .into_iter()
            .find(|t| t.window.wl_surface() == surface)
        {
            toplevel.surface.scale = scale;
            toplevel.surface.shown = None;
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        wl_event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = wl_event
        else {
            return;
        };
        let has_keyboard = capabilities.contains(Capability::Keyboard);
        if has_keyboard && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        } else if !has_keyboard && let Some(keyboard) = state.keyboard.take() {
            keyboard.release();
            state.login.entry.stop_repeat();
        }
        let has_pointer = capabilities.contains(Capability::Pointer);
        if has_pointer && state.pointer.is_none() {
            state.pointer = Some(seat.get_pointer(qh, ()));
        } else if !has_pointer && let Some(pointer) = state.pointer.take() {
            pointer.release();
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        wl_event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            serial,
            key,
            state: WEnum::Value(KeyState::Pressed),
            ..
        } = wl_event
        {
            state.serial = serial;
            if !state.route(key, qh) {
                // Its modifier still counts: a Caps Lock pressed in a popup.
                let login = &mut state.login;
                return login.keyboard.pressed_elsewhere(key, &mut login.entry);
            }
        }
        let login = &mut state.login;
        if login.keyboard.handle(wl_event, &mut login.entry) {
            state.events.push_back(Event::Submit);
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        wl_event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match wl_event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_at = Some(PointerAt {
                    surface,
                    x: surface_x,
                    y: surface_y,
                });
            }
            wl_pointer::Event::Leave { .. } => state.pointer_at = None,
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(at) = &mut state.pointer_at {
                    (at.x, at.y) = (surface_x, surface_y);
                }
            }
            wl_pointer::Event::Button {
                serial,
                button: BTN_LEFT,
                state: WEnum::Value(ButtonState::Pressed),
                ..
            } => {
                state.serial = serial;
                state.click(qh);
            }
            _ => {}
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
delegate_noop!(State: WlCompositor);
delegate_noop!(State: WpFractionalScaleManagerV1);
delegate_noop!(State: WpViewporter);
delegate_noop!(State: WpViewport);
// Each toplevel covers its one output, so its enter, leave and scale events change nothing.
delegate_noop!(State: ignore WlSurface);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_power_row_and_its_dialog_leave_the_message() {
        for event in [Event::Power(0), Event::Confirm, Event::Dismiss] {
            assert!(!event.clears_message());
        }
        for event in [Event::Submit, Event::Cancel, Event::Choose(0)] {
            assert!(event.clears_message());
        }
    }
}
