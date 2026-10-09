//! The protocols: wl_compositor (and the wl_subcompositor Smithay adds with it), wl_shm,
//! wl_seat, wl_output, xdg_wm_base, wp_fractional_scale_v1, wp_viewporter and wp_presentation.
//!
//! Every toplevel is fullscreen on the output it asks for, or else the primary one. Popups are
//! placed by their positioner, kept on the output, and drawn above their parent. A popup grab
//! gives the topmost grabbing popup the keyboard; Escape or a click outside the grabbing popups
//! dismisses them (see input.rs).

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{
    PopupKind, Window, WindowSurfaceType, find_popup_root_surface, get_popup_toplevel_coords,
};
use smithay::input::{SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, send_surface_state, with_states,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_fractional_scale, delegate_output, delegate_presentation,
    delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_shell,
};

use crate::{ClientState, State, Toplevel};

impl State {
    /// Configures toplevel `i` fullscreen on its output (the primary if it has none) and maps it
    /// there. Without any output it is configured with no size and stays unmapped.
    pub fn place(&mut self, i: usize) {
        let primary = self.gpu.heads.first().map(|h| h.output.clone());
        if self.toplevels[i].output.is_none() {
            self.toplevels[i].output = primary;
        }
        // Before the configure, so the client draws its first frame at the right scale.
        self.send_scale(i);
        let toplevel = &self.toplevels[i];
        let geometry = toplevel
            .output
            .as_ref()
            .and_then(|o| self.space.output_geometry(o));
        let surface = toplevel.window.toplevel().unwrap();
        surface.with_pending_state(|s| {
            s.size = geometry.map(|g| g.size);
            s.states.set(xdg_toplevel::State::Fullscreen);
            s.states.set(xdg_toplevel::State::Activated);
        });
        if surface.is_initial_configure_sent() {
            surface.send_pending_configure();
        } else {
            surface.send_configure();
        }
        if let Some(geometry) = geometry {
            self.space
                .map_element(toplevel.window.clone(), geometry.loc, false);
        }
        self.refocus();
    }

