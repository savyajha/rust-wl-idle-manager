use std::io;
use std::ops::{Deref, DerefMut};

use tracing::warn;
use zeroize::Zeroize;

/// The most bytes of UTF-8 a password can have; typing past it is ignored.
pub const CAPACITY: usize = 1024;

/// Page-aligned, so that `N` bytes (a multiple of the page size) fill whole pages that
/// `mlock` can pin.
#[repr(align(4096))]
struct Pages<const N: usize>([u8; N]);

/// `N` bytes in pages of their own: allocated once, never moved or grown, kept out of swap,
/// and wiped when dropped. For secrets; not `Debug`.
pub struct Locked<const N: usize>(Box<Pages<N>>);

impl<const N: usize> Locked<N> {
    // Not `Default`: it mlocks pages.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let pages = Box::new(Pages([0; N]));
        // SAFETY: the range is the buffer's own memory, which lives as long as `pages`.
        if unsafe { libc::mlock(pages.0.as_ptr().cast(), N) } != 0 {
            let e = io::Error::last_os_error();
            warn!("could not lock a secret's buffer in memory: {e}");
        }
        Self(pages)
    }
}

impl<const N: usize> Deref for Locked<N> {
    type Target = [u8; N];

    fn deref(&self) -> &[u8; N] {
        &self.0.0
    }
}

impl<const N: usize> DerefMut for Locked<N> {
    fn deref_mut(&mut self) -> &mut [u8; N] {
        &mut self.0.0
    }
}

impl<const N: usize> Drop for Locked<N> {
    fn drop(&mut self) {
        self.0.0.zeroize();
    }
}

/// A password as UTF-8, in one `Locked` page, wiped wherever the password shrinks.
/// Not `Debug`.
pub struct Password {
    page: Locked<CAPACITY>,
    len: usize,
}

impl Password {
    // Not `Default`: it mlocks a page.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            page: Locked::new(),
            len: 0,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.page[..self.len]
    }

    pub fn chars(&self) -> usize {
        self.bytes().iter().filter(|&&b| starts_char(b)).count()
    }

    pub fn push(&mut self, c: char) {
        let end = self.len + c.len_utf8();
        if end <= CAPACITY {
            c.encode_utf8(&mut self.page[self.len..end]);
            self.len = end;
        }
    }

    pub fn pop(&mut self) {
        if let Some(start) = self.last_start() {
            self.page[start..self.len].zeroize();
            self.len = start;
        }
    }

    pub fn last(&self) -> Option<char> {
        let start = self.last_start()?;
        str::from_utf8(&self.page[start..self.len])
            .ok()?
            .chars()
            .next()
    }

    pub fn wipe(&mut self) {
        self.page.zeroize();
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
        password.page.iter().all(|&b| b == 0)
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
