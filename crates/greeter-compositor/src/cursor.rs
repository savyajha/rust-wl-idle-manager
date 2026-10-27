//! The pointer's image: a cursor theme's `left_ptr` when the command line names a theme, or
//! else a built-in arrow (the greeter user has no theme of its own).

use std::path::Path;

/// `X` outline, `.` fill, space transparent. The hotspot is the top-left pixel.
const ARROW: [&str; 17] = [
    "X           ",
    "XX          ",
    "X.X         ",
    "X..X        ",
    "X...X       ",
    "X....X      ",
    "X.....X     ",
    "X......X    ",
    "X.......X   ",
    "X........X  ",
    "X.....XXXXX ",
    "X..X..X     ",
    "X.X X..X    ",
    "XX  X..X    ",
    "X    X..X   ",
    "     X..X   ",
    "      XX    ",
];

/// A cursor image: premultiplied ARGB8888 pixels, row-major, and the pixel that points.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    pub hotspot: (usize, usize),
    pub pixels: Vec<u32>,
}

impl Image {
    /// The image in the top-left corner of a transparent `width` × `height` one (a cursor
    /// plane has its own size), cut off where it does not fit.
    pub fn padded(&self, width: usize, height: usize) -> Vec<u32> {
        let mut pixels = vec![0u32; width * height];
        let w = self.width.min(width);
        for y in 0..self.height.min(height) {
            let row = &self.pixels[y * self.width..][..w];
            pixels[y * width..][..w].copy_from_slice(row);
        }
        pixels
    }
}

/// The built-in arrow at an integer `scale`.
pub fn arrow(scale: usize) -> Image {
    let (width, height) = (ARROW[0].len() * scale, ARROW.len() * scale);
    let mut pixels = vec![0u32; width * height];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            let colour = match c {
                b'X' => 0xff00_0000,
                b'.' => 0xffff_ffff,
                _ => continue,
            };
            for dy in 0..scale {
                let at = (y * scale + dy) * width + x * scale;
                pixels[at..at + scale].fill(colour);
            }
        }
    }
    Image {
        width,
        height,
        hotspot: (0, 0),
        pixels,
    }
}

/// The images of a theme's pointer, `cursors/left_ptr` (or else `cursors/default`) in its
/// directory, with their nominal sizes.
pub fn read_theme(dir: &Path) -> Result<Vec<(u32, Image)>, String> {
    let bytes = ["left_ptr", "default"]
        .iter()
        .find_map(|name| std::fs::read(dir.join("cursors").join(name)).ok())
        .ok_or_else(|| {
            format!(
                "no cursors/left_ptr or cursors/default in {}",
                dir.display()
            )
        })?;
    let images = xcursor::parser::parse_xcursor(&bytes)
        .filter(|images| !images.is_empty())
        .ok_or_else(|| format!("{}: not an Xcursor file", dir.display()))?;
    Ok(images
        .into_iter()
        .map(|i| (i.size, from_xcursor(i)))
        .collect())
}

/// An Xcursor image: its pixels are premultiplied ARGB8888 in little-endian order already.
fn from_xcursor(image: xcursor::parser::Image) -> Image {
    let pixels = image
        .pixels_rgba
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| u32::from_le_bytes(*p))
        .collect();
    Image {
        width: image.width as usize,
        height: image.height as usize,
        hotspot: (image.xhot as usize, image.yhot as usize),
        pixels,
    }
}

