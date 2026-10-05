use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, ensure};
use knuffel::ast::SpannedNode;
use knuffel::decode::Context as DecodeContext;
use knuffel::errors::DecodeError;
use knuffel::traits::ErrorSpan;

/// The longest timeout: ext-idle-notify takes milliseconds as a u32, about 49 days.
const MAX_SECS: u64 = u32::MAX as u64 / 1000;

/// The rust-wl-idle-manager config file.
#[derive(knuffel::Decode, Debug, PartialEq)]
pub struct Config {
    /// The locker's argv; without one, the daemon locks with its built-in lock screen.
    #[knuffel(child, unwrap(arguments))]
    pub locker: Option<Vec<String>>,
    /// The idle timeouts, in file order.
    #[knuffel(children(name = "timeout"))]
    pub timeouts: Vec<Timeout>,
}

/// One `timeout` node: what to do after a period of idleness.
#[derive(Debug, PartialEq)]
pub struct Timeout {
    /// How long the session must be idle.
    pub after: Duration,
    /// What to do once it is.
    pub action: Action,
    /// The argv to run when the session is no longer idle.
    pub on_resume: Option<Vec<String>>,
    /// Count only input, ignoring idle inhibitors.
    pub ignore_inhibit: bool,
}

/// The action of a timeout.
#[derive(knuffel::Decode, Debug, PartialEq)]
pub enum Action {
    Lock,
    Suspend,
    SuspendThenHibernate,
    Hibernate,
    Spawn(#[knuffel(arguments)] Vec<String>),
}

/// A `timeout` node as knuffel decodes it, before the checks `Timeout` adds.
#[derive(knuffel::Decode)]
struct RawTimeout {
    /// The delay in seconds, not yet range-checked.
    #[knuffel(argument)]
    secs: u64,
    /// The `ignore-inhibit` flag node.
    #[knuffel(child)]
    ignore_inhibit: bool,
    /// The `on-resume` argv, possibly empty.
    #[knuffel(child, unwrap(arguments))]
    on_resume: Option<Vec<String>>,
    /// Every other child; exactly one is allowed.
    #[knuffel(children)]
    actions: Vec<Action>,
}

impl Config {
    /// Read, parse and validate the KDL config at `path`.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&path.display().to_string(), &text)
            .with_context(|| format!("invalid config {}", path.display()))
    }

    /// Parse and validate KDL `text`; `name` labels it in errors.
    fn parse(name: &str, text: &str) -> anyhow::Result<Self> {
        let config: Self = knuffel::parse(name, text).map_err(render)?;
        ensure!(
            config.locker.as_ref().is_none_or(|argv| !argv.is_empty()),
            "locker: needs a command"
        );
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
        for child in node.children() {
            let name = &**child.node_name;
            if matches!(name, "spawn" | "on-resume") && child.arguments.is_empty() {
                ctx.emit_error(DecodeError::missing(
                    child,
                    format!("{name} needs a command"),
                ));
            }
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

/// Turn knuffel's error, whose Display is only "error parsing KDL", into one listing each problem.
fn render(err: knuffel::Error) -> anyhow::Error {
    let mut text = String::new();
    let _ = miette::NarratableReportHandler::new().render_report(&mut text, &err);
    anyhow::anyhow!("{}", text.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        locker "hyprlock-wallpaper"          // argv; one or more string args
        timeout 300 { lock; }
        timeout 600 {
            ignore-inhibit
            spawn "niri" "msg" "action" "power-off-monitors"
            on-resume "niri" "msg" "action" "power-on-monitors"
        }
        timeout 1200 { suspend-then-hibernate; }
    "#;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
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
            }
        );
    }

    #[test]
    fn locker_is_optional() {
        let config = Config::parse("test.kdl", "timeout 5 { lock; }\n").unwrap();
        assert_eq!(config.locker, None);
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
            ("locker\n", "locker: needs a command"),
            (
                "locker \"x\"\ntimeout 5 { spawn; }\n",
                "spawn needs a command",
            ),
            (
                "locker \"x\"\ntimeout 5 { lock; on-resume; }\n",
                "on-resume needs a command",
            ),
        ] {
            let err = parse_err(text);
            assert!(err.contains(want), "{text:?}: {err}");
        }
    }
}
