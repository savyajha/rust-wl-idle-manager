//! The login screen: what it shows, the greeter's widgets (drawn over the lock screen's, or
//! instead of its user, the avatar row), the indicator row, the focus ring, where they are
//! for the pointer, and the popups' lists.

use cairo::{Context, Operator};
use pango::Weight;
use tokio::time::Instant;
use tracing::error;
use ui::draw::{self, Avatar, Font, Label, Layout, Painter, Rect, Scene};
use ui::entry::{Entry, Look, Status};
use ui::keyboard::Keyboard;
use ui::theme::Rgba;

use crate::battery::Battery;
use crate::chooser::{Chooser, Screen};
use crate::focus::Widget;
use crate::icons;
use crate::layouts::Layout as KeyboardLayout;
use crate::power::Action;
use crate::sessions::Session;

// In logical pixels. The message line and the session chip sit under the field (under the
// caps lock line).
const CHIP_HEIGHT: f64 = 24.0;
const MESSAGE_BELOW: f64 = 32.0;
const SESSION_BELOW: f64 = 58.0;
/// Between a chip's text and its ends, and a popup row's text and the popup's edges.
const PADDING: f64 = 12.0;
// The session popup: its rows, the padding around them, and its gap to the chip.
const ROW_HEIGHT: f64 = 28.0;
const POPUP_PADDING: f64 = 4.0;
pub const POPUP_GAP: i32 = 4;
const POPUP_MIN_WIDTH: f64 = 160.0;
const POPUP_RADIUS: f64 = 10.0;
/// Dark and nearly opaque, since the popup cannot blur what is under it.
const POPUP_FILL: Rgba = [0.12, 0.13, 0.15, 0.92];
// The avatar row: each avatar's column, wider for a longer name, and its name under it.
const COLUMN: f64 = 120.0;
const COLUMN_GAP: f64 = 16.0;
const NAME_BELOW: f64 = 8.0;
// The focus ring: its width, its gap to the widget, and its colour, which stands out from
// the widgets' outlines.
const RING_WIDTH: f64 = 2.0;
const RING_GAP: f64 = 2.0;
const RING: Rgba = [1.0, 1.0, 1.0, 0.7];
// Hover: white over the widget under the pointer, which raises its frosted fill's alpha by
// about HOVER; a power button, which has no fill, gets a plate of PLATE behind its icon and
// label, its corners PLATE_RADIUS round.
const HOVER: f64 = 0.08;
const PLATE: f64 = 0.1;
const PLATE_RADIUS: f64 = 12.0;

// The indicator row at the top right: 24 high, 8 from the top and 12 from the right edge,
// its items 8 apart (the layout, then the battery, rightmost). With a popup the layout is a
// chip like the session chip, else its text alone. DESIGN.md has this as the spec for the
// desktop bar's right end.
const INDICATOR_TOP: f64 = 8.0;
const INDICATOR_RIGHT: f64 = 12.0;
const INDICATOR_HEIGHT: f64 = 24.0;
const INDICATOR_GAP: f64 = 8.0;
/// The battery: a body 21 by 12, outlined 1 wide, with a nub 2 by 6.
const BATTERY_WIDTH: f64 = 23.0;

// The power row, centred at the bottom: buttons 72 by 56 (the icon 24 high, 4 from the
// top; the label 34 from it), 16 apart, 24 from the bottom.
const POWER_WIDTH: f64 = 72.0;
const POWER_HEIGHT: f64 = 56.0;
const POWER_GAP: f64 = 16.0;
const POWER_BOTTOM: f64 = 24.0;
// The dialog, centred, over the screen dimmed: 320 by 132, its question 28 from the top,
// its buttons 120 by 32, 16 apart, 20 from the bottom.
const DIALOG_WIDTH: f64 = 320.0;
const DIALOG_HEIGHT: f64 = 132.0;
const DIALOG_BUTTON_WIDTH: f64 = 120.0;
const DIALOG_BUTTON_HEIGHT: f64 = 32.0;
const DIM: Rgba = [0.0, 0.0, 0.0, 0.45];

/// The field's hint for "Other…"'s user name.
const USER_NAME: &str = "User name";

const WHITE: Rgba = [1.0; 4];
const OUTLINE: Rgba = [1.0, 1.0, 1.0, draw::FIELD_OUTLINE];

/// What the login screen shows and is typed into.
pub struct Login {
    pub entry: Entry,
    pub keyboard: Keyboard,
    pub prompt: Prompt,
    /// The line under the field: PAM's message, or what went wrong.
    pub message: Option<String>,
    pub sessions: Vec<Session>,
    /// The session to start, in `sessions`.
    pub chosen: usize,
    pub chooser: Chooser,
    /// The layouts to choose from; none to type with the compositor's keymap.
    pub layouts: Vec<KeyboardLayout>,
    /// The one typed with, in `layouts`.
    pub layout: usize,
    pub battery: Option<Battery>,
}

