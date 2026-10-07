use std::time::Duration;

use tokio::time::Instant;
use tracing::error;
use wayland_client::WEnum;
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat};
use xkbcommon::xkb;

use crate::entry::{Entry, Key, Repeat};

/// A keyboard's keymap and modifiers, which turn its events into keys for an `Entry`.
pub struct Keyboard {
    xkb: xkb::Context,
    /// The keymap and modifiers, once the compositor has sent the keymap.
    xkb_state: Option<xkb::State>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
        }
    }
}

impl Keyboard {
    /// Act on the key with evdev `code`, pressed; returns whether it submitted the password.
    fn press(&self, entry: &mut Entry, code: u32) -> bool {
        let Some(xkb) = &self.xkb_state else {
            return false;
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
        key.is_some_and(|key| entry.press(key, code, Instant::now()))
    }

    /// Act on a `wl_keyboard` event for `entry`; returns whether it submitted the password.
    pub fn handle(&mut self, wl_event: wl_keyboard::Event, entry: &mut Entry) -> bool {
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
                        &self.xkb,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::COMPILE_NO_FLAGS,
                    )
                };
                match keymap {
                    Ok(Some(keymap)) => {
                        // A new state starts with no modifiers; the next `modifiers` sets them.
                        self.xkb_state = Some(xkb::State::new(&keymap));
                        entry.caps_lock = false;
                    }
                    Ok(None) => error!("the compositor sent a keymap xkbcommon cannot compile"),
                    Err(e) => error!("reading the keymap: {e}"),
                }
            }
            // The release may go elsewhere.
            wl_keyboard::Event::Leave { .. } => entry.stop_repeat(),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => match key_state {
                KeyState::Pressed => return self.press(entry, key),
                KeyState::Released => entry.release(key),
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
                if let Some(xkb) = &mut self.xkb_state {
                    xkb.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                    entry.caps_lock =
                        xkb.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
                }
            }
            // A rate of 0 means no repeat; clamped so a bad compositor can't panic or spin us.
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                entry.set_repeat((rate > 0).then(|| Repeat {
                    delay: Duration::from_millis(delay.max(0) as u64),
                    interval: Duration::from_secs(1) / rate.min(1000) as u32,
                }))
            }
            _ => {}
        }
        false
    }
}
