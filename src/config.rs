use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use knuffel::ast::{Literal, SpannedNode, TypeName};
use knuffel::decode::{Context as DecodeContext, Kind};
use knuffel::errors::DecodeError;
use knuffel::span::Spanned;
use knuffel::traits::{DecodeChildren, ErrorSpan};
use pango::glib::prelude::StaticType;
use pango::glib::{DateTime, EnumClass};

use crate::theme::{self, Rgba};

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

/// The lock screen without a `lock-screen` block: the date and a large clock at the top, the
/// avatar, name and password field at the bottom, laid out for a 1280×720 output.
pub const DEFAULT_LOCK_SCREEN: &str = r##"
lock-screen {
    background
    date { anchor "top"; offset 0 64; size 26; weight "semibold"; color "#ffffff" 0.88; }
    clock {
        anchor "top"; offset 0 80; size 152; weight "semibold"; letter-spacing -5
        color "#ffffff" 0.82
    }
    avatar { anchor "bottom"; offset 0 149; }
    name { anchor "bottom"; offset 0 121; weight "semibold"; }
    password { anchor "bottom"; offset 0 56; }
}
"##;

/// A number from `MIN` to `MAX`, written as an integer or a decimal.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Number<const MIN: i32, const MAX: i32>(pub f64);

/// A size in logical pixels.
pub type Size = Number<1, 1000>;
pub type Distance = Number<-10000, 10000>;
pub type Fraction = Number<0, 1>;
pub type Factor = Number<0, 10>;

#[derive(knuffel::DecodeScalar, Clone, Copy, Debug, Default, PartialEq)]
pub enum Anchor {
    TopLeft,
    Top,
    TopRight,
    Left,
    #[default]
    Center,
    Right,
    BottomLeft,
    Bottom,
    BottomRight,
}

/// A font weight, by Pango's name for it, such as "semibold".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Weight(pub pango::Weight);

/// Where a widget goes: at `anchor`, moved by `offset` logical pixels away from its edges.
#[derive(knuffel::Decode, Clone, Copy, Debug, Default, PartialEq)]
pub struct Place {
    #[knuffel(child, unwrap(argument))]
    pub anchor: Option<Anchor>,
    #[knuffel(child)]
    pub offset: Option<Offset>,
}

#[derive(knuffel::Decode, Clone, Copy, Debug, Default, PartialEq)]
pub struct Offset(
    #[knuffel(argument)] pub Distance,
    #[knuffel(argument)] pub Distance,
);

/// The opacities of the background's top and bottom gradients.
#[derive(knuffel::Decode, Clone, Copy, Debug, PartialEq)]
pub struct Gradient(
    #[knuffel(argument)] pub Fraction,
    #[knuffel(argument)] pub Fraction,
);

/// A color, and an opacity it is multiplied by.
#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Color {
    #[knuffel(argument, str)]
    pub name: ColorName,
    #[knuffel(argument, default = Number(1.0))]
    pub alpha: Fraction,
}

