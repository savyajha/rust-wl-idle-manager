//! The VM test's client: one fullscreen toplevel per output, each a solid colour drawn with
//! wl_shm at the output's preferred scale (fractional + viewporter), and a grabbing popup on the
//! P key, anchored near the right edge so that it has to slide back onto the output. Q exits.
//! It prints one line per event the test looks for. A toplevel whose output goes is kept (the
//! compositor moves it) and reused for the next output that comes. SIGUSR1 inverts the colours.

use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_keyboard, wl_output, wl_pointer,
    wl_registry, wl_seat, wl_shm, wl_shm_pool::WlShmPool, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation::WpPresentation, wp_presentation_feedback,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};

const COLOURS: [u32; 2] = [0xff33_66cc, 0xffcc_6633];
const POPUP_COLOUR: u32 = 0xffee_eeee;
const KEY_Q: u32 = 16;
const KEY_P: u32 = 25;

/// A surface we draw: a toplevel's or the popup's.
struct Surface {
    surface: WlSurface,
    xdg: xdg_surface::XdgSurface,
    viewport: WpViewport,
    _scale: WpFractionalScaleV1,
    /// Logical size, from the last configure.
    size: (i32, i32),
    /// Preferred scale in 120ths.
    scale120: u32,
    colour: u32,
    label: String,
}

struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    name: String,
    /// The output was removed; its toplevel, if any, waits for the next one.
    gone: bool,
    toplevel: Option<(Surface, xdg_toplevel::XdgToplevel)>,
}

struct Popup {
    surface: Surface,
    popup: xdg_popup::XdgPopup,
}

struct State {
    compositor: WlCompositor,
    shm: wl_shm::WlShm,
    wm_base: xdg_wm_base::XdgWmBase,
    seat: wl_seat::WlSeat,
    presentation: WpPresentation,
    viewporter: WpViewporter,
    fractional: WpFractionalScaleManagerV1,
    outputs: Vec<Output>,
    popup: Option<Popup>,
    keyboard: Option<WlSurface>,
    keyboard_on: String,
    pointer: Option<(WlSurface, f64, f64)>,
}

fn main() {
    let conn = Connection::connect_to_env().expect("no Wayland display");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let mut state = State {
        compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
        shm: globals.bind(&qh, 1..=1, ()).unwrap(),
        wm_base: globals.bind(&qh, 1..=6, ()).unwrap(),
        seat: globals.bind(&qh, 1..=7, ()).unwrap(),
        presentation: globals.bind(&qh, 1..=1, ()).unwrap(),
        viewporter: globals.bind(&qh, 1..=1, ()).unwrap(),
        fractional: globals.bind(&qh, 1..=1, ()).unwrap(),
        outputs: Vec::new(),
        popup: None,
        keyboard: None,
        keyboard_on: "nothing".into(),
        pointer: None,
    };
    globals.contents().with_list(|list| {
        for g in list.iter().filter(|g| g.interface == "wl_output") {
            state.add_output(globals.registry(), g.name, g.version, &qh);
        }
    });
    println!("started");
    // SIGUSR1, read through a signalfd so the loop below sees it.
    // Safety: plain libc calls on a local signal set; the fd is owned from here on.
    let signals = unsafe {
        let mut set = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        OwnedFd::from_raw_fd(libc::signalfd(-1, &set, libc::SFD_CLOEXEC))
    };
    loop {
        queue.dispatch_pending(&mut state).unwrap();
        queue.flush().unwrap();
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let mut fds =
            [guard.connection_fd().as_raw_fd(), signals.as_raw_fd()].map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        // Safety: two valid pollfds.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if fds[0].revents != 0 {
            // WouldBlock on a partial message; a lost connection fails at the next flush.
            let _ = guard.read();
        }
        if fds[1].revents != 0 {
            let mut info = [0u8; 128];
            // Safety: reads one signalfd_siginfo into a buffer of its size.
            unsafe { libc::read(signals.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) };
            state.recolour(&qh);
        }
    }
}

impl State {
    fn recolour(&mut self, qh: &QueueHandle<Self>) {
        println!("recoloured");
        for (surface, _) in self.outputs.iter_mut().filter_map(|o| o.toplevel.as_mut()) {
            surface.colour ^= 0x00ff_ffff;
            if surface.size.0 > 0 {
                draw(surface, &self.shm, &self.presentation, qh);
            }
        }
    }

