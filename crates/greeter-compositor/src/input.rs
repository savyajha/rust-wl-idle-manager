//! Input from libinput (opened after the first frame) and hotplug from udev. The keyboard goes
//! to the focused surface with the system's default keymap; Ctrl+Alt+Fn switches VT. The pointer
//! goes to the surface under it, popups first.

use smithay::backend::input::{
    AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
    KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::session::Session;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::udev::{UdevBackend, UdevEvent};
use smithay::input::keyboard::{FilterResult, KeyboardHandle, keysyms};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle};
use smithay::reexports::input::{ClickMethod, Device, Libinput};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, SERIAL_COUNTER};
use smithay::wayland::compositor::get_parent;
use tracing::{info, warn};

use crate::State;

/// What a key does besides going to the client.
enum KeyAction {
    SwitchVt(i32),
    DismissPopup,
    Swallow,
}

impl State {
    pub fn keyboard(&self) -> KeyboardHandle<State> {
        self.seat.get_keyboard().expect("added in main")
    }

    pub fn pointer(&self) -> PointerHandle<State> {
        self.seat.get_pointer().expect("added in main")
    }

    /// Watches udev for monitor hotplug: change events of our own GPU rescan its connectors,
    /// probing them. No device is opened. Set up before the start-up scan, so that a hotplug
    /// from then on is queued, not missed.
    pub fn watch_hotplug(&mut self) {
        let watching = UdevBackend::new(self.session.seat()).is_ok_and(|udev| {
            let source = self.handle.insert_source(udev, |event, _, state| {
                if let UdevEvent::Changed { device_id } = event
                    && device_id == state.gpu.dev_id
                    && state.scan_connectors(true)
                {
                    state.render_all();
                }
            });
            source.is_ok()
        });
        if !watching {
            warn!("udev: cannot watch it; no output hotplug");
        }
    }

