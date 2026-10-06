use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::config::{Colour, ColourName, LockScreen};

/// Red, green, blue and alpha, each from 0 to 1, not premultiplied.
pub type Rgba = [f64; 4];

/// The error colour the default lock screen names, for when gtk.css lacks it.
const ERROR_COLOR: &str = "#ffb4ab";

/// GTK's named colours, as matugen writes them to `gtk-4.0/gtk.css`.
pub struct Palette(HashMap<String, Rgba>);

impl Palette {
    /// The colours gtk.css defines, over the built-in one; a missing file is logged. So is
    /// each colour `config` names that neither defines.
    pub fn load(config: &LockScreen) -> Self {
        let css = file("gtk.css").and_then(|path| {
            fs::read_to_string(&path)
                .inspect_err(|e| info!("no GTK colours from {}: {e}", path.display()))
                .ok()
        });
        let palette = Self::parse(&css.unwrap_or_default());
        for colour in config.colours() {
            if let ColourName::Named(name) = &colour.name
                && !palette.0.contains_key(name)
            {
                warn!("unknown colour {name:?}; using white");
            }
        }
        palette
    }

    /// The built-in colour, and those `css` defines in `@define-color <name> #hex;` lines.
    pub fn parse(css: &str) -> Self {
        let defined = css.lines().filter_map(|line| {
            let mut words = line.strip_prefix("@define-color")?.split_whitespace();
            let name = words.next()?.to_owned();
            Some((name, hex(words.next()?.strip_suffix(';')?)?))
        });
        let built_in = (
            "error_color".to_owned(),
            hex(ERROR_COLOR).expect("a colour"),
        );
        Self([built_in].into_iter().chain(defined).collect())
    }

    /// `colour`, with its opacity applied; an unknown name gives white.
    pub fn get(&self, colour: &Colour) -> Rgba {
        let [r, g, b, a] = match &colour.name {
            ColourName::Literal(rgba) => *rgba,
            ColourName::Named(name) => self.0.get(name).copied().unwrap_or([1.0; 4]),
        };
        [r, g, b, a * colour.alpha.0]
    }
}

/// `name` in GTK 4's config directory, `$XDG_CONFIG_HOME/gtk-4.0` (by default
/// `~/.config/gtk-4.0`); `None`, logged, if neither variable is set.
fn file(name: &str) -> Option<PathBuf> {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    if config.is_none() {
        warn!("neither XDG_CONFIG_HOME nor HOME is set; not reading gtk-4.0/{name}");
    }
    Some(config?.join("gtk-4.0").join(name))
}

/// GTK's default font, `gtk-font-name` in its settings.ini, or "Sans".
pub fn font() -> String {
    let settings = file("settings.ini").and_then(|path| fs::read_to_string(path).ok());
    font_in(&settings.unwrap_or_default())
        .unwrap_or("Sans")
        .to_owned()
}

/// The `gtk-font-name` in settings.ini's `text`.
fn font_in(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("gtk-font-name")?
                .trim()
                .strip_prefix('=')
        })
        .map(str::trim)
}

/// `#rrggbb` or `#rrggbbaa`.
pub fn hex(text: &str) -> Option<Rgba> {
    let digits = text.strip_prefix('#')?;
    if !matches!(digits.len(), 6 | 8) || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    let alpha = if digits.len() == 8 { byte(6)? } else { 255 };
    Some([byte(0)?, byte(2)?, byte(4)?, alpha].map(|b| f64::from(b) / 255.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Number;

    fn colour(name: &str, alpha: f64) -> Colour {
        Colour {
            name: name.parse().unwrap(),
            alpha: Number(alpha),
        }
    }

    #[test]
    fn the_font_is_read_from_settings_ini() {
        let ini = "[Settings]\ngtk-cursor-theme-name=x\ngtk-font-name = Adwaita Sans 11\n";
        assert_eq!(font_in(ini), Some("Adwaita Sans 11"));
        assert_eq!(font_in("[Settings]\n"), None);
    }

    #[test]
    fn hex_colours_parse() {
        assert_eq!(hex("#ff0080"), Some([1.0, 0.0, 128.0 / 255.0, 1.0]));
        assert_eq!(hex("#00000000"), Some([0.0; 4]));
        for bad in [
            "ff0080", "#ff008", "#ff00800", "#gg0080", "#ff0é0", "#+f+f+f",
        ] {
            assert_eq!(hex(bad), None, "{bad}");
        }
    }

    #[test]
    fn define_color_lines_are_read_and_the_rest_ignored() {
        let palette = Palette::parse(
            "/* comment */\n@define-color accent_color #add28e;\n\
             @define-color error_color #ff000080;\n@define-color bad #nothex;\n\
             window { color: @accent_color; }\n",
        );
        assert_eq!(palette.0.len(), 2);
        assert_eq!(
            palette.get(&colour("accent_color", 1.0)),
            hex("#add28e").unwrap()
        );
        // The file's error colour replaces the built-in one; the opacity multiplies.
        assert_eq!(
            palette.get(&colour("error_color", 0.5)),
            [1.0, 0.0, 0.0, 128.0 / 510.0]
        );
    }

    #[test]
    fn literals_built_ins_and_unknown_names_resolve() {
        let palette = Palette::parse("");
        assert_eq!(palette.get(&colour("#ffffff", 0.82)), [1.0, 1.0, 1.0, 0.82]);
        assert_eq!(
            palette.get(&colour("error_color", 1.0)),
            hex("#ffb4ab").unwrap()
        );
        assert_eq!(palette.get(&colour("nope", 1.0)), [1.0; 4]);
    }
}
