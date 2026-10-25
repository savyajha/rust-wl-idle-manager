//! The keyboard focus on the login screen. Tab and Shift+Tab move it through the widgets
//! shown, in reading order, and round; Left and Right move it along the avatar row. Enter
//! and Space activate the widget it is on, except in the field, which takes them as it
//! takes any key that types; such a key, typed elsewhere, moves the focus to the field
//! first. A ring shows it once a key has moved it, until a click. No I/O.

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
}

/// A key, as the focus has it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Key {
    Tab,
    /// Shift+Tab.
    BackTab,
    Left,
    Right,
    /// Enter or Space.
    Activate,
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
            // To the neighbour in the row, if it is an avatar too.
            Key::Left | Key::Right if matches!(self.focused, Widget::Avatar(_)) => {
                let next = at.and_then(|i| match key {
                    Key::Left => i.checked_sub(1),
                    _ => Some(i + 1),
                });
                if let Some(&next @ Widget::Avatar(_)) = next.and_then(|i| shown.get(i)) {
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
}