/// greetd's prompt.
#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub secret: bool,
    pub text: String,
}

/// Everything the login screen shows; it is drawn again when this changes.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub look: Look,
    /// greetd's prompt without its colon, for the empty field.
    pub hint: String,
    /// The answer to a visible prompt, shown as it is typed.
    pub text: Option<String>,
    message: Option<String>,
    /// The session chip's text, during a login with more than one session.
    session: Option<String>,
    /// The avatar row's names, when it is shown instead of a user.
    pub row: Option<Vec<String>>,
    /// Whether Cancel is shown, in the bottom row: there is somewhere to go back to.
    back: bool,
    ring: Option<Widget>,
    /// The widget under the pointer, which the toplevel sets (`Extras::hover`).
    pub hover: Option<Widget>,
    indicators: Indicators,
    power: Vec<Action>,
    /// The dialog's action, while it is open.
    dialog: Option<Action>,
}

/// What the indicator row shows.
#[derive(Clone, Debug, Default, PartialEq)]
struct Indicators {
    /// The layout's short name, if layouts are configured.
    layout: Option<String>,
    /// Whether the layout opens a popup: there is more than one.
    picker: bool,
    battery: Option<Battery>,
}

impl Login {
    /// Type with layout `i` from now on; on a failure (`--check` compiled them all) the
    /// old one stays.
    pub fn set_layout(&mut self, i: usize) {
        match self
            .keyboard
            .set_layout(&self.layouts[i].name, &mut self.entry)
        {
            Ok(()) => self.layout = i,
            Err(e) => error!("{e:#}"),
        }
    }

    /// The machine woke from sleep. What was typed goes, unless it was submitted (an
    /// answer greetd is to have): someone else may be at the machine now, and the timer
    /// that forgets a password stood still in sleep, as on the lock screen. The keys held
    /// are forgotten, their releases may never come; and the focus leaves the power row.
    pub fn resumed(&mut self, now: Instant) {
        if self.entry.look(now).status != Status::Checking {
            self.entry.wipe();
        }
        self.keyboard.forget_held(&mut self.entry);
        self.chooser.resumed();
    }

    /// The widgets shown (`Chooser::shown`): the field's arrow while something is typed
    /// and not being checked, as `Extras` draws it.
    pub fn shown(&self, now: Instant) -> Vec<Widget> {
        self.chooser
            .shown(self.entry.look(now).status == Status::Typing)
    }

    pub fn view(&self, now: Instant) -> View {
        let screen = &self.chooser.screen;
        let (secret, prompt) = match screen {
            Screen::UserName => (false, USER_NAME),
            _ => (self.prompt.secret, self.prompt.text.as_str()),
        };
        let hint = prompt.trim().trim_end_matches(':').trim_end();
        let hint = if hint.is_empty() { draw::HINT } else { hint };
        let session = matches!(screen, Screen::Login(_)) && self.sessions.len() > 1;
        View {
            look: self.entry.look(now),
            hint: hint.to_owned(),
            text: (!secret).then(|| String::from_utf8_lossy(self.entry.typed()).into_owned()),
            message: self.message.clone(),
            session: session.then(|| self.sessions[self.chosen].name.clone()),
            row: (*screen == Screen::Row).then(|| self.chooser.names()),
            back: self.chooser.can_go_back(),
            ring: self.chooser.focus.ring(),
            hover: None,
            indicators: Indicators {
                layout: self
                    .layouts
                    .get(self.layout)
                    .map(|layout| layout.short.clone()),
                picker: self.layouts.len() > 1,
                battery: self.battery,
            },
            power: self.chooser.power.clone(),
            dialog: self.chooser.dialog,
        }
    }
}

/// A frosted button or chip with its text, in physical pixels.
struct Button {
    text: String,
    rect: Rect,
    label: Label,
    alpha: f64,
}

/// A line of text.
struct Line {
    text: String,
    label: Label,
}

/// What a button of the bottom row is: Cancel, back from the login, or a power button.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Bottom {
    Cancel,
    Power(Action),
}

/// A button of the bottom row: its box and its label (the icon is drawn each time).
struct BottomButton {
    kind: Bottom,
    rect: Rect,
    label: Label,
}

/// The confirmation dialog.
struct Dialog {
    action: Action,
    panel: Rect,
    question: Label,
    cancel: Button,
    confirm: Button,
}

/// The avatar row, with each avatar's name under it.
struct Row {
    names: Vec<String>,
    avatars: Vec<Avatar>,
    labels: Vec<Label>,
}

