use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

use tracing::{info, warn};

use crate::config::Colour;

/// Red, green, blue and alpha, each from 0 to 1, not premultiplied.
pub type Rgba = [f64; 4];

/// Named colours used by the defaults, for when gtk.css lacks them.
const BUILT_IN: [(&str, &str); 1] = [("error_color", "#ffb4ab")];

/// GTK's named colours, as matugen writes them to `gtk-4.0/gtk.css`.
pub struct Palette(HashMap<String, Rgba>);

impl Palette {
    /// The built-in colours, overridden by GTK's gtk.css; a missing file is logged, and
    /// leaves the built-in ones.
    pub fn load() -> Self {
        let path = file("gtk.css");
        let css = fs::read_to_string(&path).unwrap_or_else(|e| {
            info!("no GTK colours from {}: {e}", path.display());
            String::new()
        });
        Self::parse(&css)
    }

    /// The built-in colours, and those `css` defines with `@define-color <name> #hex;`
    /// lines; everything else is ignored.
    fn parse(css: &str) -> Self {
        let defined = css.lines().filter_map(|line| {
            let mut words = line.strip_prefix("@define-color")?.split_whitespace();
            let name = words.next()?;
            Some((name, hex(words.next()?.strip_suffix(';')?)?))
        });
        let built_in = BUILT_IN
            .iter()
            .map(|&(name, value)| (name, hex(value).unwrap()));
        Self(
            built_in
                .chain(defined)
                .map(|(name, rgba)| (name.to_owned(), rgba))
                .collect(),
        )
    }

    /// The colour `colour` names, with its opacity applied; an unknown name is logged and
    /// gives white.
    pub fn get(&self, colour: &Colour) -> Rgba {
        let name = colour.name.0.as_str();
        let named = || self.0.get(name).copied();
        let [r, g, b, a] = hex(name).or_else(named).unwrap_or_else(|| {
            warn!("unknown colour {name:?}; using white");
            [1.0; 4]
        });
        [r, g, b, a * colour.alpha.0]
    }
}

/// `name` in GTK 4's config directory, `$XDG_CONFIG_HOME/gtk-4.0` (by default
/// `~/.config/gtk-4.0`).
fn file(name: &str) -> PathBuf {
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
    config.unwrap_or_default().join("gtk-4.0").join(name)
}

/// GTK's default font, `gtk-font-name` in its settings.ini, or "Sans".
pub fn font() -> String {
    let settings = fs::read_to_string(file("settings.ini")).unwrap_or_default();
    font_in(&settings).unwrap_or("Sans").to_owned()
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
fn hex(text: &str) -> Option<Rgba> {
    let digits = text.strip_prefix('#')?;
    if !matches!(digits.len(), 6 | 8) || !digits.is_ascii() {
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
            name: crate::config::ColourName(name.into()),
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
        for bad in ["ff0080", "#ff008", "#ff00800", "#gg0080", "#ff0é0"] {
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
