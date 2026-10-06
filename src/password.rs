use std::fmt;
use std::io;
use std::ptr;
use std::sync::atomic::{Ordering, compiler_fence};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use xkbcommon::xkb::Keysym;

/// The most bytes of UTF-8 a password can have; typing past it is ignored.
pub const CAPACITY: usize = 1024;

/// How long a password is kept without typing before it is wiped.
const FORGET_AFTER: Duration = Duration::from_secs(30);

/// How long a failed attempt is shown.
const FAILED_FOR: Duration = Duration::from_millis(1500);

/// After every this many failed attempts in a row, no attempt is taken for `COOLDOWN`.
const COOLDOWN_AFTER: u32 = 5;
const COOLDOWN: Duration = Duration::from_secs(30);

/// Overwrite `bytes` with zeros, with volatile writes the compiler cannot remove as dead
/// stores (as the zeroize crate does).
pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes.iter_mut() {
        // SAFETY: `byte` is a valid, aligned, exclusive reference.
        unsafe { ptr::write_volatile(byte, 0) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Whether `byte` starts a UTF-8 character (is not a 0b10xxxxxx continuation byte).
fn starts_char(byte: u8) -> bool {
    byte & 0xc0 != 0x80
}

/// A key the lock screen acts on. Not `Debug`, since it may hold a character of the
/// password.
#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Enter,
    Backspace,
    Escape,
    Char(char),
}

impl Key {
    /// The key a press means, from its `keysym` and the character it types (`utf32`, from
    /// xkbcommon, 0 for none), or `None` for a key the lock screen ignores. Characters
    /// typed with Alt or Logo held are shortcuts, not text; with Ctrl, xkbcommon gives
    /// control characters, which are ignored too.
    pub fn from_xkb(keysym: Keysym, utf32: u32, alt_or_logo: bool) -> Option<Self> {
        match keysym {
            Keysym::Return | Keysym::KP_Enter => Some(Key::Enter),
            Keysym::BackSpace => Some(Key::Backspace),
            Keysym::Escape => Some(Key::Escape),
            _ if alt_or_logo => None,
            _ => char::from_u32(utf32)
                .filter(|c| !c.is_control())
                .map(Key::Char),
        }
    }
}

/// What the password field shows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    /// Nothing typed.
    Idle,
    Typing,
    /// The helper is checking the password.
    Checking,
    /// The last attempt failed (shown for `FAILED_FOR`, or until a key is pressed).
    Failed,
    /// Too many attempts failed: keys do nothing until the cooldown ends, in this many
    /// seconds (rounded up).
    Cooldown(u64),
}

/// Everything the lock screen shows of the entry; it is redrawn only when this changes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub status: Status,
    /// How many characters are typed, or were submitted while checking.
    pub chars: usize,
    pub caps_lock: bool,
}

/// A key held down, repeating.
struct Held {
    /// The key's code, to match its release.
    code: u32,
    /// Backspace, or else the last character again.
    backspace: bool,
    /// When it repeats next.
    next: Instant,
}

/// The password's buffer: page-aligned, so it fills exactly one page that `mlock` can pin.
#[repr(align(4096))]
struct Page([u8; CAPACITY]);

/// The password being typed, and what the keys do to it. No I/O apart from one log line
/// when a cooldown starts; the time is passed in.
/// The password is UTF-8 in one buffer allocated at startup and never moved or grown.
/// Its `Debug` prints nothing.
pub struct Entry {
    page: Box<Page>,
    len: usize,
    /// The password was submitted, and the helper has not answered yet.
    checking: bool,
    /// How many characters were submitted (only their number: the password is wiped).
    submitted: usize,
    /// Until when a failed attempt is shown.
    failed_until: Option<Instant>,
    /// Failed attempts in a row, and until when keys are ignored after too many.
    failures: u32,
    cooldown_until: Option<Instant>,
    /// When the password is wiped if nothing more is typed.
    forget_at: Option<Instant>,
    held: Option<Held>,
    /// The key repeat delay and interval; `None` if keys do not repeat.
    repeat: Option<(Duration, Duration)>,
    pub caps_lock: bool,
    /// The time last passed in, for the cooldown's countdown.
    now: Instant,
}