/// The greeter's widgets around the lock screen's, for one scene.
pub struct Extras {
    /// The field, from the scene.
    pill: Rect,
    /// The scene's avatar, where the avatar row goes.
    avatar: Rect,
    /// Around the field's arrow button, which logs in.
    arrow: Rect,
    /// Whether the arrow shows: something is typed, and not being checked.
    arrow_shown: bool,
    factor: f64,
    session: Option<Button>,
    message: Option<Line>,
    /// While it is shown, instead of a user.
    row: Option<Row>,
    ring: Option<Widget>,
    hover: Option<Widget>,
    /// The frame's width.
    width: f64,
    indicators: Indicators,
    /// The layout's chip, or with no popup its text alone.
    layout_chip: Option<Button>,
    /// Where the battery is.
    battery: Option<Rect>,
    /// The frame's height.
    height: f64,
    /// The bottom row: Cancel first, if shown, then the power buttons.
    bottom: Vec<BottomButton>,
    dialog: Option<Dialog>,
}

impl Extras {
    /// The widgets around `scene`'s field; `None` for a plain scene, which has none.
    pub fn new(scene: &Scene) -> Option<Self> {
        let (Some(pill), Some(arrow)) = (scene.pill(), scene.button()) else {
            return None;
        };
        let f = scene.layout.factor();
        // Without an avatar in the look, the row goes where the field is.
        let avatar = scene.avatar().unwrap_or_else(|| {
            let (x, y) = pill.center();
            let d = (64.0 * f).round();
            whole(Rect {
                x: x - d / 2.0,
                y: y - d / 2.0,
                w: d,
                h: d,
            })
        });
        Some(Self {
            pill,
            avatar,
            arrow,
            arrow_shown: false,
            factor: f,
            session: None,
            message: None,
            row: None,
            ring: None,
            hover: None,
            width: f64::from(scene.layout.pixels.0),
            indicators: Indicators::default(),
            layout_chip: None,
            battery: None,
            height: f64::from(scene.layout.pixels.1),
            bottom: Vec::new(),
            dialog: None,
        })
    }

    /// Render the message line and the session chip again where `view` changed them.
    pub fn update(&mut self, painter: &Painter, view: &View) -> Result<(), cairo::Error> {
        let f = self.factor;
        let (center, bottom) = (self.pill.center().0, self.pill.y + self.pill.h);
        let render = |text: &str, weight| {
            let font = Font::new(draw::HINT_SIZE, weight, WHITE);
            painter.label(text, &font, f, |_| (0.0, 0.0))
        };
        if self.message.as_ref().map(|m| &m.text) != view.message.as_ref() {
            self.message = None;
            if let Some(text) = &view.message {
                let mut label = render(text, Weight::Normal)?;
                let top = bottom + MESSAGE_BELOW * f;
                label.at = draw::centered((label.size().0, 0.0), (center, top));
                self.message = Some(Line {
                    text: text.clone(),
                    label,
                });
            }
        }
        if self.session.as_ref().map(|s| &s.text) != view.session.as_ref() {
            self.session = None;
            if let Some(text) = &view.session {
                let w = render(text, Weight::Medium)?.size().0 + 2.0 * PADDING * f;
                let rect = whole(Rect {
                    x: center - w / 2.0,
                    y: bottom + SESSION_BELOW * f,
                    w,
                    h: CHIP_HEIGHT * f,
                });
                self.session = Some(button(painter, text, rect, draw::FIELD, f)?);
            }
        }
        if self.row.as_ref().map(|row| &row.names) != view.row.as_ref() {
            self.row = None;
            if let Some(names) = &view.row {
                self.row = Some(self.render_row(painter, names)?);
            }
        }
        if self.indicators != view.indicators {
            self.render_indicators(painter, &view.indicators)?;
        }
        let back = view.back.then_some(Bottom::Cancel);
        let kinds: Vec<_> = back
            .into_iter()
            .chain(view.power.iter().map(|&a| Bottom::Power(a)))
            .collect();
        if !self
            .bottom
            .iter()
            .map(|button| button.kind)
            .eq(kinds.iter().copied())
        {
            self.render_bottom(painter, &kinds)?;
        }
        if self.dialog.as_ref().map(|dialog| dialog.action) != view.dialog {
            self.dialog = None;
            if let Some(action) = view.dialog {
                self.dialog = Some(self.render_dialog(painter, action)?);
            }
        }
        self.arrow_shown = view.look.status == Status::Typing;
        self.ring = view.ring;
        Ok(())
    }