    fn add_output(
        &mut self,
        registry: &wl_registry::WlRegistry,
        global: u32,
        version: u32,
        qh: &QueueHandle<Self>,
    ) {
        let wl = registry.bind(global, version.min(4), qh, ());
        self.outputs.push(Output {
            global,
            wl,
            name: String::new(),
            gone: false,
            toplevel: None,
        });
    }

    fn new_surface(&self, qh: &QueueHandle<Self>, colour: u32, label: &str) -> Surface {
        let surface = self.compositor.create_surface(qh, ());
        Surface {
            xdg: self.wm_base.get_xdg_surface(&surface, qh, ()),
            viewport: self.viewporter.get_viewport(&surface, qh, ()),
            _scale: self
                .fractional
                .get_fractional_scale(&surface, qh, surface.clone()),
            surface,
            size: (0, 0),
            scale120: 120,
            colour,
            label: label.into(),
        }
    }

    fn surface_mut(&mut self, wl: &WlSurface) -> Option<&mut Surface> {
        self.outputs
            .iter_mut()
            .filter_map(|o| o.toplevel.as_mut().map(|t| &mut t.0))
            .chain(self.popup.as_mut().map(|p| &mut p.surface))
            .find(|s| &s.surface == wl)
    }

    fn label(&self, wl: &WlSurface) -> String {
        self.outputs
            .iter()
            .filter_map(|o| o.toplevel.as_ref().map(|t| &t.0))
            .chain(self.popup.as_ref().map(|p| &p.surface))
            .find(|s| &s.surface == wl)
            .map_or("?".into(), |s| s.label.clone())
    }