impl Entry {
    /// Allocate the buffer and lock it in memory, so it is never swapped out; a refusal
    /// (such as `RLIMIT_MEMLOCK`) is logged, and the buffer is used anyway.
    pub fn new() -> Self {
        let page = Box::new(Page([0; CAPACITY]));
        // SAFETY: the range is the buffer's own memory, which lives as long as `page`.
        if unsafe { libc::mlock(page.0.as_ptr().cast(), CAPACITY) } != 0 {
            let e = io::Error::last_os_error();
            warn!("could not lock the password buffer in memory: {e}");
        }
        Self {
            page,
            len: 0,
            checking: false,
            submitted: 0,
            failed_until: None,
            failures: 0,
            cooldown_until: None,
            forget_at: None,
            held: None,
            repeat: None,
            caps_lock: false,
            now: Instant::now(),
        }
    }

    /// Act on `key`, pressed at `now`; `code` identifies the physical key. Returns true
    /// when the password is to be checked. Keys do nothing while it is being checked or
    /// during a cooldown.
    pub fn press(&mut self, key: Key, code: u32, now: Instant) -> bool {
        self.now = now;
        if self.checking || self.cooldown_until.is_some() {
            return false;
        }
        self.failed_until = None;
        self.held = None;
        match key {
            // An empty password is never checked.
            Key::Enter => {
                self.checking = self.len > 0;
                self.submitted = self.chars();
            }
            Key::Escape => self.wipe(),
            Key::Backspace | Key::Char(_) => {
                self.edit(key, now);
                self.held = self.repeat.map(|(delay, _)| Held {
                    code,
                    backspace: key == Key::Backspace,
                    next: now + delay,
                });
            }
        }
        self.checking
    }

    /// The key with `code` was released.
    pub fn release(&mut self, code: u32) {
        if self.held.as_ref().is_some_and(|held| held.code == code) {
            self.held = None;
        }
    }

    /// Stop repeating: the keyboard left the lock screen, and the release may go elsewhere.
    pub fn stop_repeat(&mut self) {
        self.held = None;
    }

    /// Set the key repeat delay and rate (per second, not 0); `None` disables repeat. A
    /// key already held stops repeating.
    pub fn set_repeat(&mut self, repeat: Option<(Duration, u32)>) {
        self.repeat = repeat.map(|(delay, rate)| (delay, Duration::from_secs(1) / rate));
        self.held = None;
    }

    /// Delete or type one character, and restart the countdown to forgetting it all. A
    /// character is encoded straight into the buffer, unless it does not fit.
    fn edit(&mut self, key: Key, now: Instant) {
        match key {
            Key::Backspace => {
                if let Some(start) = self.last_start() {
                    wipe(&mut self.page.0[start..self.len]);
                    self.len = start;
                }
            }
            Key::Char(c) if self.len + c.len_utf8() <= CAPACITY => {
                let end = self.len + c.len_utf8();
                c.encode_utf8(&mut self.page.0[self.len..end]);
                self.len = end;
            }
            _ => {}
        }
        self.forget_at = (self.len > 0).then(|| now + FORGET_AFTER);
    }

    /// The password as UTF-8.
    fn bytes(&self) -> &[u8] {
        &self.page.0[..self.len]
    }

    /// Where the last character starts, if there is one.
    fn last_start(&self) -> Option<usize> {
        self.bytes().iter().rposition(|&b| starts_char(b))
    }

