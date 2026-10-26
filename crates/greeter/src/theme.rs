//! The last user's theme, as their lock screen copied it for the greeter (`ui::export`):
//! its colours, font and processed background, read once, before the first frame. The user
//! wrote these files, so each is read only if it is theirs (the owner of their directory,
//! which root makes), a plain file and small; one that is not, or is malformed, is skipped
//! with a warning, and the rest still apply. Without any, the look's own.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::Context;
use tokio::time::Instant;
use tracing::{info, warn};
use ui::export::{self, MAX_BACKGROUND, MAX_TEXT};
use ui::theme::{self, Palette};
use ui::wallpaper::Blurred;

/// Each user's theme is in a directory of theirs here, which the NixOS module creates.
const THEMES: &str = "/var/cache/rust-wl-greeter";

pub struct Theme {
    pub palette: Palette,
    pub font: String,
    pub wallpaper: Option<Blurred>,
}

/// `user`'s theme, from `THEMES`; or with none, the built-in colours and "Sans".
pub fn load(user: Option<&str>) -> Theme {
    let started = Instant::now();
    // A name that is not one, or that would leave `THEMES`.
    let user = user.filter(|user| !matches!(*user, "" | "." | "..") && !user.contains('/'));
    let files = match user.map(|user| (user, read(&Path::new(THEMES).join(user)))) {
        Some((user, Ok(files))) => {
            let ms = started.elapsed().as_secs_f64() * 1000.0;
            let background =
                (files.wallpaper.as_ref()).map(|w| format!("background {}×{}", w.width, w.height));
            let parts = [
                files.css.as_ref().map(|_| "gtk.css".to_owned()),
                files.settings.as_ref().map(|_| "settings.ini".to_owned()),
                background,
            ];
            let parts: Vec<_> = parts.into_iter().flatten().collect();
            let parts = if parts.is_empty() {
                "nothing".to_owned()
            } else {
                parts.join(", ")
            };
            info!("theme of {user} read in {ms:.1} ms: {parts}");
            files
        }
        Some((user, Err(e))) => {
            info!("no theme of {user}: {e:#}; the look's own");
            Files::default()
        }
        None => {
            info!("no one logged in before: the look's own theme");
            Files::default()
        }
    };
    let font = theme::font_in(&files.settings.unwrap_or_default()).map(str::to_owned);
    Theme {
        palette: Palette::parse(&files.css.unwrap_or_default()),
        font: font.unwrap_or_else(|| "Sans".to_owned()),
        wallpaper: files.wallpaper,
    }
}

#[derive(Default)]
struct Files {
    css: Option<String>,
    settings: Option<String>,
    wallpaper: Option<Blurred>,
}

/// The files in the theme directory `dir`, each that passes `export::read`'s checks.
fn read(dir: &Path) -> anyhow::Result<Files> {
    let metadata =
        fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    anyhow::ensure!(metadata.is_dir(), "{} is not a directory", dir.display());
    let uid = metadata.uid();
    let file = |name: &str, max: u64| -> Option<Vec<u8>> {
        let path = dir.join(name);
        // Not there (as no link either): nothing to say.
        fs::symlink_metadata(&path).ok()?;
        let read = export::read(&path, uid, max);
        read.inspect_err(|e| warn!("ignoring {}: {e:#}", path.display()))
            .ok()
    };
    let text = |name: &str| {
        let text = String::from_utf8(file(name, MAX_TEXT)?);
        text.inspect_err(|_| warn!("ignoring {name}: not UTF-8"))
            .ok()
    };
    let wallpaper = file(export::BACKGROUND, MAX_BACKGROUND).and_then(|bytes| {
        export::decode(&bytes)
            .inspect_err(|e| warn!("ignoring the theme's background: {e:#}"))
            .ok()
    });
    Ok(Files {
        css: text("gtk.css"),
        settings: text("settings.ini"),
        wallpaper,
    })
}
