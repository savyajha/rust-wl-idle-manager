use std::str::FromStr;

use knuffel::ast::{Literal, SpannedNode, TypeName};
use knuffel::decode::{Context as DecodeContext, Kind};
use knuffel::errors::DecodeError;
use knuffel::span::Spanned;
use knuffel::traits::ErrorSpan;
use pango::glib::prelude::StaticType;
use pango::glib::{DateTime, EnumClass};

use crate::theme::{self, Rgba};

/// The lock screen without a `lock-screen` block: the date and a large clock at the top, the
/// avatar, name and password field at the bottom, laid out for a 1280×720 output.
const DEFAULT_LOCK_SCREEN: &str = r##"
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

/// A document of one `lock-screen` node.
#[derive(knuffel::Decode)]
struct Document {
    #[knuffel(child)]
    lock_screen: LockScreen,
}

impl LockScreen {
    /// Parse and validate KDL `text` of one `lock-screen` node; `name` labels it in errors.
    // Inline (as is `default_layout`), so that knuffel's parser is built in the caller's crate
    // with its own config's: one in each crate made the daemon ~145 KB larger. So nothing in
    // this crate outside tests may call either.
    #[inline]
    pub fn parse(name: &str, text: &str) -> anyhow::Result<Self> {
        let Document { lock_screen } = knuffel::parse(name, text).map_err(render)?;
        Ok(lock_screen)
    }

    /// `DEFAULT_LOCK_SCREEN`'s.
    #[inline]
    pub fn default_layout() -> Self {
        Self::parse("DEFAULT_LOCK_SCREEN", DEFAULT_LOCK_SCREEN)
            .expect("the default lock screen parses")
    }

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
pub fn render(err: knuffel::Error) -> anyhow::Error {
    let mut text = String::new();
    let _ = miette::NarratableReportHandler::new().render_report(&mut text, &err);
    anyhow::anyhow!("{}", text.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn parse_err(text: &str) -> String {
        format!("{:#}", LockScreen::parse("test.kdl", text).unwrap_err())
    }

    #[test]
    fn weights_are_pango_names() {
        assert_eq!("bold".parse(), Ok(Weight(pango::Weight::Bold)));
        assert!("boldest".parse::<Weight>().is_err());
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
        let lock_screen = LockScreen::parse("test.kdl", text).unwrap();
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
}
