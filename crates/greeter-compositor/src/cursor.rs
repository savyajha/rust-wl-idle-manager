//! The built-in arrow cursor: no cursor theme is read (the greeter user has none).

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

/// The arrow at an integer `scale`, as premultiplied ARGB8888 pixels in a `width` × `height`
/// image (a cursor plane wants its own size, larger than the arrow), row-major.
pub fn arrow(scale: usize, width: usize, height: usize) -> Vec<u32> {
    let mut pixels = vec![0u32; width * height];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            let colour = match c {
                b'X' => 0xff00_0000,
                b'.' => 0xffff_ffff,
                _ => continue,
            };
            for dy in 0..scale {
                for dx in 0..scale {
                    let (px, py) = (x * scale + dx, y * scale + dy);
                    if px < width && py < height {
                        pixels[py * width + px] = colour;
                    }
                }
            }
        }
    }
    pixels
}

/// The arrow's size at `scale`.
pub fn size(scale: usize) -> (usize, usize) {
    (ARROW[0].len() * scale, ARROW.len() * scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_even() {
        assert!(ARROW.iter().all(|row| row.len() == ARROW[0].len()));
    }

    #[test]
    fn scaled_into_a_larger_image() {
        let pixels = arrow(2, 64, 64);
        assert_eq!(pixels.len(), 64 * 64);
        // The tip: outline, at 2×2.
        assert_eq!(pixels[0], 0xff00_0000);
        assert_eq!(pixels[64 + 1], 0xff00_0000);
        // Fill on the third row, second column.
        assert_eq!(pixels[4 * 64 + 2], 0xffff_ffff);
        // Nothing beyond the arrow.
        let (w, h) = size(2);
        assert!(pixels[h * 64..].iter().all(|&p| p == 0));
        assert!((0..64).all(|y| pixels[y * 64 + w..(y + 1) * 64].iter().all(|&p| p == 0)));
    }
}
