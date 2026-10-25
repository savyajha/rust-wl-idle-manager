use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use knuffel::ast::SpannedNode;
use knuffel::decode::Context as DecodeContext;
use knuffel::errors::DecodeError;
use knuffel::traits::{DecodeChildren, ErrorSpan};
use ui::config::{Argv, LockScreen, render};

/// The longest timeout: ext-idle-notify takes milliseconds as a u32, about 49 days.
const MAX_SECS: u64 = u32::MAX as u64 / 1000;

#[derive(knuffel::Decode, Debug, PartialEq)]
pub struct Config {
    /// The locker; without one, the daemon locks with its built-in lock screen.
    #[knuffel(child)]
    pub locker: Option<Argv>,
    #[knuffel(children(name = "timeout"))]
    pub timeouts: Vec<Timeout>,
    #[knuffel(child)]
    pub lock_screen: Option<LockScreen>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Timeout {
    pub after: Duration,
    pub action: Action,
    pub on_resume: Option<Argv>,
    pub ignore_inhibit: bool,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub enum Action {
    Lock,
    Suspend,
    SuspendThenHibernate,
    Hibernate,
    Spawn(Argv),
}

/// A `timeout` node as knuffel decodes it, before the checks `Timeout` adds.
#[derive(knuffel::Decode)]
struct RawTimeout {
    #[knuffel(argument)]
    secs: u64,
    #[knuffel(child)]
    ignore_inhibit: bool,
    #[knuffel(child)]
    on_resume: Option<Argv>,
    /// Every other child; exactly one is allowed.
    #[knuffel(children)]
    actions: Vec<Action>,
}

/// The config file's nodes: a `Config`, without a `lock-screen` if there is a `locker`.
struct Document(Config);

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&path.display().to_string(), &text)
            .with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parse and validate KDL `text`; `name` labels it in errors.
    fn parse(name: &str, text: &str) -> anyhow::Result<Self> {
        let Document(mut config) = knuffel::parse(name, text).map_err(render)?;
        if config.locker.is_none() && config.lock_screen.is_none() {
            config.lock_screen = Some(LockScreen::default_layout());
        }
        Ok(config)
    }
}

impl<S: ErrorSpan> knuffel::Decode<S> for Timeout {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut DecodeContext<S>,
    ) -> Result<Self, DecodeError<S>> {
        let raw = RawTimeout::decode_node(node, ctx)?;
        if !(1..=MAX_SECS).contains(&raw.secs) {
            ctx.emit_error(DecodeError::conversion(
                &node.arguments[0].literal,
                format!("expected 1 to {MAX_SECS} seconds"),
            ));
        }
        let actions = node
            .children()
            .filter(|child| !matches!(&**child.node_name, "ignore-inhibit" | "on-resume"));
        for extra in actions.skip(1) {
            ctx.emit_error(DecodeError::unexpected(
                extra,
                "node",
                "only one action is allowed per timeout",
            ));
        }
        let Some(action) = raw.actions.into_iter().next() else {
            return Err(DecodeError::missing(
                node,
                "expected an action for this timeout",
            ));
        };
        Ok(Self {
            after: Duration::from_secs(raw.secs),
            action,
            on_resume: raw.on_resume,
            ignore_inhibit: raw.ignore_inhibit,
        })
    }
}

