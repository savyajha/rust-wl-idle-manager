use std::time::Duration;

use tokio::time::Instant;
use tracing::error;
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard};
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

use super::State;
use super::lock::Screen;
use crate::entry::{Key, Repeat};

impl Screen {
    /// Act on the key with evdev `code`, pressed; only while locked, so it is meant for us.
    fn press(&mut self, code: u32) {
        let Some(xkb) = self.xkb_state.as_ref().filter(|_| self.is_locked()) else {
            return;
        };
        // xkbcommon numbers keys from 8.
        let keycode = xkb::Keycode::new(code + 8);
        let alt_or_logo = [xkb::MOD_NAME_ALT, xkb::MOD_NAME_LOGO]
            .iter()
            .any(|name| xkb.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE));
        let key = Key::from_xkb(
            xkb.key_get_one_sym(keycode),
            xkb.key_get_utf32(keycode),
            alt_or_logo,
        );
        if let Some(key) = key
            && self.entry.press(key, code, Instant::now())
        {
            self.submitted = true;
        }
    }
}

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
        match wl_event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(KeymapFormat::XkbV1),
                fd,
                size,
            } if size > 0 => {
                // SAFETY: the compositor sends an fd holding a keymap of `size` bytes,
                // which xkbcommon maps read-only (private, as wl_keyboard v7 requires).
                let keymap = unsafe {
                    xkb::Keymap::new_from_fd(
                        &screen.xkb,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(keymap)) => {
                        // A new state starts with no modifiers; the next `modifiers` sets them.
                        screen.xkb_state = Some(xkb::State::new(&keymap));
                        screen.entry.caps_lock = false;
                    }
                    Ok(None) => error!("the compositor sent a keymap xkbcommon cannot compile"),
                    Err(e) => error!("reading the keymap: {e}"),
                }
            }
            // The release may go elsewhere.
            wl_keyboard::Event::Leave { .. } => screen.entry.stop_repeat(),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => match key_state {
                KeyState::Pressed => screen.press(key),
                KeyState::Released => screen.entry.release(key),
                // Only sent for wl_seat version 10; we bind up to 9 and repeat keys ourselves.
                _ => {}
            },
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(xkb) = &mut screen.xkb_state {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    screen.entry.caps_lock =
                        xkb.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
                }
            }
            // A rate of 0 means no repeat; clamped so a bad compositor can't panic or spin us.
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                screen.entry.set_repeat((rate > 0).then(|| Repeat {
                    delay: Duration::from_millis(delay.max(0) as u64),
                    interval: Duration::from_secs(1) / rate.min(1000) as u32,
                }))
            }
            _ => {}
        }
    }
}