    /// Opens libinput, and lets the outputs other than the primary one be set up. Once only.
    pub fn start_input(&mut self) {
        if self.libinput.is_some() {
            return;
        }
        let mut libinput = Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
            self.session.clone().into(),
        );
        if libinput.udev_assign_seat(&self.session.seat()).is_err() {
            warn!("libinput: could not assign the seat");
        }
        self.libinput = Some(libinput.clone());
        let backend = LibinputInputBackend::new(libinput);
        if let Err(e) = self.handle.insert_source(backend, |event, _, state| {
            if let InputEvent::DeviceAdded { device } = &event {
                configure(device.clone(), state.tap, state.clickfinger);
            }
            state.input(event)
        }) {
            warn!("libinput: {e}");
        }
        self.timing("input ready");
        // Without a first frame (1 s passed): the other outputs come up now.
        self.render_all();
    }

    fn input<B: InputBackend>(&mut self, event: InputEvent<B>) {
        match event {
            InputEvent::Keyboard { event } => self.key::<B>(event),
            InputEvent::PointerMotion { event } => {
                let point = self.clamp_to_layout(self.pointer + event.delta());
                self.pointer_to(point, event.time_msec());
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // Over the whole layout, and onto an output where that has gaps.
                if let Some(layout) = self.outputs().reduce(|a, b| a.merge(b)) {
                    let point =
                        layout.loc.to_f64() + event.position_transformed(layout.size).to_f64();
                    self.pointer_to(self.clamp_to_layout(point), event.time_msec());
                }
            }
            InputEvent::PointerButton { event } => self.button::<B>(event),
            InputEvent::PointerAxis { event } => self.axis::<B>(event),
            _ => {}
        }
    }

    fn key<B: InputBackend>(&mut self, event: B::KeyboardKeyEvent) {
        let keyboard = self.keyboard();
        let serial = SERIAL_COUNTER.next_serial();
        let grabbing = !self.grab.is_empty();
        let code = event.key_code();
        // Smithay forwards the release of a key whose press was intercepted; it goes nowhere.
        let swallow = event.state() == KeyState::Released && self.intercepted.contains(&code);
        self.intercepted.retain(|&c| c != code);
        let action = keyboard.input(
            self,
            code,
            event.state(),
            serial,
            event.time_msec(),
            |_, _, keysym| {
                let sym = keysym.modified_sym().raw();
                if swallow {
                    FilterResult::Intercept(KeyAction::Swallow)
                } else if event.state() != KeyState::Pressed {
                    FilterResult::Forward
                } else if (keysyms::KEY_XF86Switch_VT_1..=keysyms::KEY_XF86Switch_VT_12)
                    .contains(&sym)
                {
                    let vt = sym - keysyms::KEY_XF86Switch_VT_1 + 1;
                    FilterResult::Intercept(KeyAction::SwitchVt(vt as i32))
                } else if grabbing && sym == keysyms::KEY_Escape {
                    FilterResult::Intercept(KeyAction::DismissPopup)
                } else {
                    FilterResult::Forward
                }
            },
        );
        if action.is_some() && event.state() == KeyState::Pressed {
            self.intercepted.push(code);
        }
        match action {
            Some(KeyAction::SwitchVt(vt)) => {
                if let Err(e) = self.session.change_vt(vt) {
                    warn!("switching to VT {vt}: {e}");
                }
            }
            Some(KeyAction::DismissPopup) => self.dismiss_popups(false),
            Some(KeyAction::Swallow) | None => {}
        }
    }

    /// The outputs' rectangles in the layout.
    fn outputs(&self) -> impl Iterator<Item = Rectangle<i32, Logical>> {
        self.gpu
            .heads
            .iter()
            .filter_map(|h| self.space.output_geometry(&h.output))
    }

    /// `point`, or the nearest point on an output if it is on none.
    pub fn clamp_to_layout(&self, point: Point<f64, Logical>) -> Point<f64, Logical> {
        let distance = |p: &Point<f64, Logical>| (p.x - point.x).powi(2) + (p.y - point.y).powi(2);
        self.outputs()
            .map(|r| {
                let (start, end) = (r.loc.to_f64(), (r.loc + r.size).to_f64());
                let x = point.x.clamp(start.x, end.x - 1.0);
                Point::from((x, point.y.clamp(start.y, end.y - 1.0)))
            })
            .min_by(|a, b| distance(a).total_cmp(&distance(b)))
            .unwrap_or(point)
    }

    fn pointer_to(&mut self, point: Point<f64, Logical>, time: u32) {
        self.pointer = point;
        self.cursor_visible = true;
        self.update_pointer_focus(time);
        // The cursor moved.
        self.render_all();
    }

    /// The pointer's focus: the surface now under it.
    pub fn update_pointer_focus(&mut self, time: u32) {
        let pointer = self.pointer();
        let focus = self.surface_under(self.pointer);
        let event = MotionEvent {
            location: self.pointer,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        };
        pointer.motion(self, focus, &event);
        pointer.frame(self);
    }

    fn button<B: InputBackend>(&mut self, event: B::PointerButtonEvent) {
        let pressed = event.state() == ButtonState::Pressed;
        if !pressed && self.swallowed_button == Some(event.button_code()) {
            self.swallowed_button = None;
            return;
        }
        if pressed {
            let under = self.surface_under(self.pointer).map(|(s, _)| root(s));
            if !self.grab.is_empty() {
                // A click outside the grabbing popups dismisses them, and goes nowhere else.
                if !self
                    .grab
                    .iter()
                    .any(|p| Some(p.wl_surface()) == under.as_ref())
                {
                    self.swallowed_button = Some(event.button_code());
                    self.dismiss_popups(true);
                    return;
                }
            } else if let Some(t) = self
                .toplevels
                .iter()
                .find(|t| Some(t.surface().wl_surface()) == under.as_ref())
            {
                // Click to focus.
                self.focus = Some(t.window.clone());
                self.refocus();
            }
        }
        let pointer = self.pointer();
        pointer.button(
            self,
            &ButtonEvent {
                button: event.button_code(),
                state: event.state(),
                serial: SERIAL_COUNTER.next_serial(),
                time: event.time_msec(),
            },
        );
        pointer.frame(self);
    }

    /// Scrolling, as libinput reports it (wheel clicks in v120 units, finger and continuous
    /// scrolling in pixels).
    fn axis<B: InputBackend>(&mut self, event: B::PointerAxisEvent) {
        let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
        for axis in [Axis::Horizontal, Axis::Vertical] {
            let v120 = event.amount_v120(axis);
            let amount = event
                .amount(axis)
                .or(v120.map(|v| v * 15.0 / 120.0))
                .unwrap_or(0.0);
            if amount != 0.0 {
                frame = frame
                    .relative_direction(axis, event.relative_direction(axis))
                    .value(axis, amount);
                if let Some(v120) = v120 {
                    frame = frame.v120(axis, v120 as i32);
                }
            } else if event.source() == AxisSource::Finger && event.amount(axis) == Some(0.0) {
                frame = frame.stop(axis);
            }
        }
        let pointer = self.pointer();
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

/// The surface whose subsurface `surface` is, or itself.
pub fn root(mut surface: WlSurface) -> WlSurface {
    while let Some(parent) = get_parent(&surface) {
        surface = parent;
    }
    surface
}

/// A new touchpad set up as the desktop may have it, both of which libinput leaves off:
/// tap-to-click (`--tap`), and clickfinger (`--clickfinger`: the number of fingers picks the
/// button, not where the pad is pressed).
fn configure(mut device: Device, tap: bool, clickfinger: bool) {
    if tap && device.config_tap_finger_count() > 0 {
        let result = device.config_tap_set_enabled(true);
        info!("{}: tap-to-click on: {result:?}", device.name());
    }
    if clickfinger
        && device
            .config_click_methods()
            .contains(&ClickMethod::Clickfinger)
    {
        let result = device.config_click_set_method(ClickMethod::Clickfinger);
        info!("{}: clickfinger on: {result:?}", device.name());
    }
}
