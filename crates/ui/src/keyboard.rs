use std::time::Duration;

use anyhow::Context;
use tokio::time::Instant;
use tracing::error;
use wayland_client::WEnum;
use wayland_client::protocol::wl_keyboard::{self, KeyState, KeymapFormat};
use xkbcommon::xkb;

use crate::entry::{Entry, Key, Repeat};

/// A keyboard's keymap and modifiers, which turn its events into keys for an `Entry`.
///
/// With a layout of our own, the modifiers follow its keys, not the compositor's: the
/// compositor's say which of its own keymap's modifiers are held, and its keys may differ
/// (the right Alt is Alt in "us" but AltGr in "de"). So Caps Lock or Num Lock locked before
/// the keyboard came is not known.
pub struct Keyboard {
    xkb: xkb::Context,
    /// The keymap and modifiers, once the compositor has sent the keymap.
    xkb_state: Option<xkb::State>,
    /// A keymap of our own, used in place of the compositor's.
    own: Option<xkb::Keymap>,
    /// The keys held, by our own keymap's state, to hold them in a new one.
    held: Vec<xkb::Keycode>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Self {
            xkb: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            own: None,
            held: Vec::new(),
        }
    }
}

impl Keyboard {
    /// A keyboard that types with the xkb `layout` (`layout` or `layout(variant)`, with the
    /// default rules and model) whatever keymap the compositor sends.
    pub fn with_layout(layout: &str) -> anyhow::Result<Self> {
        let mut keyboard = Self::default();
        keyboard.own = Some(keyboard.compile(layout)?);
        Ok(keyboard)
    }

    /// Type with `layout` from now on, as `with_layout` has it, with the keys held still
    /// held and Caps Lock as it is. What `entry` has typed stays as it was typed.
    pub fn set_layout(&mut self, layout: &str, entry: &mut Entry) -> anyhow::Result<()> {
        self.own = Some(self.compile(layout)?);
        self.renew(entry);
        Ok(())
    }

    /// Follow the key with evdev `code`, pressed for something else than `entry` (a popup,
    /// the focus): its modifier only. The compositor's keymap needs nothing: its modifiers
    /// come in their own events.
    pub fn pressed_elsewhere(&mut self, code: u32, entry: &mut Entry) {
        self.follow(code, xkb::KeyDirection::Down, entry);
    }

    /// A state of our own keymap with the locked modifiers of the old one, and the keys
    /// `held` pressed again, but for lock keys: their lock is already carried over, and a
    /// second press would unlock it on release.
    fn renew(&mut self, entry: &mut Entry) {
        let (Some(own), Some(old)) = (&self.own, &self.xkb_state) else {
            return;
        };
        let mut state = xkb::State::new(own);
        let locked = old.serialize_mods(xkb::STATE_MODS_LOCKED);
        state.update_mask(0, 0, locked, 0, 0, 0);
        for &key in &self.held {
            let lock = matches!(
                old.key_get_one_sym(key),
                xkb::Keysym::Caps_Lock | xkb::Keysym::Num_Lock
            );
            if !lock {
                state.update_key(key, xkb::KeyDirection::Down);
            }
        }
        entry.caps_lock = state.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
        self.xkb_state = Some(state);
    }

    /// With our own keymap, follow the key with evdev `code` going `direction`.
    fn follow(&mut self, code: u32, direction: xkb::KeyDirection, entry: &mut Entry) {
        let (Some(_), Some(xkb)) = (&self.own, &mut self.xkb_state) else {
            return;
        };
        let key = xkb::Keycode::new(code + 8);
        self.held.retain(|&held| held != key);
        if matches!(direction, xkb::KeyDirection::Down) {
            self.held.push(key);
        }
        xkb.update_key(key, direction);
        entry.caps_lock = xkb.mod_name_is_active(xkb::MOD_NAME_CAPS, xkb::STATE_MODS_EFFECTIVE);
    }

    fn compile(&self, layout: &str) -> anyhow::Result<xkb::Keymap> {
        let (name, variant) = split_layout(layout)?;
        let flags = xkb::COMPILE_NO_FLAGS;
        let keymap = xkb::Keymap::new_from_names(&self.xkb, "", "", name, variant, None, flags);
        keymap.with_context(|| format!("xkbcommon cannot compile the layout {layout:?}"))
    }

