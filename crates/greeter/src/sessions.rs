//! The sessions to log in to: the `.desktop` files of a sessions directory, as the Desktop
//! Entry Specification (1.5) has them.

use std::fs;
use std::mem;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use tracing::{info, warn};

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    /// Its file's name, such as "niri.desktop", by which it is remembered.
    pub file: String,
    pub name: String,
    /// `Exec`, split into the program and its arguments.
    pub exec: Vec<String>,
    /// `DesktopNames`, for `XDG_CURRENT_DESKTOP`.
    pub desktop_names: Vec<String>,
}

impl Session {
    /// The session `file` holds in `text`, or `None` if it is `Hidden` or `NoDisplay`.
    pub fn parse(file: &str, text: &str) -> anyhow::Result<Option<Self>> {
        let mut in_entry = false;
        let (mut name, mut exec, mut desktop_names) = (None, None, String::new());
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_entry = line == "[Desktop Entry]";
            }
            let Some((key, value)) = line.split_once('=').filter(|_| in_entry) else {
                continue;
            };
            let value = unescape(value.trim());
            // Localized keys, such as `Name[de]`, are other keys.
            match key.trim() {
                "Name" => name = Some(value),
                "Exec" => exec = Some(value),
                "DesktopNames" => desktop_names = value,
                "Hidden" | "NoDisplay" if value == "true" => return Ok(None),
                _ => {}
            }
        }
        let exec = exec.context("no Exec in [Desktop Entry]")?;
        Ok(Some(Self {
            file: file.to_owned(),
            name: name.context("no Name in [Desktop Entry]")?,
            exec: split_exec(&exec).context("invalid Exec")?,
            desktop_names: desktop_names
                .split(';')
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect(),
        }))
    }

    /// The command for greetd's `start_session`. greetd joins it with spaces and runs it
    /// with `sh -c`, so each argument is quoted for the shell.
    pub fn command(&self) -> Vec<String> {
        let quote = |arg: &String| format!("'{}'", arg.replace('\'', r"'\''"));
        self.exec.iter().map(quote).collect()
    }

    /// The session's environment, as `KEY=value`.
    pub fn env(&self) -> Vec<String> {
        let desktop = self.file.strip_suffix(".desktop").unwrap_or(&self.file);
        let mut env = vec![
            "XDG_SESSION_TYPE=wayland".to_owned(),
            format!("XDG_SESSION_DESKTOP={desktop}"),
        ];
        if !self.desktop_names.is_empty() {
            env.push(format!(
                "XDG_CURRENT_DESKTOP={}",
                self.desktop_names.join(":")
            ));
        }
        env
    }
}

/// The sessions in `dir`, sorted by name; a file that cannot be read or parsed is logged
/// and skipped.
pub fn load(dir: &Path) -> anyhow::Result<Vec<Session>> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading the sessions in {}", dir.display()))?;
    let mut sessions = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let Some(file) = path.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !file.ends_with(".desktop") {
            continue;
        }
        let session = fs::read_to_string(&path)
            .context("reading it")
            .and_then(|text| Session::parse(file, &text));
        match session {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            Err(e) => warn!("skipping the session {}: {e:#}", path.display()),
        }
    }
    sessions.sort_by(|a, b| (&a.name, &a.file).cmp(&(&b.name, &b.file)));
    ensure!(!sessions.is_empty(), "no sessions in {}", dir.display());
    Ok(sessions)
}

/// Which of `sessions` to start for a user whose last was `remembered` (its file name):
/// that one while it is there, else the first.
pub fn preselect(sessions: &[Session], remembered: Option<&str>) -> usize {
    let remembered = remembered.and_then(|file| sessions.iter().position(|s| s.file == file));
    let chosen = remembered.unwrap_or(0);
    let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
    let how = if remembered.is_some() {
        "remembered"
    } else {
        "first"
    };
    let (names, name) = (names.join(", "), &sessions[chosen].name);
    info!("sessions: {names}; {name} chosen ({how})");
    chosen
}

/// A string value's escapes: `\s`, `\n`, `\t`, `\r` and `\\`. Any other backslash is kept,
/// for `Exec`'s own quoting.
fn unescape(value: &str) -> String {
    let mut text = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            text.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => text.push(' '),
            Some('n') => text.push('\n'),
            Some('t') => text.push('\t'),
            Some('r') => text.push('\r'),
            Some('\\') => text.push('\\'),
            Some(other) => text.extend(['\\', other]),
            None => text.push('\\'),
        }
    }
    text
}

