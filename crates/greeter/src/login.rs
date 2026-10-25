//! The login screen: what it shows, the greeter's widgets (drawn over the lock screen's),
//! where they are for the pointer, and the session popup's list.

use cairo::{Context, Operator};
use pango::Weight;
use tokio::time::Instant;
use ui::draw::{self, Font, Label, Layout, Painter, Rect, Scene};
use ui::entry::{Entry, Look};
use ui::keyboard::Keyboard;
use ui::theme::Rgba;

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
    /// The session chip's text, with more than one session.
    session: Option<String>,
}

/// What a click on the login screen hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    LogIn,
    Users,
    Session,
}

impl Login {
    pub fn view(&self, now: Instant) -> View {
        let hint = self.prompt.text.trim().trim_end_matches(':').trim_end();
        let hint = if hint.is_empty() { draw::HINT } else { hint };
        let visible = !self.prompt.secret;
        let multiple = self.sessions.len() > 1;
        View {
            look: self.entry.look(now),
            hint: hint.to_owned(),
            text: visible.then(|| String::from_utf8_lossy(self.entry.typed()).into_owned()),
            message: self.message.clone(),
            session: multiple.then(|| self.sessions[self.chosen].name.clone()),
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

/// The greeter's widgets around the lock screen's, for one scene.
pub struct Extras {
    /// The field, from the scene.
    pill: Rect,
    factor: f64,
    log_in: Button,
    users: Button,
    session: Option<Button>,
    message: Option<Line>,
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
        Ok(Some(Self {
            pill,
            factor: f,
            log_in: button(painter, "Log In", log_in, draw::BUTTON, f)?,
            users: button(painter, "‹ Users", users, draw::FIELD, f)?,
            session: None,
            message: None,
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
        Ok(())
    }

    pub fn paint(&self, cr: &Context) -> Result<(), cairo::Error> {
        let buttons = [Some(&self.log_in), Some(&self.users), self.session.as_ref()];
        for button in buttons.into_iter().flatten() {
            draw::rounded(cr, button.rect, button.rect.h / 2.0);
            draw::frost(cr, button.alpha, OUTLINE, self.factor)?;
            button.label.paint(cr, 1.0)?;
        }
        if let Some(message) = &self.message {
            message.label.paint(cr, draw::HINT_ALPHA)?;
        }
        Ok(())
    }

    /// What is at `(x, y)`, in physical pixels.
    pub fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        let inside = |rect: Rect| {
            (rect.x..rect.x + rect.w).contains(&x) && (rect.y..rect.y + rect.h).contains(&y)
        };
        if inside(self.log_in.rect) {
            Some(Hit::LogIn)
        } else if inside(self.users.rect) {
            Some(Hit::Users)
        } else if self.session.as_ref().is_some_and(|s| inside(s.rect)) {
            Some(Hit::Session)
        } else {
            None
        }
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

    fn session(name: &str) -> Session {
        Session {
            file: format!("{name}.desktop"),
            name: name.into(),
            exec: vec![name.into()],
            desktop_names: Vec::new(),
        }
    }

    fn login(sessions: &[&str], secret: bool, prompt: &str) -> Login {
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
        }
    }

    #[test]
    fn the_view_has_the_prompt_the_visible_answer_and_the_session() {
        let now = Instant::now();
        let view = login(&["A", "B"], true, "Password: ").view(now);
        assert_eq!(view.hint, "Password");
        assert_eq!((view.text, view.session.as_deref()), (None, Some("B")));
        let view = login(&["A"], false, "").view(now);
        assert_eq!(view.hint, draw::HINT);
        // Typed text shows for a visible prompt only; one session needs no chip.
        assert_eq!((view.text.as_deref(), view.session), (Some(""), None));
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
