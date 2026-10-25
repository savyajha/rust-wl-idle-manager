use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use ui::config::{LockScreen, render};

/// The look without a `look` block: the lock screen's, with the avatar, name and field 100
/// pixels higher, leaving room under the field for the message line and the session chip.
const DEFAULT_LOOK: &str = r##"
look {
    background
    date { anchor "top"; offset 0 64; size 26; weight "semibold"; color "#ffffff" 0.88; }
    clock {
        anchor "top"; offset 0 80; size 152; weight "semibold"; letter-spacing -5
        color "#ffffff" 0.82
    }
    avatar { anchor "bottom"; offset 0 249; }
    name { anchor "bottom"; offset 0 221; weight "semibold"; }
    password { anchor "bottom"; offset 0 156; }
}
"##;

#[derive(knuffel::Decode, Debug, PartialEq)]
pub struct Config {
    /// The user who logs in.
    #[knuffel(child, unwrap(argument))]
    pub user: String,
    /// The directory of the sessions' `.desktop` files.
    #[knuffel(child, unwrap(argument))]
    pub sessions: PathBuf,
    /// The xkb layout to type with, instead of the compositor's keymap.
    #[knuffel(child, unwrap(argument))]
    pub keyboard_layout: Option<String>,
    /// The lock screen's widgets, as its `lock-screen` block has them.
    #[knuffel(child, default = default_look())]
    pub look: LockScreen,
}

/// A document of one `look` node.
#[derive(knuffel::Decode)]
struct Look {
    #[knuffel(child)]
    look: LockScreen,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&path.display().to_string(), &text)
            .with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parse and validate KDL `text`; `name` labels it in errors.
    fn parse(name: &str, text: &str) -> anyhow::Result<Self> {
        knuffel::parse(name, text).map_err(render)
    }
}

fn default_look() -> LockScreen {
    let Look { look } = knuffel::parse("DEFAULT_LOOK", DEFAULT_LOOK)
        .map_err(render)
        .expect("the default look parses");
    look
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_takes_its_user_sessions_layout_and_look() {
        let text = r#"
            user "alice"
            sessions "/run/sessions"
            keyboard-layout "de"
            look { password { anchor "top"; }; }
        "#;
        let config = Config::parse("test.kdl", text).unwrap();
        assert_eq!(config.user, "alice");
        assert_eq!(config.sessions, PathBuf::from("/run/sessions"));
        assert_eq!(config.keyboard_layout.as_deref(), Some("de"));
        assert_eq!(config.look.avatar, None);
    }

    #[test]
    fn without_a_look_the_default_one_is_used() {
        let config = Config::parse("test.kdl", "user \"a\"; sessions \"/s\"").unwrap();
        let look = config.look;
        assert!(look.avatar.is_some() && look.clock.is_some());
        assert_eq!(config.keyboard_layout, None);
    }

    #[test]
    fn bad_configs_are_rejected_where_they_are() {
        for (text, want) in [
            ("sessions \"/s\"", "child node `user` is required"),
            ("user \"a\"", "child node `sessions` is required"),
            (
                "user 1; sessions \"/s\"",
                "expected string scalar, found integer",
            ),
            (
                "user \"a\"; sessions \"/s\"; colour 1",
                "unexpected node `colour`",
            ),
            (
                "user \"a\"; sessions \"/s\"; look { clock; }",
                "`password` is required",
            ),
        ] {
            let err = format!("{:#}", Config::parse("test.kdl", text).unwrap_err());
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }
}
