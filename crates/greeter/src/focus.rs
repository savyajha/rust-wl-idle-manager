//! The keyboard focus on the login screen. Tab and Shift+Tab move it through the widgets
//! shown, in reading order, and round; Left and Right move it along a row (the avatars, the
//! power buttons, the dialog's buttons). Enter and Space activate the widget it is on,
//! except in the field, which takes them as it takes any key that types; such a key, typed
//! elsewhere, moves the focus to the field first, if it is shown (not on the avatar row, nor
//! in the dialog). A ring shows it once a key has moved it, until a click. No I/O.

/// A widget the focus can be on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Widget {
    /// The avatar at this place in the row; "Other…" is last.
    Avatar(usize),
    /// The field.
    Pill,
    /// The "‹ Users" chip.
    Users,
    /// The session chip.
    Session,
    LogIn,
    /// The keyboard layout's indicator, when it opens a popup.
    Layout,
    /// The power button at this place in the power row.
    Power(usize),
    /// The confirmation dialog's buttons.
    Cancel,
    Confirm,
}

/// A key, as the focus has it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Tab,
    /// Shift+Tab.
    BackTab,
    Left,
    Right,
    /// Enter.
    Activate,
    /// Space: Enter, except on a power button, where it is text (a password may start with
    /// one, and must not put the machine to sleep).
    Space,
    /// A key the field acts on: a character, Backspace.
    Text,
}

/// What a key does, besides moving the focus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    None,
    /// The key goes to the field.
    Type,
    Activate(Widget),
}

#[derive(Debug)]
pub struct Focus {
    pub focused: Widget,
    /// Whether the ring shows: a key moved the focus since the last click.
    ring: bool,
}

impl Focus {
    /// The focus on `focused`, with no ring.
    pub fn new(focused: Widget) -> Self {
        Self {
            focused,
            ring: false,
        }
    }

    /// The widget the ring is drawn around, if any.
    pub fn ring(&self) -> Option<Widget> {
        self.ring.then_some(self.focused)
    }

    /// Act on `key`, with the widgets `shown` (in reading order).
    pub fn key(&mut self, key: Key, shown: &[Widget]) -> Action {
        let at = shown.iter().position(|&widget| widget == self.focused);
        let key = match key {
            Key::Space if matches!(self.focused, Widget::Power(_)) => Key::Text,
            Key::Space => Key::Activate,
            key => key,
        };
        match key {
            Key::Tab | Key::BackTab if !shown.is_empty() => {
                let n = shown.len();
                let next = match (at, key) {
                    (Some(i), Key::Tab) => (i + 1) % n,
                    (Some(i), _) => (i + n - 1) % n,
                    (None, _) => 0,
                };
                self.move_to(shown[next]);
            }
            // To the neighbour, if it is in the same row.
            Key::Left | Key::Right if row(self.focused).is_some() => {
                let next = at.and_then(|i| match key {
                    Key::Left => i.checked_sub(1),
                    _ => Some(i + 1),
                });
                let next = next.and_then(|i| shown.get(i)).copied();
                if let Some(next) = next.filter(|&next| row(next) == row(self.focused)) {
                    self.move_to(next);
                }
            }
            Key::Activate | Key::Text if self.focused == Widget::Pill => return Action::Type,
            Key::Activate if at.is_some() => return Action::Activate(self.focused),
            Key::Text if shown.contains(&Widget::Pill) => {
                self.focused = Widget::Pill;
                return Action::Type;
            }
            // Text on the avatar row, which has no field.
            _ => {}
        }
        Action::None
    }

    /// A click on `widget`: the focus goes there, and the ring is hidden.
    pub fn click(&mut self, widget: Widget) {
        self.focused = widget;
        self.ring = false;
    }

    fn move_to(&mut self, widget: Widget) {
        self.focused = widget;
        self.ring = true;
    }
}

/// Which row `widget` is in, for Left and Right, if any.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Avatars,
    Power,
    Dialog,
}

