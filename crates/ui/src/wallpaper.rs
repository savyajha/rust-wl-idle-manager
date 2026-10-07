use std::fs;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, bail, ensure};
use cairo::{Context, Filter, Format, ImageSurface};
use tokio::process::Command;
use tokio::time::{self, Instant};
use tracing::info;
use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::cloexec_above_stderr;
use crate::config::Argv;

/// How much smaller than the wallpaper the blurred copy is; the blur hides the loss.
const WALLPAPER_SHRINK: i32 = 4;

/// How many box blurs in a row approximate a Gaussian one.
const PASSES: usize = 3;

const TIMEOUT: Duration = Duration::from_secs(5);

/// The most pixels a wallpaper may have; a larger one is refused before it is decoded.
const MAX_PIXELS: usize = 64_000_000;

/// The wallpaper, shrunk, blurred and toned, as cairo's `Rgb24` pixels (0x00RRGGBB in
/// native byte order), row after row.
pub struct Blurred {
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Run `command`, then decode, shrink, blur and tone the image whose path it prints.
pub async fn load(
    command: Argv,
    blur: f64,
    brightness: f64,
    saturation: f64,
) -> anyhow::Result<Blurred> {
    let program = &command.0[0];
    let running = cloexec_above_stderr(&mut Command::new(program))
        .args(&command.0[1..])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = time::timeout(TIMEOUT, running)
        .await
        .with_context(|| {
            let seconds = TIMEOUT.as_secs();
            format!("the wallpaper command {program} took over {seconds} s")
        })?
        .with_context(|| format!("running the wallpaper command {program}"))?;
    ensure!(
        output.status.success(),
        "the wallpaper command {program} failed: {}",
        output.status
    );
    let stdout = String::from_utf8(output.stdout).context("the wallpaper path is not UTF-8")?;
    let path = stdout.lines().map(str::trim).find(|line| !line.is_empty());
    let path = path
        .context("the wallpaper command printed no path")?
        .to_owned();
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let small = fs::read(&path)
            .map_err(Into::into)
            .and_then(decode)
            .and_then(|image| shrink(&image))
            .with_context(|| format!("decoding the wallpaper {path}"))?;
        let (width, height) = (small.width(), small.height());
        let data = small.take_data()?;
        let mut pixels: Vec<u32> = data
            .as_chunks()
            .0
            .iter()
            .map(|p| u32::from_ne_bytes(*p))
            .collect();
        drop(data);
        let radius = (blur / f64::from(WALLPAPER_SHRINK)).round() as usize;
        for _ in 0..PASSES {
            let across = blur_rows_transposed(&pixels, width as usize, radius);
            pixels = blur_rows_transposed(&across, height as usize, radius);
        }
        let pixels = tone(&pixels, brightness, saturation);
        // Give the decoding's tens of megabytes back to the system; glibc would keep them
        // in this thread's arena.
        // SAFETY: malloc_trim has no preconditions.
        unsafe { libc::malloc_trim(0) };
        let elapsed = started.elapsed().as_secs_f64() * 1000.0;
        info!("wallpaper {path} ready in {elapsed:.0} ms");
        Ok(Blurred {
            width,
            height,
            pixels,
        })
    })
    .await?
}

/// Decode a PNG (with cairo) or JPEG, unless its header says it has over `MAX_PIXELS`. A
/// PNG's transparency shows black.
fn decode(bytes: Vec<u8>) -> anyhow::Result<ImageSurface> {
    let check = |width: usize, height: usize| match width * height {
        0 => bail!("empty image"),
        pixels if pixels > MAX_PIXELS => bail!("{width} × {height} is too large"),
        _ => Ok(()),
    };
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // The IHDR chunk comes first: width and height, big-endian, from byte 16.
        let number = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
        let (width, height) = number(16).zip(number(20)).context("no PNG header")?;
        check(width as usize, height as usize)?;
        return Ok(ImageSurface::create_from_png(&mut bytes.as_slice())?);
    }
    // Blue, green, red and alpha bytes are cairo's Rgb24 on a little-endian machine.
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::BGRA);
    let mut decoder = JpegDecoder::new_with_options(ZCursor::new(&bytes), options);
    decoder.decode_headers()?;
    let (width, height) = decoder.dimensions().context("no JPEG dimensions")?;
    check(width, height)?;
    let pixels = decoder.decode()?;
    ensure!(
        pixels.len() == width * height * 4,
        "unexpected JPEG colours"
    );
    let (width, height) = (width as i32, height as i32);
    Ok(ImageSurface::create_for_data(
        pixels,
        Format::Rgb24,
        width,
        height,
        width * 4,
    )?)
}