    /// The password submitted for checking (empty if none is); it is wiped when the
    /// returned guard is dropped, on every path.
    pub fn submission(&mut self) -> Submission<'_> {
        Submission(self)
    }

    /// The helper answered: show a failure, unless `ok`, or start a cooldown after every
    /// `COOLDOWN_AFTER` failures in a row. Returns false, ignoring the answer, if no check
    /// was under way (the lock ended since).
    pub fn checked(&mut self, ok: bool, now: Instant) -> bool {
        self.now = now;
        if !self.checking {
            return false;
        }
        let failures = if ok { 0 } else { self.failures + 1 };
        self.reset();
        self.failures = failures;
        match (ok, failures % COOLDOWN_AFTER) {
            (true, _) => {}
            (false, 0) => {
                info!(
                    "{COOLDOWN_AFTER} wrong passwords; waiting {} s",
                    COOLDOWN.as_secs()
                );
                self.cooldown_until = Some(now + COOLDOWN);
            }
            (false, _) => self.failed_until = Some(now + FAILED_FOR),
        }
        true
    }

    /// Wipe the whole buffer, and stop any key repeating.
    pub fn wipe(&mut self) {
        wipe(&mut self.page.0);
        self.len = 0;
        self.forget_at = None;
        self.held = None;
    }

    /// Wipe the password and forget any check, failure or cooldown, as for a new lock.
    pub fn reset(&mut self) {
        self.wipe();
        self.checking = false;
        self.submitted = 0;
        self.failed_until = None;
        self.failures = 0;
        self.cooldown_until = None;
    }

    /// The whole seconds the cooldown has left, rounded up, if there is one.
    fn cooldown_seconds(&self) -> Option<u64> {
        let left = self.cooldown_until?.saturating_duration_since(self.now);
        Some(left.as_secs() + u64::from(left.subsec_nanos() > 0))
    }

    /// When `tick` next has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        let held = self.held.as_ref().map(|held| held.next);
        // The countdown's next whole second, the last of which ends the cooldown.
        let second = self.cooldown_until.zip(self.cooldown_seconds());
        let second =
            second.map(|(until, left)| until - Duration::from_secs(left.saturating_sub(1)));
        [held, self.failed_until, second, self.forget_at]
            .into_iter()
            .flatten()
            .min()
    }

    /// Do what is due at `now`: repeat a held key, stop showing a failure, end a
    /// cooldown, forget a password left untouched.
    pub fn tick(&mut self, now: Instant) {
        self.now = now;
        if let (Some(held), Some((_, interval))) = (&mut self.held, self.repeat)
            && held.next <= now
        {
            held.next = now + interval;
            let key = match held.backspace {
                true => Some(Key::Backspace),
                // The last character, decoded in place.
                false => self
                    .last_start()
                    .and_then(|start| str::from_utf8(&self.page.0[start..self.len]).ok())
                    .and_then(|last| last.chars().next())
                    .map(Key::Char),
            };
            if let Some(key) = key {
                self.edit(key, now);
            }
        }
        if self.failed_until.is_some_and(|until| until <= now) {
            self.failed_until = None;
        }
        if self.cooldown_until.is_some_and(|until| until <= now) {
            self.cooldown_until = None;
        }
        if self.forget_at.is_some_and(|at| at <= now) {
            self.wipe();
        }
    }

    /// What the lock screen should show.
    pub fn look(&self) -> Look {
        let status = if self.checking {
            Status::Checking
        } else if let Some(seconds) = self.cooldown_seconds() {
            Status::Cooldown(seconds)
        } else if self.failed_until.is_some() {
            Status::Failed
        } else if self.len > 0 {
            Status::Typing
        } else {
            Status::Idle
        };
        Look {
            status,
            chars: if self.checking {
                self.submitted
            } else {
                self.chars()
            },
            caps_lock: self.caps_lock,
        }
    }

    /// How many characters are typed.
    fn chars(&self) -> usize {
        self.bytes().iter().filter(|&&b| starts_char(b)).count()
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

/// The password submitted for checking, wiped when this is dropped.
pub struct Submission<'a>(&'a mut Entry);

impl Submission<'_> {
    /// The password, or nothing if none was submitted (such as after a reset).
    pub fn bytes(&self) -> &[u8] {
        if self.0.checking { self.0.bytes() } else { &[] }
    }
}