    /// Place the indicators, right to left from the top right corner.
    fn render_indicators(
        &mut self,
        painter: &Painter,
        indicators: &Indicators,
    ) -> Result<(), cairo::Error> {
        let f = self.factor;
        let mut right = self.width - INDICATOR_RIGHT * f;
        // The next item, `w` wide, left of the last.
        let mut place = |w: f64| {
            let y = INDICATOR_TOP * f;
            let rect = whole(Rect {
                x: right - w,
                y,
                w,
                h: INDICATOR_HEIGHT * f,
            });
            right = rect.x - INDICATOR_GAP * f;
            rect
        };
        self.battery = indicators.battery.map(|_| place(BATTERY_WIDTH * f));
        self.layout_chip = None;
        if let Some(text) = &indicators.layout {
            let font = Font::new(draw::HINT_SIZE, Weight::Medium, WHITE);
            let w = painter.label(text, &font, f, |_| (0.0, 0.0))?.size().0 + 2.0 * PADDING * f;
            self.layout_chip = Some(button(painter, text, place(w), draw::FIELD, f)?);
        }
        self.indicators = indicators.clone();
        Ok(())
    }

    /// The bottom row of `kinds`, centred at the bottom.
    fn render_bottom(&mut self, painter: &Painter, kinds: &[Bottom]) -> Result<(), cairo::Error> {
        let f = self.factor;
        let n = kinds.len() as f64;
        let left = self.width / 2.0 - (n * POWER_WIDTH + (n - 1.0) * POWER_GAP) * f / 2.0;
        let y = self.height - (POWER_BOTTOM + POWER_HEIGHT) * f;
        let font = Font::new(draw::HINT_SIZE, Weight::Medium, WHITE);
        self.bottom.clear();
        for (i, &kind) in kinds.iter().enumerate() {
            let x = left + i as f64 * (POWER_WIDTH + POWER_GAP) * f;
            let rect = whole(Rect {
                x,
                y,
                w: POWER_WIDTH * f,
                h: POWER_HEIGHT * f,
            });
            let text = match kind {
                Bottom::Cancel => "Cancel",
                Bottom::Power(action) => action.label(),
            };
            let label = painter.label(text, &font, f, |(w, _)| {
                ((rect.center().0 - w / 2.0).round(), rect.y + 34.0 * f)
            })?;
            self.bottom.push(BottomButton { kind, rect, label });
        }
        Ok(())
    }

    /// The dialog asking to confirm `action`, centred.
    fn render_dialog(&self, painter: &Painter, action: Action) -> Result<Dialog, cairo::Error> {
        let f = self.factor;
        let panel = whole(Rect {
            x: (self.width - DIALOG_WIDTH * f) / 2.0,
            y: (self.height - DIALOG_HEIGHT * f) / 2.0,
            w: DIALOG_WIDTH * f,
            h: DIALOG_HEIGHT * f,
        });
        let font = Font::new(15.0, Weight::Semibold, WHITE);
        // Only those that ask first open it.
        let text = action.question().unwrap_or_default();
        let question = painter.label(text, &font, f, |(w, _)| {
            ((panel.center().0 - w / 2.0).round(), panel.y + 28.0 * f)
        })?;
        let (w, h) = (DIALOG_BUTTON_WIDTH * f, DIALOG_BUTTON_HEIGHT * f);
        let y = panel.y + panel.h - 20.0 * f - h;
        let beside = |x| whole(Rect { x, y, w, h });
        let middle = panel.center().0;
        let cancel = beside(middle - 8.0 * f - w);
        Ok(Dialog {
            action,
            panel,
            question,
            cancel: button(painter, "Cancel", cancel, draw::FIELD, f)?,
            confirm: button(
                painter,
                action.label(),
                beside(middle + 8.0 * f),
                draw::BUTTON,
                f,
            )?,
        })
    }

    /// The widget under the pointer at `at`, in physical pixels (None: not on this
    /// surface), now shown hovered: as `hit` has it for what `update` last showed, so none
    /// that has gone (such as the power row's under the dialog).
    pub fn hover(&mut self, at: Option<(f64, f64)>) -> Option<Widget> {
        self.hover = at.and_then(|(x, y)| self.hit(x, y));
        self.hover
    }

    /// Whether `(x, y)`, in physical pixels, is in the dialog.
    pub fn in_dialog(&self, x: f64, y: f64) -> bool {
        self.dialog
            .as_ref()
            .is_some_and(|dialog| inside(dialog.panel, x, y))
    }

