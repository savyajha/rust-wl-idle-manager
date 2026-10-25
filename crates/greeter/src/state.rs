//! What the greeter remembers from one login to the next: the last user and each user's
//! last session, in a small KDL file of a `last-user "user"` line and `session "user"
//! "file.desktop"` lines. The last user can be taken from tuigreet's, once.

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::Context;
use tracing::warn;
use ui::config::render;

use crate::users::User;

/// The most of tuigreet's last user file that is read: a user name is at most 32 bytes.
const TUIGREET_CAPACITY: u64 = 256;

#[derive(knuffel::Decode, Debug, Default, PartialEq)]
pub struct State {
    #[knuffel(child, unwrap(argument))]
    last_user: Option<String>,
    #[knuffel(children(name = "session"))]
    sessions: Vec<Remembered>,
}

#[derive(knuffel::Decode, Debug, PartialEq)]
struct Remembered {
    #[knuffel(argument)]
    user: String,
    /// The session's file name.
    #[knuffel(argument)]
    file: String,
}

impl State {
    /// The state in `path`: empty if there is none, and, with a warning, if it cannot be
    /// read or parsed.
    pub fn load(path: &Path) -> Self {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                warn!("ignoring the state {}: {e}", path.display());
                return Self::default();
            }
        };
        let name = path.display().to_string();
        knuffel::parse(&name, &text)
            .map_err(render)
            .unwrap_or_else(|e| {
                warn!("ignoring the state {name}: {e:#}");
                Self::default()
            })
    }

    pub fn last_user(&self) -> Option<&str> {
        self.last_user.as_deref()
    }

    pub fn set_last_user(&mut self, user: &str) {
        self.last_user = Some(user.to_owned());
    }

    /// Take the last user from tuigreet's file at `path` (its text, a user name), unless
    /// one is remembered, or the file is larger than a user name could be, or it names
    /// none of `users`. Returns whether it did.
    pub fn seed_last_user(&mut self, path: &Path, users: &[User]) -> bool {
        if self.last_user.is_some() {
            return false;
        }
        // Not through a link, nor waiting at a FIFO (before the first frame): a file only.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path);
        let mut bytes = Vec::new();
        let read = file.and_then(|file| {
            if !file.metadata()?.is_file() {
                return Err(io::Error::other("not a file"));
            }
            file.take(TUIGREET_CAPACITY + 1).read_to_end(&mut bytes)
        });
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return false,
            Err(e) => {
                warn!("ignoring tuigreet's last user {}: {e}", path.display());
                return false;
            }
        }
        if bytes.len() as u64 > TUIGREET_CAPACITY {
            return false;
        }
        let name = String::from_utf8(bytes).unwrap_or_default();
        let name = name.trim();
        if !users.iter().any(|user| user.name == name) {
            return false;
        }
        self.last_user = Some(name.to_owned());
        true
    }

    /// The file name of `user`'s last session.
    pub fn session(&self, user: &str) -> Option<&str> {
        let remembered = self.sessions.iter().find(|r| r.user == user)?;
        Some(&remembered.file)
    }

    pub fn set_session(&mut self, user: &str, file: &str) {
        self.sessions.retain(|r| r.user != user);
        self.sessions.push(Remembered {
            user: user.to_owned(),
            file: file.to_owned(),
        });
    }

    /// Write the state to `path` (mode 0600), replacing it at once: written to a new file
    /// beside it (never through a link left there), then renamed over it.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let mut text = String::new();
        if let Some(user) = &self.last_user {
            text += &format!("last-user {}\n", quote(user));
        }
        for Remembered { user, file } in &self.sessions {
            text += &format!("session {} {}\n", quote(user), quote(file));
        }
        let temporary = path.with_extension("new");
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
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("creating {}", temporary.display()))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", temporary.display()))?;
        fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;
        // The rename itself is in the directory.
        let dir = path.parent().context("the state has no directory")?;
        fs::File::open(dir)
            .and_then(|dir| dir.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))
    }
}

/// `text` as a KDL string.
fn quote(text: &str) -> String {
    let mut quoted = String::from('"');
    for c in text.chars() {
        match c {
            '"' => quoted += "\\\"",
            '\\' => quoted += "\\\\",
            c if c.is_control() => quoted += &format!("\\u{{{:x}}}", u32::from(c)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_is_saved_and_loaded_again() {
        let dir = std::env::temp_dir().join(format!("greeter-state-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state");
        assert_eq!(State::load(&path), State::default());
        let mut state = State::default();
        state.set_last_user("bob");
        state.set_last_user("alice");
        state.set_session("alice", "niri.desktop");
        state.set_session("bob", "odd \"name\" \\ é\n.desktop");
        state.set_session("alice", "sway.desktop");
        state.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let first =
            "last-user \"alice\"\nsession \"bob\" \"odd \\\"name\\\" \\\\ é\\u{a}.desktop\"\n";
        assert!(text.starts_with(first), "{text}");
        let loaded = State::load(&path);
        assert_eq!(loaded, state);
        assert_eq!(loaded.session("alice"), Some("sway.desktop"));
        assert_eq!(loaded.session("carol"), None);
        assert_eq!(loaded.last_user(), Some("alice"));
        let mode = fs::metadata(&path).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o600
        );
        assert!(!path.with_extension("new").exists());
        // A stale temporary file, even a link, is replaced, not written through.
        let elsewhere = dir.join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, path.with_extension("new")).unwrap();
        state.save(&path).unwrap();
        assert!(!elsewhere.exists());
        assert_eq!(State::load(&path), state);

        // An invalid state is ignored, as is one that cannot be read (a directory).
        fs::write(&path, "session \"alice\"\n").unwrap();
        assert_eq!(State::load(&path), State::default());
        assert_eq!(State::load(&dir), State::default());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tuigreets_last_user_is_taken_once_if_listed() {
        let dir = std::env::temp_dir().join(format!("greeter-seed-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lastuser");
        let users = [User {
            name: "bob".into(),
            full_name: "Bob".into(),
        }];
        let seeded = |text: &str| {
            fs::write(&path, text).unwrap();
            let mut state = State::default();
            let seeded = state.seed_last_user(&path, &users);
            assert_eq!(seeded, state.last_user() == Some("bob"));
            seeded
        };
        // As tuigreet writes it, without a newline; with one too.
        assert!(seeded("bob"));
        assert!(seeded("bob\n"));
        // Not a listed user, or not a user name at all.
        assert!(!seeded("dave"));
        assert!(!seeded(""));
        assert!(!seeded(&format!("bob{}", " ".repeat(300))));
        fs::write(&path, b"\xffbob").unwrap();
        assert!(!State::default().seed_last_user(&path, &users));
        // Not over a last user remembered, listed or not.
        fs::write(&path, "bob").unwrap();
        let mut state = State::default();
        state.set_last_user("dave");
        assert!(!state.seed_last_user(&path, &users));
        assert_eq!(state.last_user(), Some("dave"));
        // No file, or not a file: a directory, a FIFO (which would block), a link.
        assert!(!State::default().seed_last_user(&dir.join("none"), &users));
        assert!(!State::default().seed_last_user(&dir, &users));
        let fifo = dir.join("fifo");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        assert!(made.unwrap().success());
        assert!(!State::default().seed_last_user(&fifo, &users));
        let link = dir.join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(!State::default().seed_last_user(&link, &users));
        assert!(State::default().seed_last_user(&path, &users));
        fs::remove_dir_all(&dir).unwrap();
    }
}