impl Drop for Submission<'_> {
    fn drop(&mut self) {
        self.0.wipe();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAY: Duration = Duration::from_millis(600);
    const INTERVAL: Duration = Duration::from_millis(40);

    fn entry() -> (Entry, Instant) {
        let mut entry = Entry::new();
        entry.set_repeat(Some((DELAY, 25)));
        (entry, Instant::now())
    }

    fn type_text(entry: &mut Entry, text: &str, now: Instant) {
        for c in text.chars() {
            entry.press(Key::Char(c), 1, now);
            entry.release(1);
        }
    }

    #[test]
    fn characters_are_stored_as_utf8_and_backspace_removes_one() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "aé€😀", now);
        assert_eq!(entry.bytes(), "aé€😀".as_bytes());
        assert_eq!(entry.look().chars, 4);
        for want in ["aé€", "aé", "a", "", ""] {
            entry.press(Key::Backspace, 2, now);
            assert_eq!(entry.bytes(), want.as_bytes());
        }
        // The removed bytes are wiped, not just cut off.
        assert!(entry.page.0.iter().all(|&b| b == 0));
    }

    #[test]
    fn input_past_capacity_is_ignored() {
        let (mut entry, now) = entry();
        type_text(&mut entry, &"a".repeat(CAPACITY - 1), now);
        // A two-byte character no longer fits; a one-byte one still does.
        entry.press(Key::Char('é'), 1, now);
        assert_eq!(entry.bytes().len(), CAPACITY - 1);
        type_text(&mut entry, "bc", now);
        assert_eq!(entry.bytes().len(), CAPACITY);
        assert_eq!(entry.bytes().last(), Some(&b'b'));
    }

    #[test]
    fn the_buffer_is_page_aligned_and_never_moves() {
        let (mut entry, now) = entry();
        let page = entry.page.0.as_ptr();
        assert_eq!(page as usize % 4096, 0);
        type_text(&mut entry, &"x".repeat(2 * CAPACITY), now);
        entry.reset();
        assert_eq!(entry.page.0.as_ptr(), page);
    }

    #[test]
    fn escape_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "secret", now);
        assert!(!entry.press(Key::Escape, 3, now));
        assert_eq!(entry.bytes(), b"");
        assert!(entry.page.0.iter().all(|&b| b == 0));
        assert_eq!(entry.look().status, Status::Idle);
    }

    #[test]
    fn enter_submits_only_a_password() {
        let (mut entry, now) = entry();
        assert!(!entry.press(Key::Enter, 4, now));
        assert_eq!(entry.look().status, Status::Idle);
        type_text(&mut entry, "pw", now);
        assert_eq!(entry.look().status, Status::Typing);
        assert!(entry.press(Key::Enter, 4, now));
        assert_eq!(entry.look().status, Status::Checking);
    }

    #[test]
    fn keys_are_ignored_while_checking() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        for key in [Key::Char('x'), Key::Backspace, Key::Escape, Key::Enter] {
            assert!(!entry.press(key, 5, now));
        }
        assert_eq!(entry.bytes(), b"pw");
    }

    #[test]
    fn a_failure_wipes_and_shows_until_it_expires_or_a_key_is_pressed() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "wrong", now);
        entry.press(Key::Enter, 4, now);
        assert!(entry.checked(false, now));
        assert_eq!(entry.bytes(), b"");
        assert_eq!(entry.look().status, Status::Failed);
        assert_eq!(entry.deadline(), Some(now + FAILED_FOR));
        entry.tick(now + FAILED_FOR);
        assert_eq!(entry.look().status, Status::Idle);

        type_text(&mut entry, "wrong", now);
        entry.press(Key::Enter, 4, now);
        entry.checked(false, now);
        type_text(&mut entry, "r", now);
        assert_eq!(entry.look().status, Status::Typing);
    }

    #[test]
    fn success_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "right", now);
        entry.press(Key::Enter, 4, now);
        assert!(entry.checked(true, now));
        assert_eq!(entry.bytes(), b"");
        assert_eq!(entry.look().status, Status::Idle);
    }

    #[test]
    fn an_answer_after_a_reset_is_ignored() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        entry.reset();
        assert!(!entry.checked(true, now));
        assert!(!entry.checked(false, now));
        assert_eq!(entry.look().status, Status::Idle);
    }

    #[test]
    fn an_untouched_password_is_forgotten() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "abc", now);
        let later = now + Duration::from_secs(20);
        type_text(&mut entry, "d", later);
        entry.tick(now + FORGET_AFTER);
        assert_eq!(entry.bytes(), b"abcd");
        assert_eq!(entry.deadline(), Some(later + FORGET_AFTER));
        entry.tick(later + FORGET_AFTER);
        assert_eq!(entry.bytes(), b"");
        assert_eq!(entry.deadline(), None);
    }

    #[test]
    fn held_keys_repeat_after_the_delay_until_released() {
        let (mut entry, now) = entry();
        entry.press(Key::Char('a'), 7, now);
        assert_eq!(entry.deadline(), Some(now + DELAY));
        entry.tick(now + DELAY);
        entry.tick(now + DELAY + INTERVAL);
        assert_eq!(entry.bytes(), b"aaa");
        // Another key's release does not stop it.
        entry.release(8);
        entry.tick(now + DELAY + 2 * INTERVAL);
        assert_eq!(entry.bytes(), b"aaaa");
        entry.release(7);
        entry.tick(now + DELAY + 3 * INTERVAL);
        assert_eq!(entry.bytes(), b"aaaa");

        entry.press(Key::Backspace, 9, now);
        entry.tick(now + DELAY);
        assert_eq!(entry.bytes(), b"aa");
        entry.stop_repeat();
        entry.tick(now + DELAY + INTERVAL);
        assert_eq!(entry.bytes(), b"aa");
    }

    #[test]
    fn enter_and_escape_do_not_repeat_and_stop_a_repeat() {
        let (mut entry, now) = entry();
        entry.press(Key::Char('a'), 7, now);
        entry.press(Key::Escape, 3, now);
        assert_eq!(entry.deadline(), None);
        entry.set_repeat(None);
        entry.press(Key::Char('a'), 7, now);
        assert_eq!(entry.deadline(), Some(now + FORGET_AFTER));
    }

    #[test]
    fn wipe_and_reset_clear_everything() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "abc", now);
        entry.press(Key::Char('d'), 7, now);
        entry.wipe();
        assert_eq!(entry.bytes(), b"");
        assert_eq!(entry.deadline(), None);
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        entry.reset();
        assert_eq!(entry.look().status, Status::Idle);
        assert!(!entry.press(Key::Char('x'), 1, now));
        assert_eq!(entry.bytes(), b"x");
    }

    /// Submit a password and have the helper answer `ok`.
    fn attempt(entry: &mut Entry, ok: bool, now: Instant) {
        type_text(entry, "pw", now);
        assert!(entry.press(Key::Enter, 4, now));
        assert!(entry.checked(ok, now));
    }

    #[test]
    fn every_fifth_failure_in_a_row_starts_a_cooldown() {
        let (mut entry, now) = entry();
        for _ in 0..4 {
            attempt(&mut entry, false, now);
            assert_eq!(entry.look().status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert_eq!(entry.look().status, Status::Cooldown(30));
        assert_eq!(entry.deadline(), Some(now + Duration::from_secs(1)));

        // Keys type nothing and submit nothing until it ends.
        for key in [Key::Char('x'), Key::Backspace, Key::Escape, Key::Enter] {
            assert!(!entry.press(key, 5, now));
        }
        assert_eq!(entry.bytes(), b"");
        entry.tick(now + COOLDOWN - Duration::from_millis(1));
        assert_eq!(entry.look().status, Status::Cooldown(1));
        assert_eq!(entry.deadline(), Some(now + COOLDOWN));
        let now = now + COOLDOWN;
        entry.tick(now);
        assert_eq!(entry.look().status, Status::Idle);
        assert_eq!(entry.deadline(), None);

        // The count goes on: the tenth failure in a row starts another.
        for _ in 0..4 {
            attempt(&mut entry, false, now);
            assert_eq!(entry.look().status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert!(matches!(entry.look().status, Status::Cooldown(_)));
    }

    #[test]
    fn the_cooldown_counts_down_each_second() {
        let (mut entry, now) = entry();
        for _ in 0..5 {
            attempt(&mut entry, false, now);
        }
        let mut seen = Vec::new();
        while let Some(at) = entry.deadline() {
            entry.tick(at);
            seen.push((at - now, entry.look().status));
        }
        assert_eq!(seen.len(), 30);
        assert_eq!(seen[0], (Duration::from_secs(1), Status::Cooldown(29)));
        assert_eq!(seen[28], (Duration::from_secs(29), Status::Cooldown(1)));
        assert_eq!(seen[29], (COOLDOWN, Status::Idle));
        // From part-way through a second, the next wake is at the whole second.
        for _ in 0..5 {
            attempt(&mut entry, false, now);
        }
        entry.tick(now + Duration::from_millis(1500));
        assert_eq!(entry.look().status, Status::Cooldown(29));
        assert_eq!(entry.deadline(), Some(now + Duration::from_secs(2)));
    }

    #[test]
    fn checking_shows_the_number_of_characters_submitted() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pwé", now);
        entry.press(Key::Enter, 4, now);
        drop(entry.submission());
        assert_eq!(entry.bytes(), b"");
        assert_eq!(entry.look().chars, 3);
        entry.checked(false, now);
        assert_eq!(entry.look().chars, 0);
    }

    #[test]
    fn a_success_resets_the_failures() {
        let (mut entry, now) = entry();
        for _ in 0..4 {
            attempt(&mut entry, false, now);
        }
        attempt(&mut entry, true, now);
        for _ in 0..4 {
            attempt(&mut entry, false, now);
            assert_eq!(entry.look().status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert!(matches!(entry.look().status, Status::Cooldown(_)));
    }

    #[test]
    fn a_reset_ends_a_cooldown_and_the_failures() {
        // A reset is what an unlock (logind's or the password's) and a new lock do.
        let (mut entry, now) = entry();
        for _ in 0..5 {
            attempt(&mut entry, false, now);
        }
        entry.reset();
        assert_eq!(entry.look().status, Status::Idle);
        assert_eq!(entry.deadline(), None);
        for _ in 0..4 {
            attempt(&mut entry, false, now);
        }
        entry.reset();
        attempt(&mut entry, false, now);
        assert_eq!(entry.look().status, Status::Failed);
    }

    #[test]
    fn caps_lock_shows() {
        let (mut entry, _) = entry();
        entry.caps_lock = true;
        assert!(entry.look().caps_lock);
    }

    #[test]
    fn debug_prints_nothing() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "secret", now);
        assert_eq!(format!("{entry:?}"), "");
    }

    #[test]
    fn dropping_the_submission_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        assert!(entry.submission().bytes().is_empty());
        assert_eq!(entry.bytes(), b"");
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        {
            let submission = entry.submission();
            assert_eq!(submission.bytes(), b"pw");
        }
        assert!(entry.page.0.iter().all(|&b| b == 0));
        assert_eq!(entry.look().status, Status::Checking);
    }

    #[test]
    fn keys_map_to_characters_except_with_alt_or_logo() {
        let key = |keysym, c: char, alt_or_logo| Key::from_xkb(keysym, c as u32, alt_or_logo);
        assert!(key(Keysym::a, 'a', false) == Some(Key::Char('a')));
        assert!(key(Keysym::a, 'a', true).is_none());
        // Ctrl+A, as xkbcommon gives it.
        assert!(key(Keysym::a, '\u{1}', false).is_none());
        assert!(key(Keysym::Shift_L, '\0', false).is_none());
        assert!(key(Keysym::Return, '\r', true) == Some(Key::Enter));
        assert!(key(Keysym::KP_Enter, '\r', false) == Some(Key::Enter));
        assert!(key(Keysym::BackSpace, '\u{8}', false) == Some(Key::Backspace));
        assert!(key(Keysym::Escape, '\u{1b}', false) == Some(Key::Escape));
    }
}