    /// The avatars of `names` in a row centred where the scene's avatar is, with their
    /// names under them, in columns wide enough for the longest.
    fn render_row(&self, painter: &Painter, names: &[String]) -> Result<Row, cairo::Error> {
        let f = self.factor;
        let font = Font::new(draw::HINT_SIZE, Weight::Medium, WHITE);
        let mut labels = Vec::new();
        for name in names {
            labels.push(painter.label(name, &font, f, |_| (0.0, 0.0))?);
        }
        let widest = labels
            .iter()
            .map(|label| label.size().0)
            .fold(0.0, f64::max);
        let column = (COLUMN * f).max(widest + COLUMN_GAP * f);
        let middle = (names.len() as f64 - 1.0) / 2.0;
        let mut avatars = Vec::new();
        for (i, (name, label)) in names.iter().zip(&mut labels).enumerate() {
            let x = (i as f64 - middle) * column;
            let square = whole(Rect {
                x: self.avatar.x + x,
                ..self.avatar
            });
            avatars.push(painter.avatar_in(name, square, f)?);
            let top = square.y + square.h + NAME_BELOW * f;
            label.at = draw::centered((label.size().0, 0.0), (square.center().0, top));
        }
        Ok(Row {
            names: names.to_vec(),
            avatars,
            labels,
        })
    }

    pub fn paint(&self, cr: &Context) -> Result<(), cairo::Error> {
        let f = self.factor;
        if let Some(row) = &self.row {
            for (avatar, label) in row.avatars.iter().zip(&row.labels) {
                avatar.paint(cr, f)?;
                label.paint(cr, 1.0)?;
            }
        } else {
            if let Some(button) = &self.session {
                draw::rounded(cr, button.rect, button.rect.h / 2.0);
                draw::frost(cr, button.alpha, OUTLINE, f)?;
                button.label.paint(cr, 1.0)?;
            }
            if let Some(message) = &self.message {
                message.label.paint(cr, draw::HINT_ALPHA)?;
            }
        }
        if let Some(chip) = &self.layout_chip {
            if self.indicators.picker {
                draw::rounded(cr, chip.rect, chip.rect.h / 2.0);
                draw::frost(cr, chip.alpha, OUTLINE, f)?;
            }
            chip.label.paint(cr, 1.0)?;
        }
        if let (Some(rect), Some(battery)) = (self.battery, self.indicators.battery) {
            paint_battery(cr, rect, battery, f)?;
        }
        for button in &self.bottom {
            let icon = (button.rect.center().0, button.rect.y + 16.0 * f);
            match button.kind {
                Bottom::Cancel => icons::cancel(cr, icon, f)?,
                Bottom::Power(action) => icons::power(cr, action, icon, f)?,
            }
            button.label.paint(cr, 1.0)?;
        }
        if let Some(dialog) = &self.dialog {
            draw::set_color(cr, DIM);
            cr.paint()?;
            draw::rounded(cr, dialog.panel, POPUP_RADIUS * f);
            draw::set_color(cr, POPUP_FILL);
            cr.fill_preserve()?;
            draw::frost(cr, 0.0, OUTLINE, f)?;
            dialog.question.paint(cr, 1.0)?;
            for button in [&dialog.cancel, &dialog.confirm] {
                draw::rounded(cr, button.rect, button.rect.h / 2.0);
                draw::frost(cr, button.alpha, OUTLINE, f)?;
                button.label.paint(cr, 1.0)?;
            }
        }
        if let Some(widget) = self.hover
            && let Some(rect) = self.place(widget)
        {
            // A chip's or a button's shape, or an avatar's circle; a power button's plate.
            let (radius, alpha) = match widget {
                Widget::Back | Widget::Power(_) => (PLATE_RADIUS * f, PLATE),
                _ => (rect.h / 2.0, HOVER),
            };
            draw::rounded(cr, rect, radius);
            draw::set_color(cr, [1.0, 1.0, 1.0, alpha]);
            cr.fill()?;
        }
        // Round every widget's shape, a field's or a chip's or an avatar's.
        if let Some(rect) = self.ring.and_then(|widget| self.place(widget)) {
            let out = (RING_GAP + RING_WIDTH / 2.0) * f;
            let ring = Rect {
                x: rect.x - out,
                y: rect.y - out,
                w: rect.w + 2.0 * out,
                h: rect.h + 2.0 * out,
            };
            draw::rounded(cr, ring, ring.h / 2.0);
            draw::set_color(cr, RING);
            cr.set_line_width(RING_WIDTH * f);
            cr.stroke()?;
        }
        Ok(())
    }