/// `#` and six or eight hex digits, or a GTK color name (a letter or `_`, then letters,
/// digits, `_` and `-`), which gtk.css may define.
#[derive(Clone, Debug, PartialEq)]
pub enum ColorName {
    Literal(Rgba),
    Named(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Format(pub String);

/// A program and its arguments; never empty.
#[derive(Clone, Debug, PartialEq)]
pub struct Argv(pub Vec<String>);

#[derive(knuffel::Decode)]
struct RawArgv {
    #[knuffel(argument)]
    program: String,
    #[knuffel(arguments)]
    args: Vec<String>,
}

/// The lock screen's widgets; only `password` is required, or nothing could unlock.
#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct LockScreen {
    #[knuffel(child)]
    pub background: Option<Background>,
    #[knuffel(child)]
    pub date: Option<Text>,
    #[knuffel(child)]
    pub clock: Option<Text>,
    #[knuffel(child)]
    pub avatar: Option<Avatar>,
    #[knuffel(child)]
    pub name: Option<Name>,
    #[knuffel(child)]
    pub password: Password,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Background {
    #[knuffel(child)]
    pub wallpaper_command: Option<Argv>,
    /// The blur's radius, in the wallpaper's pixels.
    #[knuffel(child, unwrap(argument), default = Number(24.0))]
    pub blur: Number<0, 500>,
    #[knuffel(child, unwrap(argument), default = Number(0.8))]
    pub brightness: Factor,
    #[knuffel(child, unwrap(argument), default = Number(1.1))]
    pub saturation: Factor,
    #[knuffel(child, default = Gradient(Number(0.35), Number(0.45)))]
    pub gradient: Gradient,
    /// Without a wallpaper (none configured, or it failed).
    #[knuffel(child, default = Color::new("#202428"))]
    pub color: Color,
}

/// How text looks; by default 15 px, normal weight, white.
#[derive(knuffel::Decode, Clone, Debug, Default, PartialEq)]
pub struct Style {
    #[knuffel(child, unwrap(argument))]
    pub size: Option<Size>,
    #[knuffel(child, unwrap(argument, str))]
    pub weight: Option<Weight>,
    #[knuffel(child, unwrap(argument))]
    pub letter_spacing: Option<Number<-100, 100>>,
    #[knuffel(child)]
    pub color: Option<Color>,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Text {
    #[knuffel(flatten(child))]
    pub place: Place,
    #[knuffel(flatten(child))]
    pub style: Style,
    #[knuffel(child, unwrap(argument, str))]
    pub format: Option<Format>,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Name {
    #[knuffel(flatten(child))]
    pub place: Place,
    #[knuffel(flatten(child))]
    pub style: Style,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Avatar {
    #[knuffel(flatten(child))]
    pub place: Place,
    #[knuffel(child, unwrap(argument), default = Number(64.0))]
    pub diameter: Size,
    /// The initial's color.
    #[knuffel(child, default = Color::new("#ffffff"))]
    pub color: Color,
}

#[derive(knuffel::Decode, Clone, Debug, PartialEq)]
pub struct Password {
    #[knuffel(flatten(child))]
    pub place: Place,
    #[knuffel(child, unwrap(argument), default = Number(176.0))]
    pub width: Number<1, 4000>,
    #[knuffel(child, unwrap(argument), default = Number(30.0))]
    pub height: Size,
    #[knuffel(child, unwrap(argument), default = Number(15.0))]
    pub radius: Number<0, 500>,
    #[knuffel(child, unwrap(argument), default = Number(4.0))]
    pub dot_size: Number<1, 100>,
    /// The text, dots and arrow.
    #[knuffel(child, default = Color::new("#ffffff"))]
    pub color: Color,
    #[knuffel(child, default = Color::new("error_color"))]
    pub error_color: Color,
}

impl LockScreen {
    pub fn colors(&self) -> impl Iterator<Item = &Color> {
        let texts = [&self.date, &self.clock].into_iter().flatten();
        let styles = texts
            .map(|text| &text.style)
            .chain(self.name.as_ref().map(|name| &name.style));
        let backgrounds = self.background.as_ref().map(|background| &background.color);
        let avatars = self.avatar.as_ref().map(|avatar| &avatar.color);
        styles
            .filter_map(|style| style.color.as_ref())
            .chain(backgrounds)
            .chain(avatars)
            .chain([&self.password.color, &self.password.error_color])
    }
}

impl Color {
    /// `name`, opaque; for the defaults, so it must parse.
    fn new(name: &str) -> Self {
        Self {
            name: name.parse().expect("a valid color"),
            alpha: Number(1.0),
        }
    }
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
            let default: Self = knuffel::parse("DEFAULT_LOCK_SCREEN", DEFAULT_LOCK_SCREEN)
                .expect("the default lock screen parses");
            config.lock_screen = default.lock_screen;
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

impl<S: ErrorSpan> knuffel::Decode<S> for Argv {
    fn decode_node(
        node: &SpannedNode<S>,
        ctx: &mut DecodeContext<S>,
    ) -> Result<Self, DecodeError<S>> {
        let RawArgv { program, args } = RawArgv::decode_node(node, ctx)?;
        Ok(Self([program].into_iter().chain(args).collect()))
    }
}

impl<S: ErrorSpan, const MIN: i32, const MAX: i32> knuffel::DecodeScalar<S> for Number<MIN, MAX> {
    fn type_check(_: &Option<Spanned<TypeName, S>>, _: &mut DecodeContext<S>) {}

    fn raw_decode(
        value: &Spanned<Literal, S>,
        _: &mut DecodeContext<S>,
    ) -> Result<Self, DecodeError<S>> {
        let number = match &**value {
            Literal::Int(int) => i64::try_from(int).map(|int| int as f64).ok(),
            Literal::Decimal(decimal) => f64::try_from(decimal).ok(),
            _ => return Err(DecodeError::scalar_kind(Kind::Decimal, value)),
        };
        let range = f64::from(MIN)..=f64::from(MAX);
        match number.filter(|number| range.contains(number)) {
            Some(number) => Ok(Number(number)),
            None => Err(DecodeError::conversion(
                value,
                format!("expected {MIN} to {MAX}"),
            )),
        }
    }
}

impl FromStr for ColorName {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let named = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b));
        if let Some(rgba) = theme::hex(name) {
            Ok(Self::Literal(rgba))
        } else if named {
            Ok(Self::Named(name.to_owned()))
        } else {
            Err("expected #rrggbb, #rrggbbaa or a GTK colour name")
        }
    }
}

impl FromStr for Weight {
    type Err = &'static str;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let weights = EnumClass::with_type(pango::Weight::static_type()).expect("an enum");
        let value = weights
            .value_by_nick(name)
            .map(|value| value.to_value(&weights));
        match value.map(|value| value.get::<pango::Weight>()) {
            Some(Ok(weight)) => Ok(Self(weight)),
            _ => Err("expected a weight such as normal, semibold or bold"),
        }
    }
}

impl FromStr for Format {
    type Err = &'static str;

    fn from_str(format: &str) -> Result<Self, Self::Err> {
        let time = DateTime::from_unix_utc(0).expect("the epoch is a time");
        match time.format(format) {
            Ok(_) => Ok(Self(format.to_owned())),
            Err(_) => Err("GLib cannot use this format"),
        }
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
        locker "hyprlock-wallpaper"
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
                locker: Some(Argv(argv(&["hyprlock-wallpaper"]))),
                timeouts: vec![
                    Timeout {
                        after: Duration::from_secs(300),
                        action: Action::Lock,
                        on_resume: None,
                        ignore_inhibit: false,
                    },
                    Timeout {
                        after: Duration::from_secs(600),
                        action: Action::Spawn(Argv(power("power-off-monitors"))),
                        on_resume: Some(Argv(power("power-on-monitors"))),
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
    fn widgets_take_integers_decimals_and_names() {
        let text = r#"
            lock-screen {
                background { wallpaper-command "cat" "/x"; blur 10; brightness 1; }
                name { weight "bold"; color "accent_color" 0.5; }
                password { anchor "top-left"; offset 10 -2.5; }
            }
        "#;
        let lock_screen = Config::parse("test.kdl", text)
            .unwrap()
            .lock_screen
            .unwrap();
        let background = lock_screen.background.unwrap();
        assert_eq!(
            background.wallpaper_command,
            Some(Argv(argv(&["cat", "/x"])))
        );
        assert_eq!(
            (background.blur, background.brightness),
            (Number(10.0), Number(1.0))
        );
        let name = lock_screen.name.unwrap();
        assert_eq!(
            (name.style.weight, name.style.size),
            (Some(Weight(pango::Weight::Bold)), None)
        );
        let color = Color {
            name: ColorName::Named("accent_color".into()),
            alpha: Number(0.5),
        };
        assert_eq!(name.style.color, Some(color));
        let place = lock_screen.password.place;
        assert_eq!(place.anchor, Some(Anchor::TopLeft));
        assert_eq!(place.offset, Some(Offset(Number(10.0), Number(-2.5))));
        assert_eq!(lock_screen.clock, None);
        let white = ColorName::Literal([1.0; 4]);
        assert_eq!(lock_screen.password.color.name, white);
    }

    #[test]
    fn weights_are_pango_names() {
        assert_eq!("bold".parse(), Ok(Weight(pango::Weight::Bold)));
        assert!("boldest".parse::<Weight>().is_err());
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
    fn bad_lock_screens_are_rejected_where_they_are() {
        for (widget, want) in [
            ("clock", "child node `password` is required"),
            ("password; clok", "unexpected node `clok`"),
            ("password; clock; clock", "duplicate node `clock`"),
            (
                "password { anchor \"middle\"; }",
                "expected `top-left`, `top`, or one of 7 others",
            ),
            ("password { size 3; }", "unexpected node `size`"),
            (
                "password; name { format \"%H\"; }",
                "unexpected node `format`",
            ),
            ("password; clock { size 0; }", "expected 1 to 1000"),
            ("password; clock { size 1001; }", "expected 1 to 1000"),
            ("password { width \"x\"; }", "expected decimal"),
            ("password { dot-size 0.5; }", "expected 1 to 100"),
            (
                "password; date { letter-spacing 101; }",
                "expected -100 to 100",
            ),
            ("password; date { weight 600; }", "expected string"),
            (
                "password; date { weight \"heavyish\"; }",
                "expected a weight such as normal",
            ),
            (
                "password; date { color \"#fff\"; }",
                "expected #rrggbb, #rrggbbaa or a GTK colour name",
            ),
            (
                "password; date { color \"accent color\"; }",
                "expected #rrggbb",
            ),
            (
                "password; date { color \"#ffffff\" 1.5; }",
                "expected 0 to 1",
            ),
            (
                "password; date { format \"%Q\"; }",
                "GLib cannot use this format",
            ),
            (
                "password; avatar { offset 0 20000; }",
                "expected -10000 to 10000",
            ),
            (
                "password; background { wallpaper-command; }",
                "additional argument `program` is required",
            ),
            (
                "password; background { brightness -1; }",
                "expected 0 to 10",
            ),
            (
                "password; background { gradient 0.5; }",
                "additional argument",
            ),
        ] {
            let text = format!("lock-screen {{\n    {widget}\n}}\n");
            let err = parse_err(&text);
            assert!(err.contains(want), "{text:?}: {err}");
            // Each error points into the config.
            assert!(err.contains("at line "), "{text:?}: {err}");
        }
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
