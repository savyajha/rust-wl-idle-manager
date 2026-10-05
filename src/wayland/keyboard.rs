use std::time::{Duration, Instant};

use tracing::error;
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat, WlKeyboard};
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use xkbcommon::xkb;

use super::{State, Wayland};
use crate::password::{Entry, Key};
use crate::policy::Input;

impl Wayland {
    /// The password typed on the lock screen; changes show on the next `next`.
    pub fn entry_mut(&mut self) -> &mut Entry {
        &mut self.state.entry
    }
}

impl State {
    /// Act on the key with evdev code `code`, pressed. Only while locked: the lock screen
    /// is the daemon's only surface, so there is nothing else to type into.
    fn press(&mut self, code: u32) {
        let Some(xkb) = self.lock.as_ref().and(self.xkb_state.as_ref()) else {
            return;
        };
        // xkbcommon numbers keys from 8; the character comes straight as UTF-32.
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
            self.inputs.push_back(Input::PasswordEntered);
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
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = wl_event
        else {
            return;
        };
        let has_keyboard = capabilities.contains(Capability::Keyboard);
        if has_keyboard && state.lock_screen && state.keyboard.is_none() {
            state.keyboard = Some(seat.get_keyboard(qh, ()));
        } else if !has_keyboard && let Some(keyboard) = state.keyboard.take() {
            keyboard.release();
            state.entry.stop_repeat();
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
                        &state.xkb,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(keymap)) => {
                        // A new state starts with no modifiers; the next `modifiers` sets them.
                        state.xkb_state = Some(xkb::State::new(&keymap));
                        state.entry.caps_lock = false;
                    }
                    Ok(None) => error!("the compositor sent a keymap xkbcommon cannot compile"),
                    Err(e) => error!("reading the keymap: {e}"),
                }
            }
            // The release may go elsewhere.
            wl_keyboard::Event::Leave { .. } => state.entry.stop_repeat(),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => match key_state {
                KeyState::Pressed => state.press(key),
                KeyState::Released => state.entry.release(key),
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
                if let Some(xkb) = &mut state.xkb_state {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    state.entry.caps_lock =
                        xkb.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
                }
            }
            // A rate of 0 means no repeat; clamped so a bad compositor can't panic or spin us.
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state.entry.set_repeat((rate > 0).then(|| {
                    (
                        Duration::from_millis(delay.max(0) as u64),
                        rate.min(1000) as u32,
                    )
                }))
            }
            _ => {}
        }
    }
}