    /// Where `widget` is, in physical pixels, if it is shown.
    pub fn place(&self, widget: Widget) -> Option<Rect> {
        // The dialog's buttons alone while it is open.
        if let Some(dialog) = &self.dialog {
            return match widget {
                Widget::Cancel => Some(dialog.cancel.rect),
                Widget::Confirm => Some(dialog.confirm.rect),
                _ => None,
            };
        }
        let back = self.bottom.first().filter(|b| b.kind == Bottom::Cancel);
        match (widget, &self.row) {
            (Widget::Back, _) => back.map(|button| button.rect),
            (Widget::Power(i), _) => {
                let i = i + usize::from(back.is_some());
                self.bottom.get(i).map(|button| button.rect)
            }
            (Widget::Cancel | Widget::Confirm, _) => None,
            (Widget::Layout, _) => {
                let chip = self.layout_chip.as_ref().filter(|_| self.indicators.picker);
                chip.map(|chip| chip.rect)
            }
            (Widget::Avatar(i), Some(row)) => row.avatars.get(i).map(Avatar::square),
            (_, Some(_)) | (Widget::Avatar(_), None) => None,
            (Widget::Pill, None) => Some(self.pill),
            (Widget::Session, None) => self.session.as_ref().map(|session| session.rect),
            (Widget::LogIn, None) => self.arrow_shown.then_some(self.arrow),
        }
    }

    /// The widget a click at `(x, y)`, in physical pixels, is on; never the field.
    pub fn hit(&self, x: f64, y: f64) -> Option<Widget> {
        let avatars = self.row.as_ref().map_or(0, |row| row.avatars.len());
        let buttons = [
            Widget::LogIn,
            Widget::Back,
            Widget::Session,
            Widget::Layout,
            Widget::Cancel,
            Widget::Confirm,
        ];
        let power = (0..self.bottom.len()).map(Widget::Power);
        let mut widgets = (0..avatars).map(Widget::Avatar).chain(buttons).chain(power);
        widgets.find(|&widget| self.place(widget).is_some_and(|rect| inside(rect, x, y)))
    }
}

/// Draw `battery` in `rect`, a `BATTERY_WIDTH` wide box, at `f` physical pixels per logical
/// one: an outline with a nub, filled as far as its level, and a bolt on mains.
fn paint_battery(cr: &Context, rect: Rect, battery: Battery, f: f64) -> Result<(), cairo::Error> {
    // Whole pixels at scale 1 (fills on them, strokes centred half a pixel in).
    let (x, top) = (rect.x, rect.center().1.round() - 6.0 * f);
    draw::set_color(cr, WHITE);
    let outline = Rect {
        x: x + 0.5 * f,
        y: top + 0.5 * f,
        w: 20.0 * f,
        h: 11.0 * f,
    };
    draw::rounded(cr, outline, 2.0 * f);
    cr.set_line_width(f);
    cr.stroke()?;
    cr.rectangle(x + 21.0 * f, top + 3.0 * f, 2.0 * f, 6.0 * f);
    let level = (17.0 * f * f64::from(battery.percent) / 100.0).round();
    cr.rectangle(x + 2.0 * f, top + 2.0 * f, level, 8.0 * f);
    cr.fill()?;
    if battery.charging {
        // A bolt across the body, outlined dark to show over the level and past it.
        let (center, middle) = (x + 10.5 * f, top + 6.0 * f);
        let points = [
            (1.5, -6.0),
            (-3.0, 1.0),
            (-0.3, 1.0),
            (-1.5, 6.0),
            (3.0, -1.0),
            (0.3, -1.0),
        ];
        for (dx, dy) in points {
            cr.line_to(center + dx * f, middle + dy * f);
        }
        cr.close_path();
        cr.fill_preserve()?;
        draw::set_color(cr, POPUP_FILL);
        cr.stroke()?;
    }
    Ok(())
}

/// `text` centred in a frosted `rect` filled with white at `alpha`.
fn button(
    painter: &Painter,
    text: &str,
    rect: Rect,
    alpha: f64,
    factor: f64,
) -> Result<Button, cairo::Error> {
    let font = Font::new(draw::HINT_SIZE, Weight::Medium, WHITE);
    let label = painter.label(text, &font, factor, |size| {
        draw::centered(size, rect.center())
    })?;
    Ok(Button {
        text: text.to_owned(),
        rect,
        label,
        alpha,
    })
}

fn inside(rect: Rect, x: f64, y: f64) -> bool {
    (rect.x..rect.x + rect.w).contains(&x) && (rect.y..rect.y + rect.h).contains(&y)
}

/// `rect` with its corners on whole pixels.
fn whole(rect: Rect) -> Rect {
    let (x, y) = (rect.x.round(), rect.y.round());
    Rect {
        x,
        y,
        w: (rect.x + rect.w).round() - x,
        h: (rect.y + rect.h).round() - y,
    }
}

/// A popup's rows, rendered at its parent's scale.
pub struct PopupList {
    /// The popup's size, in logical pixels.
    pub size: (i32, i32),
    factor: f64,
    rows: Vec<Label>,
}

