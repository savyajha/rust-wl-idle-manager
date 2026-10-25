use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use tracing::error;
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;

/// The most buffers a surface's frames use: one the compositor shows, one to draw the next in.
const BUFFERS: usize = 2;

/// A surface's buffers, each remembered with what it shows (a `T`), so that one that shows the
/// frame wanted already is attached again without drawing.
pub struct Frames<T> {
    buffers: Vec<Framebuffer<T>>,
}

struct Framebuffer<T> {
    buffer: Buffer,
    /// `None` once what it shows is out of date.
    shows: Option<T>,
}

/// A buffer for a frame, by its index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Frame {
    /// It shows the frame already.
    Reused(usize),
    /// The frame is to be painted in it (`Frames::canvas`).
    Painted(usize),
}

impl<T> Default for Frames<T> {
    fn default() -> Self {
        Self {
            buffers: Vec::new(),
        }
    }
}

impl<T: Clone + PartialEq> Frames<T> {
    /// The buffer of `pixels` for a frame showing `wanted`: a released one that shows it, else
    /// any released one, else a new one up to `BUFFERS`; `None` while the compositor holds
    /// them all. A buffer to be painted is taken to show `wanted` from now on; buffers of
    /// another size are dropped.
    pub fn frame(
        &mut self,
        pool: &mut SlotPool,
        pixels: (i32, i32),
        format: Format,
        wanted: &T,
    ) -> Option<Frame> {
        let (width, height) = pixels;
        let sized =
            |b: &Framebuffer<T>| (b.buffer.stride(), b.buffer.height()) == (width * 4, height);
        self.buffers.retain(sized);
        let shows: Vec<_> = self.buffers.iter().map(|b| b.shows.as_ref()).collect();
        let released = |at: usize| self.buffers[at].buffer.canvas(pool).is_some();
        let frame = choose(&shows, released, wanted)?;
        if let Frame::Painted(at) = frame {
            if at == self.buffers.len() {
                let created = pool.create_buffer(width, height, width * 4, format);
                let (buffer, _) = created
                    .inspect_err(|e| error!("allocating a buffer: {e}"))
                    .ok()?;
                self.buffers.push(Framebuffer {
                    buffer,
                    shows: None,
                });
            }
            self.buffers[at].shows = Some(wanted.clone());
        }
        Some(frame)
    }

    /// Where to paint buffer `at`, which `frame` returned as `Painted`.
    pub fn canvas<'a>(&self, at: usize, pool: &'a mut SlotPool) -> &'a mut [u8] {
        let buffer = &self.buffers[at].buffer;
        buffer
            .canvas(pool)
            .expect("`frame` paints only a released buffer")
    }

    /// Attach buffer `at`, of `pixels`, to `surface`, all of it damaged.
    pub fn attach(&self, at: usize, surface: &WlSurface, pixels: (i32, i32)) -> anyhow::Result<()> {
        let framebuffer = self
            .buffers
            .get(at)
            .ok_or_else(|| anyhow::anyhow!("no buffer {at}"))?;
        framebuffer.buffer.attach_to(surface)?;
        surface.damage_buffer(0, 0, pixels.0, pixels.1);
        Ok(())
    }

    /// What each buffer shows is out of date.
    pub fn forget(&mut self) {
        for framebuffer in &mut self.buffers {
            framebuffer.shows = None;
        }
    }

    /// Drop every buffer, as before their pool is replaced.
    pub fn clear(&mut self) {
        self.buffers.clear();
    }
}

/// The frame for `wanted`, of buffers showing `shows`, of which `released(at)` says whether
/// the compositor has released buffer `at`.
fn choose<T: PartialEq>(
    shows: &[Option<&T>],
    mut released: impl FnMut(usize) -> bool,
    wanted: &T,
) -> Option<Frame> {
    if let Some(at) = (0..shows.len()).find(|&at| shows[at] == Some(wanted) && released(at)) {
        return Some(Frame::Reused(at));
    }
    match (0..shows.len()).find(|&at| released(at)) {
        Some(at) => Some(Frame::Painted(at)),
        None if shows.len() < BUFFERS => Some(Frame::Painted(shows.len())),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPING: u32 = 1;
    const MORE: u32 = 2;

    #[test]
    fn a_frame_reuses_a_released_buffer_that_shows_it() {
        let shows = [Some(&MORE), Some(&TYPING)];
        assert_eq!(choose(&shows, |_| true, &TYPING), Some(Frame::Reused(1)));
        // One the compositor holds is painted over in the other.
        assert_eq!(
            choose(&shows, |at| at == 0, &TYPING),
            Some(Frame::Painted(0))
        );
    }

    #[test]
    fn a_second_buffer_is_added_then_frames_wait_for_a_release() {
        assert_eq!(choose(&[], |_| true, &TYPING), Some(Frame::Painted(0)));
        let shows = [Some(&TYPING)];
        assert_eq!(choose(&shows, |_| false, &MORE), Some(Frame::Painted(1)));
        let shows = [Some(&TYPING), Some(&MORE)];
        assert_eq!(choose(&shows, |_| false, &MORE), None);
        assert_eq!(choose(&shows, |_| false, &TYPING), None);
    }
}
