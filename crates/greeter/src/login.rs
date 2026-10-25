//! The login screen: what it shows, the greeter's widgets (drawn over the lock screen's, or
//! instead of its user, the avatar row), the focus ring, where they are for the pointer, and
//! the session popup's list.

use cairo::{Context, Operator};
use pango::Weight;
use tokio::time::Instant;
use ui::draw::{self, Avatar, Font, Label, Layout, Painter, Rect, Scene};
use ui::entry::{Entry, Look};
use ui::keyboard::Keyboard;
use ui::theme::Rgba;

use crate::chooser::{Chooser, Screen};
use crate::focus::Widget;
use crate::sessions::Session;

// In logical pixels. The Log In button and the "‹ Users" chip sit beside the field, the
// message line and the session chip under it (under the caps lock line).
const BESIDE: f64 = 12.0;
const BUTTON_WIDTH: f64 = 80.0;
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
    /// Whether the "‹ Users" chip is shown.
    users: bool,
    ring: Option<Widget>,
}

impl Login {
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
            users: self.chooser.has_row(),
            ring: self.chooser.focus.ring(),
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
    factor: f64,
    log_in: Button,
    users: Button,
    users_shown: bool,
    session: Option<Button>,
    message: Option<Line>,
    /// While it is shown, instead of a user.
    row: Option<Row>,
    ring: Option<Widget>,
}

impl Extras {
    /// The widgets around `scene`'s field; `None` for a plain scene, which has none.
    pub fn new(painter: &Painter, scene: &Scene) -> Result<Option<Self>, cairo::Error> {
        let Some(pill) = scene.pill() else {
            return Ok(None);
        };
        let f = scene.layout.factor();
        let log_in = whole(Rect {
            x: pill.x + pill.w + BESIDE * f,
            y: pill.y,
            w: BUTTON_WIDTH * f,
            h: pill.h,
        });
        let users = whole(Rect {
            x: pill.x - (BESIDE + BUTTON_WIDTH) * f,
            y: pill.center().1 - CHIP_HEIGHT * f / 2.0,
            w: BUTTON_WIDTH * f,
            h: CHIP_HEIGHT * f,
        });
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
        Ok(Some(Self {
            pill,
            avatar,
            factor: f,
            log_in: button(painter, "Log In", log_in, draw::BUTTON, f)?,
            users: button(painter, "‹ Users", users, draw::FIELD, f)?,
            users_shown: false,
            session: None,
            message: None,
            row: None,
            ring: None,
        }))
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
        self.users_shown = view.users;
        self.ring = view.ring;
        Ok(())
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
            let users = self.users_shown.then_some(&self.users);
            for button in [Some(&self.log_in), users, self.session.as_ref()]
                .into_iter()
                .flatten()
            {
                draw::rounded(cr, button.rect, button.rect.h / 2.0);
                draw::frost(cr, button.alpha, OUTLINE, f)?;
                button.label.paint(cr, 1.0)?;
            }
            if let Some(message) = &self.message {
                message.label.paint(cr, draw::HINT_ALPHA)?;
            }
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
    fn place(&self, widget: Widget) -> Option<Rect> {
        match (widget, &self.row) {
            (Widget::Avatar(i), Some(row)) => row.avatars.get(i).map(Avatar::square),
            (_, Some(_)) | (Widget::Avatar(_), None) => None,
            (Widget::Pill, None) => Some(self.pill),
            (Widget::Users, None) => self.users_shown.then_some(self.users.rect),
            (Widget::Session, None) => self.session.as_ref().map(|session| session.rect),
            (Widget::LogIn, None) => Some(self.log_in.rect),
        }
    }

    /// The widget a click at `(x, y)`, in physical pixels, is on; never the field.
    pub fn hit(&self, x: f64, y: f64) -> Option<Widget> {
        let inside = |rect: Rect| {
            (rect.x..rect.x + rect.w).contains(&x) && (rect.y..rect.y + rect.h).contains(&y)
        };
        let avatars = self.row.as_ref().map_or(0, |row| row.avatars.len());
        let buttons = [Widget::LogIn, Widget::Users, Widget::Session];
        let mut widgets = (0..avatars).map(Widget::Avatar).chain(buttons);
        widgets.find(|&widget| self.place(widget).is_some_and(inside))
    }

    /// The session chip, in physical pixels.
    pub fn session_chip(&self) -> Option<Rect> {
        self.session.as_ref().map(|session| session.rect)
    }
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

/// The session popup's rows, rendered at its parent's scale.
pub struct SessionList {
    /// The popup's size, in logical pixels.
    pub size: (i32, i32),
    factor: f64,
    rows: Vec<Label>,
}

impl SessionList {
    pub fn new(painter: &Painter, sessions: &[Session], factor: f64) -> Result<Self, cairo::Error> {
        let font = Font::new(draw::HINT_SIZE, Weight::Normal, WHITE);
        let mut rows = Vec::new();
        let mut width = POPUP_MIN_WIDTH;
        for (i, session) in sessions.iter().enumerate() {
            let top = (POPUP_PADDING + ROW_HEIGHT * i as f64) * factor;
            let label = painter.label(&session.name, &font, factor, |(_, h)| {
                let x = (POPUP_PADDING + PADDING) * factor;
                (x.round(), (top + (ROW_HEIGHT * factor - h) / 2.0).round())
            })?;
            width = width.max(label.size().0 / factor + 2.0 * (POPUP_PADDING + PADDING));
            rows.push(label);
        }
        let height = ROW_HEIGHT * sessions.len() as f64 + 2.0 * POPUP_PADDING;
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
            chooser: Chooser::new(users.collect(), None, sessions.len() > 1),
        }
    }

    #[test]
    fn the_view_has_the_prompt_the_visible_answer_and_the_session() {
        let now = Instant::now();
        let view = login(&["Alice"], &["A", "B"], true, "Password: ").view(now);
        assert_eq!(view.hint, "Password");
        assert_eq!((view.text, view.session.as_deref()), (None, Some("B")));
        // One user: their login, with "‹ Users" for the row of them and "Other…".
        assert_eq!((view.row, view.users), (None, true));
        // No one listed: the user name, with no row to go back to.
        let view = login(&[], &["A"], true, "").view(now);
        assert_eq!((&*view.hint, view.users), ("User name", false));
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
        assert_eq!((view.session, view.users), (None, true));
        login.chooser.choose(2);
        let view = login.view(now);
        // Asked for visibly, before any user is known: no session chip yet.
        assert_eq!((&*view.hint, view.text.as_deref()), ("User name", Some("")));
        assert_eq!((view.row, view.session, view.users), (None, None, true));
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
}