impl PopupList {
    pub fn new(painter: &Painter, names: &[&str], factor: f64) -> Result<Self, cairo::Error> {
        let font = Font::new(draw::HINT_SIZE, Weight::Normal, WHITE);
        let mut rows = Vec::new();
        let mut width = POPUP_MIN_WIDTH;
        for (i, name) in names.iter().enumerate() {
            let top = (POPUP_PADDING + ROW_HEIGHT * i as f64) * factor;
            let label = painter.label(name, &font, factor, |(_, h)| {
                let x = (POPUP_PADDING + PADDING) * factor;
                (x.round(), (top + (ROW_HEIGHT * factor - h) / 2.0).round())
            })?;
            width = width.max(label.size().0 / factor + 2.0 * (POPUP_PADDING + PADDING));
            rows.push(label);
        }
        let height = ROW_HEIGHT * names.len() as f64 + 2.0 * POPUP_PADDING;
        Ok(Self {
            size: (width.ceil() as i32, height.ceil() as i32),
            factor,
            rows,
        })
    }

    /// Draw the list, with row `selected` highlighted, into `canvas`, an `Argb8888` frame
    /// of `layout`.
    pub fn draw(
        &self,
        canvas: &mut [u8],
        layout: Layout,
        selected: usize,
    ) -> Result<(), cairo::Error> {
        let f = self.factor;
        let (width, height) = layout.pixels;
        draw::draw_on(canvas, layout.pixels, cairo::Format::ARgb32, |cr| {
            cr.set_operator(Operator::Clear);
            cr.paint()?;
            cr.set_operator(Operator::Over);
            let all = Rect {
                x: 0.0,
                y: 0.0,
                w: f64::from(width),
                h: f64::from(height),
            };
            draw::rounded(cr, all, POPUP_RADIUS * f);
            draw::set_color(cr, POPUP_FILL);
            cr.fill_preserve()?;
            draw::frost(cr, 0.0, OUTLINE, f)?;
            let row = whole(Rect {
                x: POPUP_PADDING * f,
                y: (POPUP_PADDING + ROW_HEIGHT * selected as f64) * f,
                w: f64::from(width) - 2.0 * POPUP_PADDING * f,
                h: ROW_HEIGHT * f,
            });
            draw::rounded(cr, row, (POPUP_RADIUS - POPUP_PADDING) * f);
            draw::frost(cr, draw::FIELD, [0.0; 4], f)?;
            for label in &self.rows {
                label.paint(cr, 1.0)?;
            }
            Ok(())
        })
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// The row at logical `y`.
    pub fn row_at(&self, y: f64) -> Option<usize> {
        row_at(y, self.rows.len())
    }
}

/// Which of `rows` rows is at logical `y` in the popup.
fn row_at(y: f64, rows: usize) -> Option<usize> {
    let row = ((y - POPUP_PADDING) / ROW_HEIGHT).floor();
    (row >= 0.0 && (row as usize) < rows).then_some(row as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::User;

    fn session(name: &str) -> Session {
        Session {
            file: format!("{name}.desktop"),
            name: name.into(),
            exec: vec![name.into()],
            desktop_names: Vec::new(),
        }
    }

    fn login(users: &[&str], sessions: &[&str], secret: bool, prompt: &str) -> Login {
        let users = users.iter().map(|name| User {
            name: name.to_lowercase(),
            full_name: name.to_string(),
        });
        Login {
            entry: Entry::new(),
            keyboard: Keyboard::default(),
            prompt: Prompt {
                secret,
                text: prompt.into(),
            },
            message: None,
            sessions: sessions.iter().map(|name| session(name)).collect(),
            chosen: sessions.len().saturating_sub(1),
            chooser: Chooser::new(users.collect(), None, sessions.len() > 1, false),
            layouts: Vec::new(),
            layout: 0,
            battery: None,
        }
    }

    #[test]
    fn the_view_has_the_prompt_the_visible_answer_and_the_session() {
        let now = Instant::now();
        let view = login(&["Alice"], &["A", "B"], true, "Password: ").view(now);
        assert_eq!(view.hint, "Password");
        assert_eq!((view.text, view.session.as_deref()), (None, Some("B")));
        // One user: their login, with Cancel for the row of them and "Other…".
        assert_eq!((view.row, view.back), (None, true));
        // No one listed: the user name, with no row to go back to.
        let view = login(&[], &["A"], true, "").view(now);
        assert_eq!((&*view.hint, view.back), ("User name", false));
        let view = login(&["Alice"], &["A"], false, "").view(now);
        assert_eq!(view.hint, draw::HINT);
        // Typed text shows for a visible prompt only; one session needs no chip.
        assert_eq!((view.text.as_deref(), view.session), (Some(""), None));
    }

    #[test]
    fn the_view_has_the_row_then_a_visible_field_for_other() {
        let now = Instant::now();
        let mut login = login(&["Alice", "Bob"], &["A", "B"], true, "Password: ");
        let view = login.view(now);
        let names = ["Alice", "Bob", "Other…"].map(String::from);
        assert_eq!(view.row.as_deref(), Some(&names[..]));
        // Nowhere to go back to from the row.
        assert_eq!((view.session, view.back), (None, false));
        login.chooser.choose(2);
        let view = login.view(now);
        // Asked for visibly, before any user is known: no session chip yet.
        assert_eq!((&*view.hint, view.text.as_deref()), ("User name", Some("")));
        assert_eq!((view.row, view.session, view.back), (None, None, true));
    }

    #[test]
    fn rows_are_found_by_height() {
        assert_eq!(row_at(10.0, 0), None);
        // Under the padding, 28 high each.
        assert_eq!(row_at(3.0, 2), None);
        assert_eq!(row_at(4.0, 2), Some(0));
        assert_eq!(row_at(31.9, 2), Some(0));
        assert_eq!(row_at(32.0, 2), Some(1));
        assert_eq!(row_at(60.0, 2), None);
    }

    #[test]
    fn the_indicators_show_the_layout_and_the_battery() {
        let now = Instant::now();
        let mut login = login(&["Alice"], &["A"], true, "");
        assert_eq!(login.view(now).indicators, Indicators::default());
        let names = ["de".to_owned(), "us(dvorak)".to_owned()];
        login.layouts = crate::layouts::new(&names);
        login.layout = 1;
        let battery = Battery {
            percent: 5,
            charging: false,
        };
        login.battery = Some(battery);
        let indicators = login.view(now).indicators;
        assert_eq!(indicators.layout.as_deref(), Some("US"));
        assert_eq!(
            (indicators.picker, indicators.battery),
            (true, Some(battery))
        );
        // One layout: its name, but no popup.
        login.layouts.pop();
        login.layout = 0;
        let indicators = login.view(now).indicators;
        assert_eq!(
            (indicators.layout.as_deref(), indicators.picker),
            (Some("DE"), false)
        );
    }

    /// The hovered widget is the one under the pointer of those shown, and none off them,
    /// off the surface, or once the dialog hides the power row.
    #[test]
    fn hover_follows_the_pointer_and_the_screen() {
        let config = ui::config::LockScreen::default_layout();
        let palette = ui::theme::Palette::parse("");
        let painter = Painter::given(config, palette, "Adwaita Sans", None, "Ann".into());
        let scene = painter.scene(Layout::new((1280, 720), 120, None)).unwrap();
        let mut extras = Extras::new(&scene).unwrap();
        let mut login = login(&["Alice"], &["A", "B"], true, "Password: ");
        login.chooser.power = vec![Action::Sleep, Action::Restart];
        let now = Instant::now();
        extras.update(&painter, &login.view(now)).unwrap();
        // Nothing typed: no arrow.
        let arrow = scene.button().unwrap().center();
        assert_eq!(extras.hover(Some(arrow)), None);
        login.entry.press(ui::entry::Key::Char('a'), 1, now);
        extras.update(&painter, &login.view(now)).unwrap();
        let log_in = extras.place(Widget::LogIn).unwrap().center();
        let restart = extras.place(Widget::Power(1)).unwrap().center();
        assert_eq!(extras.hover(Some(log_in)), Some(Widget::LogIn));
        assert_eq!(extras.hover(Some(restart)), Some(Widget::Power(1)));
        assert_eq!(extras.hover(Some((1.0, 1.0))), None);
        assert_eq!(extras.hover(None), None);
        login.chooser.dialog = Some(Action::Restart);
        extras.update(&painter, &login.view(now)).unwrap();
        assert_eq!(extras.hover(Some(restart)), None);
        let cancel = extras.place(Widget::Cancel).unwrap().center();
        assert_eq!(extras.hover(Some(cancel)), Some(Widget::Cancel));
    }

    #[test]
    fn waking_wipes_what_was_typed_but_not_an_answer_submitted() {
        let now = Instant::now();
        let mut login = login(&["Alice"], &["A"], true, "");
        login.chooser.power = vec![Action::Sleep];
        login.chooser.focus.focused = Widget::Power(0);
        for c in "ab".chars() {
            login.entry.press(ui::entry::Key::Char(c), 1, now);
        }
        login.resumed(now);
        assert_eq!(login.entry.typed(), b"");
        assert_eq!(login.chooser.focus.focused, Widget::Pill);
        // Submitted (Enter, before greetd's prompt came): it stays, for greetd.
        login.entry.press(ui::entry::Key::Char('c'), 1, now);
        login.entry.press(ui::entry::Key::Enter, 2, now);
        login.resumed(now);
        assert_eq!(login.entry.submission().bytes(), b"c");
    }
}