    /// The keysym of the key with evdev `code`, with the modifiers in effect.
    pub fn keysym(&self, code: u32) -> Option<xkb::Keysym> {
        let xkb = self.xkb_state.as_ref()?;
        Some(xkb.key_get_one_sym(xkb::Keycode::new(code + 8)))
    }

    /// What the key with evdev `code` means to an `Entry`, with the modifiers in effect.
    pub fn key(&self, code: u32) -> Option<Key> {
        let xkb = self.xkb_state.as_ref()?;
        // xkbcommon numbers keys from 8.
        let keycode = xkb::Keycode::new(code + 8);
        let alt_or_logo = [xkb::MOD_NAME_ALT, xkb::MOD_NAME_LOGO]
            .iter()
            .any(|name| xkb.mod_name_is_active(name, xkb::STATE_MODS_EFFECTIVE));
        let ctrl = xkb.mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE);
        Key::from_xkb(
            xkb.key_get_one_sym(keycode),
            xkb.key_get_utf32(keycode),
            alt_or_logo,
            ctrl,
        )
    }

    /// Act on the key with evdev `code`, pressed; returns whether it submitted the password.
    fn press(&self, entry: &mut Entry, code: u32) -> bool {
        let key = self.key(code);
        key.is_some_and(|key| entry.press(key, code, Instant::now()))
    }

    /// Act on a `wl_keyboard` event for `entry`; returns whether it submitted the password.
    pub fn handle(&mut self, wl_event: wl_keyboard::Event, entry: &mut Entry) -> bool {
        match wl_event {
            // The compositor's keymap is not compiled; its fd is closed when dropped.
            wl_keyboard::Event::Keymap { .. } if let Some(own) = &self.own => {
                self.xkb_state = Some(xkb::State::new(own));
                entry.caps_lock = false;
            }
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
            // The release may go elsewhere; so may a modifier's, which is forgotten.
            wl_keyboard::Event::Leave { .. } => {
                entry.stop_repeat();
                self.held.clear();
                self.renew(entry);
            }
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                let (submitted, direction) = match key_state {
                    KeyState::Pressed => (self.press(entry, key), xkb::KeyDirection::Down),
                    KeyState::Released => {
                        entry.release(key);
                        (false, xkb::KeyDirection::Up)
                    }
                    // Only sent for wl_seat version 10; we bind up to 9 and repeat keys
                    // ourselves.
                    _ => return false,
                };
                self.follow(key, direction, entry);
                return submitted;
            }
            wl_keyboard::Event::Modifiers { .. } if self.own.is_some() => {}
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

/// An xkb layout as written, `layout` or `layout(variant)`: its layout and variant (empty
/// for none). Their names are letters, digits, `_` and `-`.
fn split_layout(layout: &str) -> anyhow::Result<(&str, &str)> {
    let valid = |name: &str| {
        let mut chars = name.chars();
        !name.is_empty() && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    let split = match layout.split_once('(') {
        Some((name, rest)) => rest.strip_suffix(')').map(|variant| (name, variant)),
        None => Some((layout, "")),
    };
    let split =
        split.filter(|&(name, variant)| valid(name) && (!layout.contains('(') || valid(variant)));
    split.with_context(|| format!("{layout:?} is not a layout or layout(variant)"))
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use wayland_client::protocol::wl_keyboard::Event;

    use super::*;

    // evdev codes.
    const Q: u32 = 16;
    const S: u32 = 31;
    const CAPS_LOCK: u32 = 58;
    const RIGHT_ALT: u32 = 100;
    const LEFT_SHIFT: u32 = 42;

    fn key(key: u32, pressed: bool) -> Event {
        let state = if pressed {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        Event::Key {
            serial: 0,
            time: 0,
            key,
            state: WEnum::Value(state),
        }
    }

    /// `keyboard` given the compositor's keymap (unread with a layout of our own).
    fn ready(mut keyboard: Keyboard, entry: &mut Entry) -> Keyboard {
        let fd = File::open("/dev/null").unwrap().into();
        let keymap = Event::Keymap {
            format: WEnum::Value(KeymapFormat::XkbV1),
            fd,
            size: 1,
        };
        keyboard.handle(keymap, entry);
        keyboard
    }

    #[test]
    fn altgr_types_with_our_own_layout_whatever_the_compositors_modifiers() {
        let mut entry = Entry::new();
        let mut keyboard = ready(Keyboard::with_layout("de").unwrap(), &mut entry);
        // The compositor's "us" keymap has the right Alt as Alt (Mod1), and says so.
        keyboard.handle(key(RIGHT_ALT, true), &mut entry);
        let alt = Event::Modifiers {
            serial: 0,
            mods_depressed: 1 << 3,
            mods_latched: 0,
            mods_locked: 0,
            group: 0,
        };
        keyboard.handle(alt, &mut entry);
        keyboard.handle(key(Q, true), &mut entry);
        keyboard.handle(key(Q, false), &mut entry);
        keyboard.handle(key(RIGHT_ALT, false), &mut entry);
        keyboard.handle(key(Q, true), &mut entry);
        assert_eq!(entry.typed(), b"@q");
    }

    #[test]
    fn a_variant_is_compiled_and_a_new_layout_keeps_caps_lock() {
        let mut entry = Entry::new();
        let mut keyboard = ready(Keyboard::with_layout("us(dvorak)").unwrap(), &mut entry);
        keyboard.handle(key(S, true), &mut entry);
        keyboard.handle(key(CAPS_LOCK, true), &mut entry);
        keyboard.handle(key(CAPS_LOCK, false), &mut entry);
        assert!(entry.caps_lock);
        keyboard.set_layout("de", &mut entry).unwrap();
        assert!(entry.caps_lock);
        keyboard.handle(key(S, true), &mut entry);
        assert_eq!(entry.typed(), b"oS");
        assert!(keyboard.set_layout("no-such-layout", &mut entry).is_err());
    }

    #[test]
    fn layouts_split_into_layout_and_variant() {
        assert_eq!(split_layout("de").unwrap(), ("de", ""));
        assert_eq!(
            split_layout("de(nodeadkeys)").unwrap(),
            ("de", "nodeadkeys")
        );
        for bad in [
            "", "(x)", "de(", "de()", "de(x)y", "de(x(y))", "d e", "us,de",
        ] {
            assert!(split_layout(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_key_held_stays_held_in_a_new_layout_until_released() {
        let mut entry = Entry::new();
        let mut keyboard = ready(Keyboard::with_layout("us").unwrap(), &mut entry);
        keyboard.handle(key(LEFT_SHIFT, true), &mut entry);
        keyboard.set_layout("de", &mut entry).unwrap();
        keyboard.handle(key(S, true), &mut entry);
        keyboard.handle(key(S, false), &mut entry);
        keyboard.handle(key(LEFT_SHIFT, false), &mut entry);
        // Released, it is not held again by the next layout.
        keyboard.set_layout("us", &mut entry).unwrap();
        keyboard.handle(key(S, true), &mut entry);
        assert_eq!(entry.typed(), b"Ss");
    }

    #[test]
    fn caps_lock_held_through_a_new_layout_stays_locked() {
        let mut entry = Entry::new();
        let mut keyboard = ready(Keyboard::with_layout("us").unwrap(), &mut entry);
        keyboard.handle(key(CAPS_LOCK, true), &mut entry);
        keyboard.set_layout("de", &mut entry).unwrap();
        keyboard.handle(key(CAPS_LOCK, false), &mut entry);
        assert!(entry.caps_lock);
        keyboard.handle(key(S, true), &mut entry);
        assert_eq!(entry.typed(), b"S");
    }

    #[test]
    fn a_modifier_pressed_for_a_popup_still_counts() {
        let mut entry = Entry::new();
        let mut keyboard = ready(Keyboard::with_layout("us").unwrap(), &mut entry);
        keyboard.pressed_elsewhere(CAPS_LOCK, &mut entry);
        keyboard.handle(key(CAPS_LOCK, false), &mut entry);
        assert!(entry.caps_lock);
        keyboard.handle(key(S, true), &mut entry);
        assert_eq!(entry.typed(), b"S");
    }
}