fn row(widget: Widget) -> Option<Row> {
    match widget {
        Widget::Avatar(_) => Some(Row::Avatars),
        Widget::Power(_) => Some(Row::Power),
        Widget::Cancel | Widget::Confirm => Some(Row::Dialog),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Widget::*;

    const LOGIN: [Widget; 4] = [Pill, Users, Session, LogIn];
    const ROW: [Widget; 3] = [Avatar(0), Avatar(1), Avatar(2)];

    #[test]
    fn tab_and_shift_tab_go_round_the_widgets_and_show_the_ring() {
        let mut focus = Focus::new(Pill);
        assert_eq!(focus.ring(), None);
        let mut tab = |key| {
            assert_eq!(focus.key(key, &LOGIN), Action::None);
            focus.ring()
        };
        assert_eq!(tab(Key::Tab), Some(Users));
        assert_eq!(tab(Key::Tab), Some(Session));
        assert_eq!(tab(Key::Tab), Some(LogIn));
        assert_eq!(tab(Key::Tab), Some(Pill));
        assert_eq!(tab(Key::BackTab), Some(LogIn));
        assert_eq!(tab(Key::BackTab), Some(Session));
        // Through the avatars too, one by one.
        let mut focus = Focus::new(Avatar(2));
        focus.key(Key::Tab, &ROW);
        assert_eq!(focus.focused, Avatar(0));
        // A widget no longer shown: Tab starts from the first; nothing shown, nothing moves.
        let mut focus = Focus::new(Session);
        focus.key(Key::Tab, &[Pill, LogIn]);
        assert_eq!(focus.focused, Pill);
        assert_eq!(focus.key(Key::Tab, &[]), Action::None);
        assert_eq!((focus.focused, focus.ring()), (Pill, Some(Pill)));
    }

    #[test]
    fn left_and_right_move_along_the_row_only() {
        let mut focus = Focus::new(Avatar(1));
        focus.key(Key::Left, &ROW);
        assert_eq!(focus.ring(), Some(Avatar(0)));
        // Not round.
        focus.key(Key::Left, &ROW);
        assert_eq!(focus.focused, Avatar(0));
        focus.key(Key::Right, &ROW);
        focus.key(Key::Right, &ROW);
        focus.key(Key::Right, &ROW);
        assert_eq!(focus.focused, Avatar(2));
        // Nothing elsewhere: no ring either.
        let mut focus = Focus::new(Session);
        assert_eq!(focus.key(Key::Right, &LOGIN), Action::None);
        assert_eq!((focus.focused, focus.ring()), (Session, None));
    }

    #[test]
    fn enter_and_space_activate_but_type_in_the_field() {
        let mut focus = Focus::new(Pill);
        assert_eq!(focus.key(Key::Activate, &LOGIN), Action::Type);
        for widget in [Users, Session, LogIn] {
            focus.focused = widget;
            assert_eq!(focus.key(Key::Activate, &LOGIN), Action::Activate(widget));
        }
        let mut focus = Focus::new(Avatar(1));
        assert_eq!(focus.key(Key::Activate, &ROW), Action::Activate(Avatar(1)));
        // Not a widget that is gone.
        assert_eq!(focus.key(Key::Activate, &LOGIN), Action::None);
    }

    #[test]
    fn typing_goes_to_the_field_and_moves_the_focus_there() {
        let mut focus = Focus::new(Session);
        focus.key(Key::Tab, &LOGIN);
        assert_eq!(focus.key(Key::Text, &LOGIN), Action::Type);
        // The ring stays: it is on the field now.
        assert_eq!(focus.ring(), Some(Pill));
        assert_eq!(focus.key(Key::Text, &LOGIN), Action::Type);
        // The row has no field.
        let mut focus = Focus::new(Avatar(0));
        assert_eq!(focus.key(Key::Text, &ROW), Action::None);
        assert_eq!(focus.focused, Avatar(0));
    }

    #[test]
    fn a_click_moves_the_focus_and_hides_the_ring() {
        let mut focus = Focus::new(Pill);
        focus.key(Key::Tab, &LOGIN);
        focus.click(LogIn);
        assert_eq!((focus.focused, focus.ring()), (LogIn, None));
        // Activating by key shows no ring either.
        focus.key(Key::Activate, &LOGIN);
        assert_eq!(focus.ring(), None);
        focus.key(Key::BackTab, &LOGIN);
        assert_eq!(focus.ring(), Some(Session));
    }

    #[test]
    fn left_and_right_move_along_the_power_row_and_the_dialog() {
        let shown = [Pill, LogIn, Power(0), Power(1), Layout];
        let mut focus = Focus::new(Power(0));
        focus.key(Key::Left, &shown);
        assert_eq!(focus.focused, Power(0));
        focus.key(Key::Right, &shown);
        focus.key(Key::Right, &shown);
        assert_eq!(focus.focused, Power(1));
        // Typed there, it goes to the field; so does Space, which is no Enter there.
        assert_eq!(focus.key(Key::Text, &shown), Action::Type);
        focus.focused = Power(0);
        assert_eq!(focus.key(Key::Space, &shown), Action::Type);
        assert_eq!(focus.focused, Pill);
        focus.focused = LogIn;
        assert_eq!(focus.key(Key::Space, &shown), Action::Activate(LogIn));
        let dialog = [Cancel, Confirm];
        let mut focus = Focus::new(Cancel);
        focus.key(Key::Right, &dialog);
        assert_eq!(focus.key(Key::Activate, &dialog), Action::Activate(Confirm));
        // The dialog has no field: typing does nothing.
        assert_eq!(focus.key(Key::Text, &dialog), Action::None);
        assert_eq!(focus.focused, Confirm);
    }
}
