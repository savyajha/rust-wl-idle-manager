use std::time::Duration;

use tokio::time::Instant;
use tracing::info;
use xkbcommon::xkb::Keysym;

use crate::password::Password;

/// How long a password is kept without typing before it is wiped.
const FORGET_AFTER: Duration = Duration::from_secs(30);

/// How long a failed attempt is shown.
const FAILED_FOR: Duration = Duration::from_millis(1500);

/// After every this many failed attempts in a row, no attempt is taken for `COOLDOWN`.
const COOLDOWN_AFTER: u32 = 5;
const COOLDOWN: Duration = Duration::from_secs(30);

/// A key the lock screen acts on; not `Debug`, as it may hold a character of the password.
#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Enter,
    Backspace,
    Escape,
    Char(char),
}

impl Key {
    /// The key a press means, from its `keysym` and the character it types (`utf32`, 0 for
    /// none), or `None`. With Alt or Logo held, a character is a shortcut, not text; with
    /// Ctrl, xkbcommon gives control characters, which are ignored too.
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Status {
    Idle,
    Typing,
    Checking,
    /// The last attempt failed (shown for `FAILED_FOR`, or until a key is pressed).
    Failed,
    /// Too many attempts failed: keys do nothing for this many more seconds (rounded up).
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

#[derive(Clone, Copy)]
pub struct Repeat {
    pub delay: Duration,
    pub interval: Duration,
}

struct Held {
    /// The key's code, which its release matches.
    code: u32,
    repeats: Repeats,
    next: Instant,
}

#[derive(Clone, Copy)]
enum Repeats {
    Backspace,
    /// The last character typed again, so that no copy of it is kept.
    LastChar,
}

/// The password being typed and what keys do to it; no I/O but a log line, and the time
/// is passed in.
pub struct Entry {
    password: Password,
    checking: bool,
    /// How many characters were submitted (only their number: the password is wiped).
    submitted: usize,
    failed_until: Option<Instant>,
    /// Failed attempts in a row.
    failures: u32,
    /// Keys are ignored until then, after too many failures.
    cooldown_until: Option<Instant>,
    forget_at: Option<Instant>,
    held: Option<Held>,
    repeat: Option<Repeat>,
    pub caps_lock: bool,
}

impl Entry {
    pub fn new() -> Self {
        Self {
            password: Password::new(),
            checking: false,
            submitted: 0,
            failed_until: None,
            failures: 0,
            cooldown_until: None,
            forget_at: None,
            held: None,
            repeat: None,
            caps_lock: false,
        }
    }

    /// Act on `key`, pressed at `now`; `code` identifies the physical key. Returns true when
    /// the password is to be checked. Keys do nothing while it is checked or in a cooldown.
    pub fn press(&mut self, key: Key, code: u32, now: Instant) -> bool {
        if self.checking || self.cooldown_until.is_some_and(|until| until > now) {
            return false;
        }
        self.cooldown_until = None;
        self.failed_until = None;
        self.held = None;
        match key {
            // An empty password is never checked.
            Key::Enter => {
                self.checking = !self.password.bytes().is_empty();
                self.submitted = self.password.chars();
            }
            Key::Escape => self.wipe(),
            Key::Backspace | Key::Char(_) => {
                self.edit(key, now);
                let repeats = if key == Key::Backspace {
                    Repeats::Backspace
                } else {
                    Repeats::LastChar
                };
                self.held = self.repeat.map(|repeat| Held {
                    code,
                    repeats,
                    next: now + repeat.delay,
                });
            }
        }
        self.checking
    }

    pub fn release(&mut self, code: u32) {
        if self.held.as_ref().is_some_and(|held| held.code == code) {
            self.held = None;
        }
    }

    pub fn stop_repeat(&mut self) {
        self.held = None;
    }

    pub fn set_repeat(&mut self, repeat: Option<Repeat>) {
        self.repeat = repeat;
        self.held = None;
    }

    fn edit(&mut self, key: Key, now: Instant) {
        match key {
            Key::Backspace => self.password.pop(),
            Key::Char(c) => self.password.push(c),
            Key::Enter | Key::Escape => {}
        }
        self.forget_at = (!self.password.bytes().is_empty()).then(|| now + FORGET_AFTER);
    }

    /// The submitted password (empty if none is), wiped when the guard is dropped.
    pub fn submission(&mut self) -> Submission<'_> {
        Submission(self)
    }

    /// The helper answered: show a failure unless `ok`, or start a cooldown. Returns false,
    /// ignoring the answer, if no check was under way (the lock ended since).
    pub fn checked(&mut self, ok: bool, now: Instant) -> bool {
        if !self.checking {
            return false;
        }
        let failures = if ok { 0 } else { self.failures + 1 };
        self.reset();
        self.failures = failures;
        if ok {
            return true;
        }
        if failures % COOLDOWN_AFTER == 0 {
            info!(
                "{COOLDOWN_AFTER} wrong passwords; waiting {} s",
                COOLDOWN.as_secs()
            );
            self.cooldown_until = Some(now + COOLDOWN);
        } else {
            self.failed_until = Some(now + FAILED_FOR);
        }
        true
    }

    pub fn wipe(&mut self) {
        self.password.wipe();
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

    fn cooldown_seconds(&self, now: Instant) -> Option<u64> {
        let left = self.cooldown_until?.saturating_duration_since(now);
        Some(left.as_secs() + u64::from(left.subsec_nanos() > 0))
    }

    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        let held = self.held.as_ref().map(|held| held.next);
        // The countdown's next whole second, the last of which ends the cooldown.
        let second = self.cooldown_until.zip(self.cooldown_seconds(now));
        let second =
            second.map(|(until, left)| until - Duration::from_secs(left.saturating_sub(1)));
        [held, self.failed_until, second, self.forget_at]
            .into_iter()
            .flatten()
            .min()
    }

    pub fn tick(&mut self, now: Instant) {
        if let (Some(held), Some(repeat)) = (&mut self.held, self.repeat)
            && held.next <= now
        {
            held.next = now + repeat.interval;
            let key = match held.repeats {
                Repeats::Backspace => Some(Key::Backspace),
                Repeats::LastChar => self.password.last().map(Key::Char),
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

    pub fn look(&self, now: Instant) -> Look {
        let status = if self.checking {
            Status::Checking
        } else if let Some(seconds) = self.cooldown_seconds(now).filter(|&s| s > 0) {
            Status::Cooldown(seconds)
        } else if self.failed_until.is_some_and(|until| until > now) {
            Status::Failed
        } else if !self.password.bytes().is_empty() {
            Status::Typing
        } else {
            Status::Idle
        };
        let chars = if self.checking {
            self.submitted
        } else {
            self.password.chars()
        };
        Look {
            status,
            chars,
            caps_lock: self.caps_lock,
        }
    }
}

pub struct Submission<'a>(&'a mut Entry);

impl Submission<'_> {
    pub fn bytes(&self) -> &[u8] {
        if self.0.checking {
            self.0.password.bytes()
        } else {
            &[]
        }
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
        entry.set_repeat(Some(Repeat {
            delay: DELAY,
            interval: INTERVAL,
        }));
        (entry, Instant::now())
    }

    fn type_text(entry: &mut Entry, text: &str, now: Instant) {
        for c in text.chars() {
            entry.press(Key::Char(c), 1, now);
            entry.release(1);
        }
    }

    fn typed(entry: &Entry) -> &[u8] {
        entry.password.bytes()
    }

    #[test]
    fn keys_type_and_delete_characters() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "aé€", now);
        assert_eq!(typed(&entry), "aé€".as_bytes());
        assert_eq!(entry.look(now).chars, 3);
        entry.press(Key::Backspace, 2, now);
        assert_eq!(typed(&entry), "aé".as_bytes());
    }