impl<S: ErrorSpan> DecodeChildren<S> for Document {
    fn decode_children(
        nodes: &[SpannedNode<S>],
        ctx: &mut DecodeContext<S>,
    ) -> Result<Self, DecodeError<S>> {
        let config = Config::decode_children(nodes, ctx)?;
        let lock_screen = nodes.iter().find(|node| &**node.node_name == "lock-screen");
        if config.locker.is_some()
            && let Some(node) = lock_screen
        {
            ctx.emit_error(DecodeError::unexpected(
                node,
                "node",
                "lock-screen has no effect with locker",
            ));
        }
        Ok(Self(config))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::config::{Anchor, Weight};

    const SAMPLE: &str = r#"
        locker "hyprlock-wallpaper"
        timeout 300 { lock; }
        timeout 600 {
            ignore-inhibit
            spawn "niri" "msg" "action" "power-off-monitors"
            on-resume "niri" "msg" "action" "power-on-monitors"
        }
        timeout 1200 { suspend-then-hibernate; }
    "#;

    fn argv(args: &[&str]) -> Argv {
        let (program, args) = args.split_first().unwrap();
        let program = program.to_string();
        let args = args.iter().map(|s| s.to_string()).collect();
        Argv { program, args }
    }

    fn parse_err(text: &str) -> String {
        format!("{:#}", Config::parse("test.kdl", text).unwrap_err())
    }

    #[test]
    fn sample_parses() {
        let config = Config::parse("test.kdl", SAMPLE).unwrap();
        let power = |action| argv(&["niri", "msg", "action", action]);
        assert_eq!(
            config,
            Config {
                locker: Some(argv(&["hyprlock-wallpaper"])),
                timeouts: vec![
                    Timeout {
                        after: Duration::from_secs(300),
                        action: Action::Lock,
                        on_resume: None,
                        ignore_inhibit: false,
                    },
                    Timeout {
                        after: Duration::from_secs(600),
                        action: Action::Spawn(power("power-off-monitors")),
                        on_resume: Some(power("power-on-monitors")),
                        ignore_inhibit: true,
                    },
                    Timeout {
                        after: Duration::from_secs(1200),
                        action: Action::SuspendThenHibernate,
                        on_resume: None,
                        ignore_inhibit: false,
                    },
                ],
                lock_screen: None,
            }
        );
    }

    #[test]
    fn without_a_lock_screen_block_the_default_is_used() {
        let config = Config::parse("test.kdl", "timeout 5 { lock; }\n").unwrap();
        assert_eq!(config.locker, None);
        let lock_screen = config.lock_screen.unwrap();
        assert_eq!(lock_screen.password.place.anchor, Some(Anchor::Bottom));
        assert_eq!(
            lock_screen.clock.unwrap().style.weight,
            Some(Weight(pango::Weight::Semibold))
        );
    }

    #[test]
    fn a_lock_screen_with_a_locker_is_rejected_where_it_is() {
        let err = parse_err("locker \"x\"\nlock-screen { password; }\n");
        assert!(
            err.contains("lock-screen has no effect with locker"),
            "{err}"
        );
        assert!(err.contains("at line 2, columns 1"), "{err}");
    }

    #[test]
    fn unknown_node_is_rejected() {
        let err = parse_err("locker \"x\"\nlockr \"y\"\n");
        assert!(err.contains("unexpected node `lockr`"), "{err}");
    }

    #[test]
    fn unknown_property_is_rejected() {
        let err = parse_err("locker \"x\"\ntimeout 5 delay=1 { lock; }\n");
        assert!(err.contains("unexpected property `delay`"), "{err}");
    }

    #[test]
    fn timeout_without_action_is_rejected() {
        let err = parse_err("locker \"x\"\ntimeout 5 { ignore-inhibit; }\n");
        assert!(err.contains("expected an action for this timeout"), "{err}");
    }

    #[test]
    fn timeout_with_two_actions_is_rejected_at_its_line() {
        let err = parse_err("locker \"x\"\ntimeout 5 {\n    lock\n    spawn \"true\"\n}\n");
        assert!(
            err.contains("only one action is allowed per timeout"),
            "{err}"
        );
        assert!(err.contains("at line 4, column 5"), "{err}");
    }

    #[test]
    fn unknown_action_is_rejected() {
        let err = parse_err("locker \"x\"\ntimeout 5 { lok; }\n");
        assert!(
            err.contains("expected `lock`, `suspend`, or one of 3 others"),
            "{err}"
        );
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (text, want) in [
            (
                "locker \"x\"\ntimeout 0 { lock; }\n",
                "expected 1 to 4294967 seconds",
            ),
            (
                "locker \"x\"\ntimeout 4294968 { lock; }\n",
                "expected 1 to 4294967 seconds",
            ),
            ("locker\n", "additional argument `program` is required"),
            (
                "locker \"x\"\ntimeout 5 { spawn; }\n",
                "additional argument `program` is required",
            ),
            (
                "locker \"x\"\ntimeout 5 { lock; on-resume; }\n",
                "additional argument `program` is required",
            ),
        ] {
            let err = parse_err(text);
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }
}