/// `Exec` split into arguments: on spaces outside double quotes, in which a backslash
/// escapes the next character. Field codes (`%f`, `%U` and the like) are dropped, and
/// an argument that was only field codes with them; `%%` is `%`.
fn split_exec(exec: &str) -> anyhow::Result<Vec<String>> {
    let mut args = Vec::new();
    let mut arg = String::new();
    // Whether `arg` has begun: it may be empty, as `""` is.
    let mut begun = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                begun = true;
            }
            '\\' if quoted => arg.push(chars.next().context("a backslash at the end")?),
            ' ' | '\t' if !quoted => {
                if mem::take(&mut begun) {
                    args.push(mem::take(&mut arg));
                }
            }
            '%' if !quoted => match chars.next() {
                Some('%') => {
                    arg.push('%');
                    begun = true;
                }
                Some(_) => {}
                None => bail!("a % at the end"),
            },
            _ => {
                arg.push(c);
                begun = true;
            }
        }
    }
    ensure!(!quoted, "a quote is not closed");
    if begun {
        args.push(arg);
    }
    ensure!(!args.is_empty(), "no program");
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> anyhow::Result<Option<Session>> {
        Session::parse("test.desktop", text)
    }

    #[test]
    fn a_session_has_its_name_command_and_desktop_names() {
        let text = "# A comment\n[Desktop Entry]\nName=Niri\nName[de]=Nirí\n\
                    Comment=A scrollable-tiling compositor\nExec=niri-session\n\
                    DesktopNames=niri;Shared;\nType=Application\n\n[Desktop Action x]\nName=Other\n";
        let session = session(text).unwrap().unwrap();
        assert_eq!(session.name, "Niri");
        assert_eq!(session.exec, ["niri-session"]);
        assert_eq!(session.desktop_names, ["niri", "Shared"]);
        assert_eq!(
            session.env(),
            [
                "XDG_SESSION_TYPE=wayland",
                "XDG_SESSION_DESKTOP=test",
                "XDG_CURRENT_DESKTOP=niri:Shared"
            ]
        );
    }

    #[test]
    fn hidden_and_broken_sessions_are_not_sessions() {
        let entry = "[Desktop Entry]\nName=N\nExec=x\n";
        assert!(session(&format!("{entry}Hidden=true\n")).unwrap().is_none());
        assert!(
            session(&format!("{entry}NoDisplay=true\n"))
                .unwrap()
                .is_none()
        );
        assert!(
            session(&format!("{entry}Hidden=false\n"))
                .unwrap()
                .is_some()
        );
        // Keys outside [Desktop Entry] do not count.
        assert!(session("Name=N\nExec=x\n[Desktop Entry]\n").is_err());
        assert!(session("[Desktop Entry]\nName=N\n").is_err());
        assert!(session("[Desktop Entry]\nExec=x\n").is_err());
        assert!(session("[Desktop Entry]\nName=N\nExec=\"x\n").is_err());
        assert!(session("[Desktop Entry]\nName=N\nExec=%U\n").is_err());
    }

    #[test]
    fn exec_splits_on_spaces_outside_quotes() {
        let split = |exec: &str| split_exec(&unescape(exec)).unwrap();
        assert_eq!(split("a  b\tc"), ["a", "b", "c"]);
        assert_eq!(
            split(r#"sh -c "echo \"hi\" there""#),
            ["sh", "-c", r#"echo "hi" there"#]
        );
        assert_eq!(split(r#"a "" b"#), ["a", "", "b"]);
        // The string escape first, then the quoting: four backslashes are one.
        assert_eq!(split(r#""a\\\\b" \$x"#), [r"a\b", r"\$x"]);
        assert_eq!(split(r#""\$HOME \`x\`""#), ["$HOME `x`"]);
        // `\s` is a space, which separates arguments like any other.
        assert_eq!(split(r"a\sb"), ["a", "b"]);
    }

    #[test]
    fn field_codes_are_dropped() {
        let split = |exec: &str| split_exec(exec).unwrap();
        assert_eq!(split("app %U"), ["app"]);
        assert_eq!(split("app %f %i --x=%c 100%%"), ["app", "--x=", "100%"]);
        assert_eq!(split(r#"app "%f""#), ["app", "%f"]);
        assert!(split_exec("app %").is_err());
        assert!(split_exec(r#""a\"#).is_err());
    }

    #[test]
    fn the_command_is_quoted_for_greetds_shell() {
        let session = Session {
            file: "x.desktop".into(),
            name: "X".into(),
            exec: vec![
                "/bin/x".into(),
                "two words".into(),
                "it's".into(),
                "".into(),
            ],
            desktop_names: Vec::new(),
        };
        assert_eq!(
            session.command(),
            ["'/bin/x'", "'two words'", r"'it'\''s'", "''"]
        );
        assert_eq!(
            session.env(),
            ["XDG_SESSION_TYPE=wayland", "XDG_SESSION_DESKTOP=x"]
        );
    }

    #[test]
    fn sessions_load_sorted_by_name_skipping_the_hidden_and_broken() {
        let dir = std::env::temp_dir().join(format!("greeter-sessions-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let write = |file: &str, text: &str| fs::write(dir.join(file), text).unwrap();
        write("b.desktop", "[Desktop Entry]\nName=Beta\nExec=b\n");
        write("a.desktop", "[Desktop Entry]\nName=Gamma\nExec=g\n");
        write("c.desktop", "[Desktop Entry]\nName=Alpha\nExec=a\n");
        write(
            "hidden.desktop",
            "[Desktop Entry]\nName=A\nExec=h\nHidden=true\n",
        );
        write("broken.desktop", "[Desktop Entry]\nName=A\n");
        write("notes.txt", "[Desktop Entry]\nName=A\nExec=n\n");
        let sessions = load(&dir).unwrap();
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "Beta", "Gamma"]);
        fs::remove_dir_all(&dir).unwrap();
        assert!(load(&dir).is_err());
    }

    #[test]
    fn the_remembered_session_is_preselected_while_it_is_there() {
        let sessions = ["a", "b"].map(|name| Session {
            file: format!("{name}.desktop"),
            name: name.into(),
            exec: vec![name.into()],
            desktop_names: Vec::new(),
        });
        assert_eq!(preselect(&sessions, Some("b.desktop")), 1);
        assert_eq!(preselect(&sessions, Some("gone.desktop")), 0);
        assert_eq!(preselect(&sessions, None), 0);
    }
}