/// `image` at 1/`WALLPAPER_SHRINK` of its size; cairo averages the pixels it replaces.
fn shrink(image: &ImageSurface) -> anyhow::Result<ImageSurface> {
    let (width, height) = (
        (image.width() / WALLPAPER_SHRINK).max(1),
        (image.height() / WALLPAPER_SHRINK).max(1),
    );
    let small = ImageSurface::create(Format::Rgb24, width, height)?;
    let cr = Context::new(&small)?;
    cr.scale(
        f64::from(width) / f64::from(image.width()),
        f64::from(height) / f64::from(image.height()),
    );
    cr.set_source_surface(image, 0.0, 0.0)?;
    cr.source().set_filter(Filter::Good);
    cr.paint()?;
    drop(cr);
    Ok(small)
}

fn channels(pixel: u32) -> [u32; 3] {
    [pixel >> 16 & 0xff, pixel >> 8 & 0xff, pixel & 0xff]
}

/// Box-blur each row of `width` 0x00RRGGBB pixels with `radius`, repeating the edge
/// pixels; the result is transposed, so that two calls blur both ways.
fn blur_rows_transposed(pixels: &[u32], width: usize, radius: usize) -> Vec<u32> {
    let height = pixels.len() / width;
    let count = 2 * radius as u32 + 1;
    let mut out = vec![0; pixels.len()];
    for (y, row) in pixels.chunks_exact(width).enumerate() {
        let at = |x: isize| channels(row[x.clamp(0, width as isize - 1) as usize]);
        let radius = radius as isize;
        let mut sum = [0; 3];
        for x in -radius..=radius {
            let pixel = at(x);
            sum = [0, 1, 2].map(|c| sum[c] + pixel[c]);
        }
        for x in 0..width {
            let [r, g, b] = sum.map(|s| (s + count / 2) / count);
            out[x * height + y] = r << 16 | g << 8 | b;
            let (gone, new) = (at(x as isize - radius), at(x as isize + radius + 1));
            sum = [0, 1, 2].map(|c| sum[c] + new[c] - gone[c]);
        }
    }
    out
}

/// `pixels` toned as CSS's brightness and saturation filters do, as cairo's bytes.
fn tone(pixels: &[u32], brightness: f64, saturation: f64) -> Vec<u8> {
    let tone = |pixel: u32| {
        let [r, g, b] = channels(pixel).map(|c| f64::from(c) * brightness);
        let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let toned = |c: f64| (luma + (c - luma) * saturation).round().clamp(0.0, 255.0) as u32;
        toned(r) << 16 | toned(g) << 8 | toned(b)
    };
    pixels
        .iter()
        .flat_map(|&pixel| tone(pixel).to_ne_bytes())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blurring_keeps_a_flat_image_and_spreads_a_dot() {
        let flat = vec![0x070809; 15];
        let across = blur_rows_transposed(&flat, 5, 2);
        assert_eq!(blur_rows_transposed(&across, 3, 2), flat);
        let dot: Vec<u32> = (0..7).map(|x| if x == 3 { 0xd20000 } else { 0 }).collect();
        let blurred = blur_rows_transposed(&dot, 7, 1);
        assert_eq!(blurred, [0, 0, 0x460000, 0x460000, 0x460000, 0, 0]);
        // Rows come back as columns.
        assert_eq!(blur_rows_transposed(&[1, 2, 3, 4], 2, 0), [1, 3, 2, 4]);
    }

    #[test]
    fn toning_scales_brightness_and_saturation() {
        let pixel = |bytes: &[u8], i: usize| u32::from_ne_bytes(bytes.as_chunks().0[i]);
        let grey_red = [0x646464, 0xc80000];
        let same = tone(&grey_red, 1.0, 1.0);
        assert_eq!([pixel(&same, 0), pixel(&same, 1)], grey_red);
        let toned = tone(&grey_red, 0.5, 2.0);
        // Grey stays grey; red moves away from its luma (21.26 after dimming).
        assert_eq!(pixel(&toned, 0), 0x323232);
        let red = (21.26f64 + (100.0 - 21.26) * 2.0).round() as u32;
        assert_eq!(pixel(&toned, 1) >> 16, red);
    }

    #[test]
    fn images_shrink_and_huge_ones_are_refused_unread() {
        let mut png = Vec::new();
        let image = ImageSurface::create(Format::Rgb24, 9, 4).unwrap();
        image.write_to_png(&mut png).unwrap();
        let small = shrink(&decode(png).unwrap()).unwrap();
        assert_eq!((small.width(), small.height()), (2, 1));
        // A PNG header for 10000 × 10000 pixels, and nothing after it.
        let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        huge.extend(10000u32.to_be_bytes().repeat(2));
        let err = decode(huge).unwrap_err().to_string();
        assert_eq!(err, "10000 × 10000 is too large");
    }
}
