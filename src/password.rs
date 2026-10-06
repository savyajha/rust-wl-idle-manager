use std::io;
use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence};

use tracing::warn;

/// The most bytes of UTF-8 a password can have; typing past it is ignored.
pub const CAPACITY: usize = 1024;

/// Overwrite `bytes` with zeros, with volatile writes the compiler cannot remove as dead
/// stores (as the zeroize crate does).
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned, exclusive reference.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Page-aligned, so that it fills exactly one page that `mlock` can pin.
#[repr(align(4096))]
struct Page([u8; CAPACITY]);

/// A password as UTF-8, in one buffer that is allocated once, never moved or grown, kept
/// out of swap, and wiped wherever the password shrinks. Not `Debug`.
pub struct Password {
    page: Box<Page>,
    len: usize,
}

impl Password {
    /// Allocate the buffer and lock it in memory; a refusal (such as `RLIMIT_MEMLOCK`) is
    /// logged, and the buffer is used anyway.
    pub fn new() -> Self {
        let page = Box::new(Page([0; CAPACITY]));
        // SAFETY: the range is the buffer's own memory, which lives as long as `page`.
        if unsafe { libc::mlock(page.0.as_ptr().cast(), CAPACITY) } != 0 {
            let e = io::Error::last_os_error();
            warn!("could not lock the password buffer in memory: {e}");
        }
        Self { page, len: 0 }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.page.0[..self.len]
    }

    pub fn chars(&self) -> usize {
        self.bytes().iter().filter(|&&b| starts_char(b)).count()
    }

    /// Add `c`, encoded straight into the buffer, unless it does not fit.
    pub fn push(&mut self, c: char) {
        let end = self.len + c.len_utf8();
        if end <= CAPACITY {
            c.encode_utf8(&mut self.page.0[self.len..end]);
            self.len = end;
        }
    }

    /// Remove the last character, wiping its bytes.
    pub fn pop(&mut self) {
        if let Some(start) = self.last_start() {
            wipe(&mut self.page.0[start..self.len]);
            self.len = start;
        }
    }

    /// The last character, decoded in place.
    pub fn last(&self) -> Option<char> {
        let start = self.last_start()?;
        str::from_utf8(&self.page.0[start..self.len])
            .ok()?
            .chars()
            .next()
    }

    pub fn wipe(&mut self) {
        wipe(&mut self.page.0);
        self.len = 0;
    }

    fn last_start(&self) -> Option<usize> {
        self.bytes().iter().rposition(|&b| starts_char(b))
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// Whether `byte` starts a UTF-8 character (is not a 0b10xxxxxx continuation byte).
fn starts_char(byte: u8) -> bool {
    byte & 0xc0 != 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wiped(password: &Password) -> bool {
        password.page.0.iter().all(|&b| b == 0)
    }

    #[test]
    fn characters_are_utf8_and_pop_wipes_one() {
        let mut password = Password::new();
        "aé€😀".chars().for_each(|c| password.push(c));
        assert_eq!(password.bytes(), "aé€😀".as_bytes());
        assert_eq!(password.chars(), 4);
        assert_eq!(password.last(), Some('😀'));
        for want in ["aé€", "aé", "a", "", ""] {
            password.pop();
            assert_eq!(password.bytes(), want.as_bytes());
        }
        assert!(wiped(&password));
    }

    #[test]
    fn a_character_past_capacity_is_ignored() {
        let mut password = Password::new();
        (1..CAPACITY).for_each(|_| password.push('a'));
        password.push('é');
        assert_eq!(password.bytes().len(), CAPACITY - 1);
        password.push('b');
        password.push('c');
        assert_eq!(password.bytes().len(), CAPACITY);
        assert_eq!(password.last(), Some('b'));
    }

    #[test]
    fn wipe_zeroes_the_whole_buffer() {
        let mut password = Password::new();
        "secret".chars().for_each(|c| password.push(c));
        password.wipe();
        assert_eq!(password.bytes(), b"");
        assert!(wiped(&password));
    }
}