    #[test]
    fn escape_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "secret", now);
        assert!(!entry.press(Key::Escape, 3, now));
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.look(now).status, Status::Idle);
    }

    #[test]
    fn enter_submits_only_a_password() {
        let (mut entry, now) = entry();
        assert!(!entry.press(Key::Enter, 4, now));
        assert_eq!(entry.look(now).status, Status::Idle);
        type_text(&mut entry, "pw", now);
        assert_eq!(entry.look(now).status, Status::Typing);
        assert!(entry.press(Key::Enter, 4, now));
        assert_eq!(entry.look(now).status, Status::Checking);
    }

    #[test]
    fn keys_are_ignored_while_checking() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        for key in [Key::Char('x'), Key::Backspace, Key::Escape, Key::Enter] {
            assert!(!entry.press(key, 5, now));
        }
        assert_eq!(typed(&entry), b"pw");
    }

    #[test]
    fn a_failure_wipes_and_shows_until_it_expires_or_a_key_is_pressed() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "wrong", now);
        entry.press(Key::Enter, 4, now);
        assert!(entry.checked(false, now));
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.look(now).status, Status::Failed);
        assert_eq!(entry.deadline(now), Some(now + FAILED_FOR));
        entry.tick(now + FAILED_FOR);
        assert_eq!(entry.look(now + FAILED_FOR).status, Status::Idle);

        type_text(&mut entry, "wrong", now);
        entry.press(Key::Enter, 4, now);
        entry.checked(false, now);
        type_text(&mut entry, "r", now);
        assert_eq!(entry.look(now).status, Status::Typing);
    }

    #[test]
    fn success_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "right", now);
        entry.press(Key::Enter, 4, now);
        assert!(entry.checked(true, now));
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.look(now).status, Status::Idle);
    }

    #[test]
    fn an_answer_after_a_reset_is_ignored() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        entry.reset();
        assert!(!entry.checked(true, now));
        assert!(!entry.checked(false, now));
        assert_eq!(entry.look(now).status, Status::Idle);
    }

    #[test]
    fn an_untouched_password_is_forgotten() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "abc", now);
        let later = now + Duration::from_secs(20);
        type_text(&mut entry, "d", later);
        entry.tick(now + FORGET_AFTER);
        assert_eq!(typed(&entry), b"abcd");
        assert_eq!(entry.deadline(later), Some(later + FORGET_AFTER));
        entry.tick(later + FORGET_AFTER);
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.deadline(later), None);
    }

    #[test]
    fn held_keys_repeat_after_the_delay_until_released() {
        let (mut entry, now) = entry();
        entry.press(Key::Char('a'), 7, now);
        assert_eq!(entry.deadline(now), Some(now + DELAY));
        entry.tick(now + DELAY);
        entry.tick(now + DELAY + INTERVAL);
        assert_eq!(typed(&entry), b"aaa");
        // Another key's release does not stop it.
        entry.release(8);
        entry.tick(now + DELAY + 2 * INTERVAL);
        assert_eq!(typed(&entry), b"aaaa");
        entry.release(7);
        entry.tick(now + DELAY + 3 * INTERVAL);
        assert_eq!(typed(&entry), b"aaaa");

        entry.press(Key::Backspace, 9, now);
        entry.tick(now + DELAY);
        assert_eq!(typed(&entry), b"aa");
        entry.stop_repeat();
        entry.tick(now + DELAY + INTERVAL);
        assert_eq!(typed(&entry), b"aa");
    }

    #[test]
    fn enter_and_escape_do_not_repeat_and_stop_a_repeat() {
        let (mut entry, now) = entry();
        entry.press(Key::Char('a'), 7, now);
        entry.press(Key::Escape, 3, now);
        assert_eq!(entry.deadline(now), None);
        entry.set_repeat(None);
        entry.press(Key::Char('a'), 7, now);
        assert_eq!(entry.deadline(now), Some(now + FORGET_AFTER));
    }

    #[test]
    fn wipe_and_reset_clear_everything() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "abc", now);
        entry.press(Key::Char('d'), 7, now);
        entry.wipe();
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.deadline(now), None);
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        entry.reset();
        assert_eq!(entry.look(now).status, Status::Idle);
        assert!(!entry.press(Key::Char('x'), 1, now));
        assert_eq!(typed(&entry), b"x");
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
            assert_eq!(entry.look(now).status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert_eq!(entry.look(now).status, Status::Cooldown(30));
        assert_eq!(entry.deadline(now), Some(now + Duration::from_secs(1)));

        // Keys type nothing and submit nothing until it ends.
        for key in [Key::Char('x'), Key::Backspace, Key::Escape, Key::Enter] {
            assert!(!entry.press(key, 5, now));
        }
        assert_eq!(typed(&entry), b"");
        let almost = now + COOLDOWN - Duration::from_millis(1);
        entry.tick(almost);
        assert_eq!(entry.look(almost).status, Status::Cooldown(1));
        assert_eq!(entry.deadline(almost), Some(now + COOLDOWN));
        let now = now + COOLDOWN;
        entry.tick(now);
        assert_eq!(entry.look(now).status, Status::Idle);
        assert_eq!(entry.deadline(now), None);

        // The count goes on: the tenth failure in a row starts another.
        for _ in 0..4 {
            attempt(&mut entry, false, now);
            assert_eq!(entry.look(now).status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert!(matches!(entry.look(now).status, Status::Cooldown(_)));
    }

    #[test]
    fn the_cooldown_counts_down_each_second() {
        let (mut entry, start) = entry();
        for _ in 0..5 {
            attempt(&mut entry, false, start);
        }
        let mut seen = Vec::new();
        let mut now = start;
        while let Some(at) = entry.deadline(now) {
            now = at;
            entry.tick(now);
            seen.push((now - start, entry.look(now).status));
        }
        assert_eq!(seen.len(), 30);
        assert_eq!(seen[0], (Duration::from_secs(1), Status::Cooldown(29)));
        assert_eq!(seen[28], (Duration::from_secs(29), Status::Cooldown(1)));
        assert_eq!(seen[29], (COOLDOWN, Status::Idle));
        // From part-way through a second, the next wake is at the whole second.
        for _ in 0..5 {
            attempt(&mut entry, false, start);
        }
        let now = start + Duration::from_millis(1500);
        entry.tick(now);
        assert_eq!(entry.look(now).status, Status::Cooldown(29));
        assert_eq!(entry.deadline(now), Some(start + Duration::from_secs(2)));
    }

    #[test]
    fn an_expired_cooldown_is_not_shown_before_it_is_ticked() {
        let (mut entry, now) = entry();
        for _ in 0..5 {
            attempt(&mut entry, false, now);
        }
        let after = now + COOLDOWN;
        assert_eq!(entry.look(after).status, Status::Idle);
        assert_eq!(entry.deadline(after), Some(after));
        // A key at that moment is taken, as the look promises.
        entry.press(Key::Char('a'), 30, after);
        assert_eq!(typed(&entry), b"a");
    }

    #[test]
    fn checking_shows_the_number_of_characters_submitted() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pwé", now);
        entry.press(Key::Enter, 4, now);
        drop(entry.submission());
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.look(now).chars, 3);
        entry.checked(false, now);
        assert_eq!(entry.look(now).chars, 0);
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
            assert_eq!(entry.look(now).status, Status::Failed);
        }
        attempt(&mut entry, false, now);
        assert!(matches!(entry.look(now).status, Status::Cooldown(_)));
    }

    #[test]
    fn a_reset_ends_a_cooldown_and_the_failures() {
        // A reset is what an unlock (logind's or the password's) and a new lock do.
        let (mut entry, now) = entry();
        for _ in 0..5 {
            attempt(&mut entry, false, now);
        }
        entry.reset();
        assert_eq!(entry.look(now).status, Status::Idle);
        assert_eq!(entry.deadline(now), None);
        for _ in 0..4 {
            attempt(&mut entry, false, now);
        }
        entry.reset();
        attempt(&mut entry, false, now);
        assert_eq!(entry.look(now).status, Status::Failed);
    }

    #[test]
    fn dropping_the_submission_wipes() {
        let (mut entry, now) = entry();
        type_text(&mut entry, "pw", now);
        assert!(entry.submission().bytes().is_empty());
        assert_eq!(typed(&entry), b"");
        type_text(&mut entry, "pw", now);
        entry.press(Key::Enter, 4, now);
        {
            let submission = entry.submission();
            assert_eq!(submission.bytes(), b"pw");
        }
        assert_eq!(typed(&entry), b"");
        assert_eq!(entry.look(now).status, Status::Checking);
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