    /// The output's scale, as the preferred scale of every surface of toplevel `i` and its popups.
    fn send_scale(&self, i: usize) {
        let toplevel = &self.toplevels[i];
        let Some(output) = &toplevel.output else {
            return;
        };
        let scale = output.current_scale();
        toplevel.window.with_surfaces(|surface, states| {
            with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale.fractional_scale())
            });
            send_surface_state(
                surface,
                states,
                scale.integer_scale(),
                output.current_transform(),
            );
        });
    }

    fn toplevel_index(&self, root: &WlSurface) -> Option<usize> {
        self.toplevels
            .iter()
            .position(|t| t.window.toplevel().unwrap().wl_surface() == root)
    }

    /// Gives the keyboard to the topmost grabbing popup, else the clicked toplevel, else the
    /// primary output's toplevel, else the first one.
    pub fn refocus(&mut self) {
        self.grab.retain(|p| p.alive());
        let primary = self.gpu.heads.first().map(|h| &h.output);
        let toplevel = self
            .toplevels
            .iter()
            .find(|t| Some(&t.window) == self.focus.as_ref())
            .or_else(|| {
                // The primary output's own toplevel before one moved there from an unplugged
                // output, whichever was created first.
                let mut on_primary = self
                    .toplevels
                    .iter()
                    .filter(|t| t.output.as_ref() == primary);
                on_primary.clone().find(|t| !t.moved).or(on_primary.next())
            })
            .or(self.toplevels.first());
        // On top, too: a toplevel moved off an unplugged output must not cover it.
        if let Some(t) = toplevel {
            self.space.raise_element(&t.window, false);
        }
        let target = match self.grab.last() {
            Some(popup) => Some(popup.wl_surface().clone()),
            None => toplevel.map(|t| t.window.toplevel().unwrap().wl_surface().clone()),
        };
        let keyboard = self.seat.get_keyboard().unwrap();
        if keyboard.current_focus() != target {
            keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
        }
    }

    /// Sends popup_done to the topmost grabbing popup, or all of them.
    pub fn dismiss_popups(&mut self, all: bool) {
        while let Some(popup) = self.grab.pop() {
            popup.send_popup_done();
            if !all {
                break;
            }
        }
        self.refocus();
    }

    /// The surface under a point of the layout, popups first, and its origin there.
    pub fn surface_under(
        &self,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (window, location) = self.space.element_under(point)?;
        let (surface, origin) =
            window.surface_under(point - location.to_f64(), WindowSurfaceType::ALL)?;
        Some((surface, (origin + location).to_f64()))
    }

    /// Places a popup by its positioner, moved or flipped as the positioner allows so that it
    /// stays on its toplevel's output.
    fn unconstrain_popup(&self, popup: &PopupSurface, positioner: &PositionerState) {
        let kind = PopupKind::Xdg(popup.clone());
        let target = find_popup_root_surface(&kind)
            .ok()
            .and_then(|root| self.toplevel_index(&root))
            .and_then(|i| {
                let toplevel = &self.toplevels[i];
                let window = self.space.element_geometry(&toplevel.window)?;
                let mut target = self.space.output_geometry(toplevel.output.as_ref()?)?;
                // Relative to the popup's parent.
                target.loc -= get_popup_toplevel_coords(&kind) + window.loc;
                Some(target)
            });
        let geometry = match target {
            Some(target) => positioner.get_unconstrained_geometry(target),
            None => positioner.get_geometry(),
        };
        popup.with_pending_state(|s| s.geometry = geometry);
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.popups.commit(surface);

        if let Some(i) = self.toplevel_index(&crate::input::root(surface.clone())) {
            self.toplevels[i].window.on_commit();
            let toplevel = self.toplevels[i].window.toplevel().unwrap();
            if !toplevel.is_initial_configure_sent() {
                self.place(i);
            }
        }
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                // Cannot fail: the initial configure was not sent.
                let _ = popup.send_configure();
            }
            if let Some(i) = find_popup_root_surface(&PopupKind::Xdg(popup))
                .ok()
                .and_then(|root| self.toplevel_index(&root))
            {
                self.send_scale(i);
            }
        }
        // Outputs and enter/leave from the new size, then draw what changed.
        self.space.refresh();
        self.render_all();
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _: &WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl OutputHandler for State {}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    /// Configured on its first commit, after any set_fullscreen.
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.toplevels.push(Toplevel {
            window: Window::new_wayland_window(surface),
            output: None,
            moved: false,
        });
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let Some(i) = self.toplevel_index(surface.wl_surface()) else {
            return;
        };
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .filter(|o| self.gpu.heads.iter().any(|h| &h.output == o));
        if output.is_some() {
            self.toplevels[i].output = output;
            self.toplevels[i].moved = false;
        }
        if surface.is_initial_configure_sent() {
            self.place(i);
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(i) = self.toplevel_index(surface.wl_surface()) {
            let toplevel = self.toplevels.remove(i);
            self.space.unmap_elem(&toplevel.window);
            self.refocus();
            self.render_all();
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        self.unconstrain_popup(&surface, &positioner);
        if let Err(e) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("tracking a popup: {e}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|s| s.positioner = positioner);
        self.unconstrain_popup(&surface, &positioner);
        surface.send_repositioned(token);
    }

    /// Granted to a popup on a toplevel, or on the topmost grabbing popup (the protocol's rule);
    /// any other is dismissed at once. The serial is not checked: there is one client.
    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        let parent = surface.get_parent_surface();
        let allowed = match self.grab.last() {
            Some(top) => parent.as_ref() == Some(top.wl_surface()),
            None => parent.is_some_and(|p| self.toplevel_index(&p).is_some()),
        };
        if allowed {
            self.grab.push(surface);
            self.refocus();
        } else {
            surface.send_popup_done();
        }
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        self.grab.retain(|p| p != &surface);
        self.refocus();
        self.render_all();
    }
}

impl FractionalScaleHandler for State {
    /// The scale is sent when the surface gets a role on an output (`send_scale`); until then,
    /// the primary output's.
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let Some(head) = self.gpu.heads.first() else {
            return;
        };
        let scale = head.output.current_scale().fractional_scale();
        with_states(&surface, |states| {
            with_fractional_scale(states, |fs| fs.set_preferred_scale(scale));
        });
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_xdg_shell!(State);
delegate_fractional_scale!(State);
delegate_viewporter!(State);
delegate_presentation!(State);
