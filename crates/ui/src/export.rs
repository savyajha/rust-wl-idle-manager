//! The lock screen's theme, copied for the greeter, which cannot read the user's files: a
//! directory like GTK's (`gtk.css` with `@define-color` lines, `settings.ini` with
//! `gtk-font-name`), which `theme` reads, and `background`, the processed wallpaper.
//!
//! `background` is a header (the magic `RWGB`, then the version, the width and the height,
//! each a little-endian u32) and the pixels, cairo's `Rgb24` (in native byte order), row
//! after row. The user writes these files and the greeter reads them before anyone has
//! logged in, so `read` takes a file only if it is the user's own and small.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, ensure};

use crate::wallpaper::Blurred;

pub const BACKGROUND: &str = "background";
const MAGIC: &[u8; 4] = b"RWGB";
const VERSION: u32 = 1;
const HEADER: usize = 16;

/// The most pixels a `background` read has, of any shape (a panorama is wide).
pub const MAX_PIXELS: u64 = 2048 * 2048;
pub const MAX_BACKGROUND: u64 = HEADER as u64 + MAX_PIXELS * 4;
/// For `gtk.css` and `settings.ini`.
pub const MAX_TEXT: u64 = 64 * 1024;

/// `background`'s bytes for a wallpaper of `width` × `height` `pixels`.
pub fn encode(width: i32, height: i32, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + pixels.len());
    bytes.extend_from_slice(MAGIC);
    for value in [VERSION, width as u32, height as u32] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(pixels);
    bytes
}

/// The wallpaper in `background`'s `bytes`, if they are one, of at most `MAX_PIXELS`.
pub fn decode(bytes: &[u8]) -> anyhow::Result<Blurred> {
    ensure!(bytes.len() >= HEADER, "shorter than its header");
    let word = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().expect("4 bytes"));
    ensure!(&bytes[..4] == MAGIC, "not a background");
    let (version, width, height) = (word(4), word(8), word(12));
    ensure!(version == VERSION, "version {version}, not {VERSION}");
    // In u64, which no two u32s overflow.
    let pixels = u64::from(width) * u64::from(height);
    ensure!(
        (1..=MAX_PIXELS).contains(&pixels),
        "{width}×{height}, empty or over {MAX_PIXELS} pixels"
    );
    let want = HEADER as u64 + pixels * 4;
    ensure!(
        bytes.len() as u64 == want,
        "{} bytes, not {want}",
        bytes.len()
    );
    Ok(Blurred {
        width: width as i32,
        height: height as i32,
        pixels: bytes[HEADER..].to_vec(),
    })
}

/// Write `contents` to `name` in `dir`, mode 0640, replacing it at once: to a new file
/// beside it (never through a link left there), then renamed over it.
pub fn write(dir: &Path, name: &str, contents: &[u8]) -> anyhow::Result<()> {
    let (path, temporary) = (dir.join(name), dir.join(format!(".{name}.new")));
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", temporary.display()));
        }
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        // Refuses an existing file, a symbolic link included.
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    // Not left to the umask.
    file.set_permissions(Permissions::from_mode(0o640))
        .and_then(|()| file.write_all(contents))
        .with_context(|| format!("writing {}", temporary.display()))?;
    fs::rename(&temporary, &path).with_context(|| format!("replacing {}", path.display()))
}

/// The contents of the file at `path`, if it is a plain file (not a link) of user `uid`'s,
/// of at most `max` bytes.
pub fn read(path: &Path, uid: u32, max: u64) -> anyhow::Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    let (owner, len) = (metadata.uid(), metadata.len());
    ensure!(
        metadata.is_file() && owner == uid && len <= max,
        "not a file of UID {uid}'s of at most {max} bytes (UID {owner}'s, {len} bytes)"
    );
    let mut bytes = Vec::new();
    // Not more than was checked, if it grows meanwhile.
    (&mut file).take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "grew over {max} bytes");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("export-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_background_round_trips_and_anything_else_is_refused() {
        let pixels: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8).collect();
        let bytes = encode(3, 2, &pixels);
        assert_eq!(&bytes[..16], b"RWGB\x01\0\0\0\x03\0\0\0\x02\0\0\0");
        let back = decode(&bytes).unwrap();
        assert_eq!(
            (back.width, back.height, back.pixels),
            (3, 2, pixels.clone())
        );
        let refused = |bytes: Vec<u8>| decode(&bytes).is_err();
        assert!(refused(bytes[..15].to_vec()), "shorter than the header");
        assert!(refused(bytes[..bytes.len() - 1].to_vec()), "short");
        assert!(refused([&bytes[..], &[0]].concat()), "long");
        assert!(refused([b"RWGX", &bytes[4..]].concat()), "bad magic");
        let mut version = bytes.clone();
        version[4] = 2;
        assert!(refused(version), "another version");
        // Any shape, a panorama's too, up to 2048 × 2048 pixels.
        assert!(decode(&encode(9000, 1, &[0; 9000 * 4])).is_ok(), "wide");
        assert!(
            refused(encode(2049, 2048, &vec![0; 2049 * 2048 * 4])),
            "too many pixels"
        );
        assert!(refused(encode(0, 5, &[])), "empty");
        // Sides whose product would overflow 32 bits, and a size without its pixels.
        assert!(refused(encode(-1, -1, &[])), "4294967295 × 4294967295");
        assert!(refused(encode(2048, 2048, &[])), "too short for its size");
    }

    #[test]
    fn a_write_replaces_at_once_and_a_read_checks_the_file() {
        let dir = dir("write");
        let uid = our_uid(&dir);
        write(&dir, "gtk.css", b"one").unwrap();
        // A stale temporary file, even a link, is replaced, not written through.
        let elsewhere = dir.join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.join(".gtk.css.new")).unwrap();
        write(&dir, "gtk.css", b"two").unwrap();
        assert!(!elsewhere.exists() && !dir.join(".gtk.css.new").exists());
        let path = dir.join("gtk.css");
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o640);
        assert_eq!(read(&path, uid, 3).unwrap(), b"two");
        // Too big, someone else's, a link, not a file, missing.
        assert!(read(&path, uid, 2).is_err());
        assert!(read(&path, uid + 1, 3).is_err());
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read(&link, uid, 3).is_err());
        assert!(read(&dir, uid, 3).is_err());
        assert!(read(&dir.join("none"), uid, 3).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The UID files made in `dir` get: ours.
    fn our_uid(dir: &Path) -> u32 {
        fs::metadata(dir).unwrap().uid()
    }
}