/// The theme's image for a cursor `size` (in logical pixels) at an output's `scale`: the one
/// nearest that many physical pixels, the larger of two as near, to be drawn as it is (sharp,
/// where a desktop scales an image down to the size exactly). The built-in arrow, at the
/// scale rounded, if there is no theme.
pub fn pick(theme: &[(u32, Image)], size: u32, scale: f64) -> Image {
    let wanted = size as f64 * scale;
    let distance = |s: u32| (s as f64 - wanted).abs();
    let nearest = theme
        .iter()
        .min_by(|(a, _), (b, _)| distance(*a).total_cmp(&distance(*b)).then(b.cmp(a)));
    match nearest {
        Some((_, image)) => image.clone(),
        None => arrow((scale.round() as usize).max(1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_even() {
        assert!(ARROW.iter().all(|row| row.len() == ARROW[0].len()));
    }

    #[test]
    fn arrow_scaled_into_a_larger_image() {
        let image = arrow(2);
        assert_eq!((image.width, image.height), (24, 34));
        let pixels = image.padded(64, 64);
        assert_eq!(pixels.len(), 64 * 64);
        // The tip: outline, at 2×2.
        assert_eq!(pixels[0], 0xff00_0000);
        assert_eq!(pixels[64 + 1], 0xff00_0000);
        // Fill on the third row, second column.
        assert_eq!(pixels[4 * 64 + 2], 0xffff_ffff);
        // Nothing beyond the arrow.
        assert!(pixels[34 * 64..].iter().all(|&p| p == 0));
        assert!((0..64).all(|y| pixels[y * 64 + 24..(y + 1) * 64].iter().all(|&p| p == 0)));
    }

    #[test]
    fn padded_cuts_off_what_does_not_fit() {
        let image = Image {
            width: 3,
            height: 2,
            hotspot: (0, 0),
            pixels: vec![1, 2, 3, 4, 5, 6],
        };
        assert_eq!(image.padded(2, 1), [1, 2]);
        assert_eq!(image.padded(4, 3), [1, 2, 3, 0, 4, 5, 6, 0, 0, 0, 0, 0]);
    }

    /// An Xcursor file of one 2×1 image per size, hotspot (1, 0), as libXcursor writes it.
    fn xcursor_file(sizes: &[u32]) -> Vec<u8> {
        let le = |v: u32| v.to_le_bytes();
        let mut file = [*b"Xcur", le(16), le(0x1_0000), le(sizes.len() as u32)].concat();
        let start = 16 + 12 * sizes.len() as u32;
        for (i, size) in sizes.iter().enumerate() {
            let at = start + i as u32 * (36 + 8);
            file.extend([le(0xfffd_0002), le(*size), le(at)].concat());
        }
        for size in sizes {
            let header = [36, 0xfffd_0002, *size, 1, 2, 1, 1, 0, 0];
            file.extend(header.iter().flat_map(|v| le(*v)));
            // Opaque red, then half-transparent black, as ARGB words.
            file.extend([le(0xffff_0000), le(0x8000_0000)].concat());
        }
        file
    }

    #[test]
    fn theme_read_from_its_directory() {
        let dir = std::env::temp_dir().join(format!("cursor-theme-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("cursors")).unwrap();
        assert!(read_theme(&dir).is_err());
        std::fs::write(dir.join("cursors/default"), b"not a cursor").unwrap();
        assert!(read_theme(&dir).is_err());
        // left_ptr comes before default.
        std::fs::write(dir.join("cursors/left_ptr"), xcursor_file(&[24, 48])).unwrap();
        let theme = read_theme(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let image = Image {
            width: 2,
            height: 1,
            hotspot: (1, 0),
            pixels: vec![0xffff_0000, 0x8000_0000],
        };
        assert_eq!(theme, [(24, image.clone()), (48, image)]);
    }

    #[test]
    fn picks_the_nearest_size() {
        let image = |width| Image {
            width,
            height: 1,
            hotspot: (0, 0),
            pixels: vec![0; width],
        };
        let theme = [(24, image(24)), (36, image(36)), (48, image(48))];
        assert_eq!(pick(&theme, 24, 1.0).width, 24);
        // 38.4: 36 is nearer than 48 (the laptop's case).
        assert_eq!(pick(&theme, 24, 1.6).width, 36);
        // 30 and 42: halfway, so the larger.
        assert_eq!(pick(&theme, 24, 1.25).width, 36);
        assert_eq!(pick(&theme, 24, 1.75).width, 48);
        // Beyond the largest, or below the smallest.
        assert_eq!(pick(&theme, 24, 3.0).width, 48);
        assert_eq!(pick(&theme, 16, 1.0).width, 24);
        // No theme: the arrow at the scale rounded.
        assert_eq!(pick(&[], 24, 1.6), arrow(2));
    }
}