    fn open_popup(&mut self, serial: u32, qh: &QueueHandle<Self>) {
        let Some(parent) = self
            .outputs
            .iter()
            .filter_map(|o| o.toplevel.as_ref().map(|t| &t.0))
            .find(|s| Some(&s.surface) == self.keyboard.as_ref())
        else {
            return;
        };
        let positioner = self.wm_base.create_positioner(qh, ());
        positioner.set_size(200, 100);
        positioner.set_anchor_rect(parent.size.0 - 50, 100, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        positioner.set_constraint_adjustment(
            xdg_positioner::ConstraintAdjustment::SlideX
                | xdg_positioner::ConstraintAdjustment::SlideY,
        );
        let surface = self.new_surface(qh, POPUP_COLOUR, "popup");
        let popup = surface
            .xdg
            .get_popup(Some(&parent.xdg), &positioner, qh, ());
        popup.grab(&self.seat, serial);
        surface.surface.commit();
        positioner.destroy();
        self.popup = Some(Popup { surface, popup });
    }
}

/// Draws a surface's colour at its preferred scale and commits it.
fn draw(
    surface: &Surface,
    shm: &wl_shm::WlShm,
    presentation: &WpPresentation,
    qh: &QueueHandle<State>,
) {
    let (w, h) = (
        (surface.size.0 as u32 * surface.scale120).div_ceil(120) as i32,
        (surface.size.1 as u32 * surface.scale120).div_ceil(120) as i32,
    );
    let len = (w * h * 4) as usize;
    // Safety: memfd_create with a valid name; the fd is owned from here on.
    let fd = unsafe { libc::memfd_create(c"test-client".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create");
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let pixels: Vec<u8> = std::iter::repeat_n(surface.colour.to_ne_bytes(), (w * h) as usize)
        .flatten()
        .collect();
    file.write_all(&pixels).unwrap();
    let pool: WlShmPool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    surface
        .viewport
        .set_destination(surface.size.0, surface.size.1);
    surface.surface.attach(Some(&buffer), 0, 0);
    surface.surface.damage_buffer(0, 0, w, h);
    presentation.feedback(&surface.surface, qh, surface.label.clone());
    surface.surface.commit();
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                state.add_output(registry, name, version, qh);
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.iter_mut().find(|o| o.global == name) {
                    output.gone = true;
                    println!("output removed {}", output.name);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        wl: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let Some(i) = state.outputs.iter().position(|o| &o.wl == wl) else {
            return;
        };
        match event {
            wl_output::Event::Name { name } => state.outputs[i].name = name,
            // Everything about the output is known: cover it.
            wl_output::Event::Done if state.outputs[i].toplevel.is_none() => {
                let name = state.outputs[i].name.clone();
                println!("output added {name}");
                let orphan = state
                    .outputs
                    .iter_mut()
                    .find_map(|o| o.toplevel.take_if(|_| o.gone));
                let (surface, toplevel) = orphan.unwrap_or_else(|| {
                    let surface = state.new_surface(qh, COLOURS[i % COLOURS.len()], &name);
                    let toplevel = surface.xdg.get_toplevel(qh, ());
                    (surface, toplevel)
                });
                toplevel.set_fullscreen(Some(wl));
                surface.surface.commit();
                state.outputs[i].toplevel = Some((surface, toplevel));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let xdg_surface::Event::Configure { serial } = event else {
            return;
        };
        xdg.ack_configure(serial);
        let (shm, presentation) = (state.shm.clone(), state.presentation.clone());
        let surface = state
            .outputs
            .iter()
            .filter_map(|o| o.toplevel.as_ref().map(|t| &t.0))
            .chain(state.popup.as_ref().map(|p| &p.surface))
            .find(|s| &s.xdg == xdg);
        if let Some(surface) = surface.filter(|s| s.size.0 > 0) {
            println!(
                "configure {} {}x{} scale {}",
                surface.label, surface.size.0, surface.size.1, surface.scale120
            );
            draw(surface, &shm, &presentation, qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        toplevel: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event
            && let Some(o) = state
                .outputs
                .iter_mut()
                .find(|o| o.toplevel.as_ref().is_some_and(|t| &t.1 == toplevel))
        {
            o.toplevel.as_mut().unwrap().0.size = (width, height);
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_popup::XdgPopup,
        event: xdg_popup::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_popup::Event::Configure {
                x,
                y,
                width,
                height,
            } => {
                println!("popup at {x},{y} {width}x{height}");
                if let Some(popup) = &mut state.popup {
                    popup.surface.size = (width, height);
                }
            }
            xdg_popup::Event::PopupDone => {
                println!("popup_done");
                if let Some(popup) = state.popup.take() {
                    popup.popup.destroy();
                    popup.surface.xdg.destroy();
                    popup.surface.surface.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, WlSurface> for State {
    fn event(
        state: &mut Self,
        _: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        wl: &WlSurface,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let (shm, presentation) = (state.shm.clone(), state.presentation.clone());
        if let Some(surface) = state.surface_mut(wl)
            && surface.scale120 != scale
        {
            surface.scale120 = scale;
            if surface.size.0 > 0 {
                println!("rescale {} {scale}", surface.label);
                draw(surface, &shm, &presentation, qh);
            }
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, String> for State {
    fn event(
        _: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        label: &String,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => println!("presented {label}"),
            wp_presentation_feedback::Event::Discarded => println!("discarded {label}"),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if caps.contains(wl_seat::Capability::Pointer) {
                seat.get_pointer(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => {
                state.keyboard_on = state.label(&surface);
                println!("keyboard on {}", state.keyboard_on);
                // Only toplevels are remembered: the popup's parent.
                if state
                    .popup
                    .as_ref()
                    .is_none_or(|p| p.surface.surface != surface)
                {
                    state.keyboard = Some(surface);
                }
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                let pressed = key_state == wl_keyboard::KeyState::Pressed;
                let action = if pressed { "pressed" } else { "released" };
                println!("key {key} {action} on {}", state.keyboard_on);
                match key {
                    KEY_P if pressed && state.popup.is_none() => state.open_popup(serial, qh),
                    KEY_Q if pressed => std::process::exit(0),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer = Some((surface, surface_x, surface_y));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(p) = &mut state.pointer {
                    (p.1, p.2) = (surface_x, surface_y);
                }
            }
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(button_state),
                ..
            } => {
                let (on, x, y) = match &state.pointer {
                    Some((surface, x, y)) => (state.label(surface), *x, *y),
                    None => ("nothing".into(), 0.0, 0.0),
                };
                let pressed = button_state == wl_pointer::ButtonState::Pressed;
                println!(
                    "button {button} {} on {on} at {x:.0},{y:.0}",
                    if pressed { "pressed" } else { "released" }
                );
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &WlBuffer,
        event: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

delegate_noop!(State: ignore WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WpPresentation);
delegate_noop!(State: ignore WpViewporter);
delegate_noop!(State: ignore WpViewport);
delegate_noop!(State: ignore WpFractionalScaleManagerV1);
delegate_noop!(State: ignore xdg_positioner::XdgPositioner);
