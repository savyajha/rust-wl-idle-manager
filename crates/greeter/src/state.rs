//! What the greeter remembers from one login to the next: each user's last session, in a
//! small KDL file of `session "user" "file.desktop"` lines.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::Context;
use tracing::warn;
use ui::config::render;

#[derive(knuffel::Decode, Debug, Default, PartialEq)]
pub struct State {
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
        state.set_session("alice", "niri.desktop");
        state.set_session("bob", "odd \"name\" \\ é\n.desktop");
        state.set_session("alice", "sway.desktop");
        state.save(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("session \"bob\" \"odd \\\"name\\\" \\\\ é\\u{a}.desktop\"\n"));
        let loaded = State::load(&path);
        assert_eq!(loaded, state);
        assert_eq!(loaded.session("alice"), Some("sway.desktop"));
        assert_eq!(loaded.session("carol"), None);
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
}
