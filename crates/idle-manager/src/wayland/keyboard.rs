use wayland_client::protocol::wl_keyboard::{self, KeyState, WlKeyboard};
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use super::State;

/// The lock screen takes the first keyboard the seat offers.
impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        wl_event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let (
            wl_seat::Event::Capabilities {
                capabilities: WEnum::Value(capabilities),
            },
            Some(screen),
        ) = (wl_event, &mut state.lock_screen)
        else {
            return;
        };
        let has_keyboard = capabilities.contains(Capability::Keyboard);
        if has_keyboard && screen.keyboard.is_none() {
            screen.keyboard = Some(seat.get_keyboard(qh, ()));
        } else if !has_keyboard && let Some(keyboard) = screen.keyboard.take() {
            keyboard.release();
            screen.entry.stop_repeat();
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
        _: &QueueHandle<Self>,
    ) {
        let screen = state.screen();
        // A press counts only while locked, so it is meant for us.
        let pressed = matches!(
            wl_event,
            wl_keyboard::Event::Key {
                state: WEnum::Value(KeyState::Pressed),
                ..
            }
        );
        if pressed && !screen.is_locked() {
            return;
        }
        if screen.xkb.handle(wl_event, &mut screen.entry) {
            screen.submitted = true;
        }
    }
}
