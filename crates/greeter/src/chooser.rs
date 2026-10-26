//! Who logs in: the avatar row of the users listed, then "Other…", which asks for any user
//! name. Enter or a click on one starts their login; Escape or "‹ Users" goes back, to the
//! user name for a user typed, else to the row. With one user listed the greeter starts on
//! their login, and with none on "Other…"'s user name, which has no row to go back to. The
//! screen decides which widgets the focus moves through.
//!
//! Over any screen, the confirmation dialog of Restart or Shut Down holds the focus: only
//! its Cancel (focused at first) and its button take keys, and closing it puts the focus
//! back where it was. Escape closes it first; then it cancels the login. No I/O.

use crate::focus::{Focus, Widget};
use crate::power::Action;
use crate::users::User;

/// The last avatar's name.
pub const OTHER: &str = "Other…";

/// What the login screen shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    /// The avatar row.
    Row,
    /// "Other…": the field takes a user name.
    UserName,
    /// The user's login, through greetd's prompts.
    Login(User),
}

#[derive(Debug)]
pub struct Chooser {
    users: Vec<User>,
    pub screen: Screen,
    pub focus: Focus,
    /// Whether the user of `Screen::Login` was typed for "Other…".
    typed: bool,
    /// Whether the session chip shows during a login: there is more than one session.
    session_chip: bool,
    /// Whether the layout indicator opens a popup: there is more than one layout.
    layout_chip: bool,
    /// The power row's actions, once logind has said which it allows.
    pub power: Vec<Action>,
    /// The action the dialog asks to confirm, while it is open.
    pub dialog: Option<Action>,
    /// Where the focus was before the dialog.
    before_dialog: Widget,
}

/// What Escape does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Escape {
    CloseDialog,
    /// Cancel the login, going back (see `back`).
    Cancel,
}

impl Chooser {
    /// The first screen for `users`, the row's focus on the last user's avatar (named
    /// `last`): "Other…" if they are not listed, the first if there is none.
    pub fn new(
        users: Vec<User>,
        last: Option<&str>,
        session_chip: bool,
        layout_chip: bool,
    ) -> Self {
        let listed = users.iter().position(|user| Some(&*user.name) == last);
        let other = last.map(|_| users.len());
        let (screen, focused) = match &users[..] {
            [] => (Screen::UserName, Widget::Pill),
            [user] => (Screen::Login(user.clone()), Widget::Pill),
            _ => (Screen::Row, Widget::Avatar(listed.or(other).unwrap_or(0))),
        };
        Self {
            users,
            screen,
            focus: Focus::new(focused),
            typed: false,
            session_chip,
            layout_chip,
            power: Vec::new(),
            dialog: None,
            before_dialog: focused,
        }
    }

    /// Whether there is a row to go back to: someone is listed.
    pub fn has_row(&self) -> bool {
        !self.users.is_empty()
    }

    pub fn is_listed(&self, user: &User) -> bool {
        self.users.contains(user)
    }

    /// The row's names: the users' full names, then "Other…".
    pub fn names(&self) -> Vec<String> {
        let names = self.users.iter().map(|user| user.full_name.clone());
        names.chain([OTHER.to_owned()]).collect()
    }

    /// The widgets shown, in reading order; the layout indicator last (W2's order), so
    /// that Tab from the field goes through the login's widgets first.
    pub fn shown(&self) -> Vec<Widget> {
        if self.dialog.is_some() {
            return vec![Widget::Cancel, Widget::Confirm];
        }
        let mut shown = Vec::new();
        if self.screen == Screen::Row {
            shown.extend((0..=self.users.len()).map(Widget::Avatar));
        } else {
            shown.push(Widget::Pill);
            if self.has_row() {
                shown.push(Widget::Users);
            }
            if self.session_chip && matches!(self.screen, Screen::Login(_)) {
                shown.push(Widget::Session);
            }
            shown.push(Widget::LogIn);
        }
        shown.extend((0..self.power.len()).map(Widget::Power));
        if self.layout_chip {
            shown.push(Widget::Layout);
        }
        shown
    }

    /// Ask to confirm `action` in the dialog, its Cancel focused.
    pub fn open_dialog(&mut self, action: Action) {
        if self.dialog.is_none() {
            self.before_dialog = self.focus.focused;
        }
        self.dialog = Some(action);
        self.focus.focused = Widget::Cancel;
    }

    /// Close the dialog, the focus back where it was; returns what it asked to confirm.
    pub fn close_dialog(&mut self) -> Option<Action> {
        let action = self.dialog.take()?;
        self.focus.focused = self.before_dialog;
        Some(action)
    }

    /// The machine woke: the focus leaves the power row, so that Enter (or a key typed on
    /// it) does not put it to sleep again. To the field, or on the row the first avatar.
    pub fn resumed(&mut self) {
        if matches!(self.focus.focused, Widget::Power(_)) {
            self.focus.focused = self.shown()[0];
        }
    }

    /// What Escape does: the topmost first. (Popups are closed by the compositor's grab
    /// before the key would come here.)
    pub fn escape(&self) -> Escape {
        if self.dialog.is_some() {
            Escape::CloseDialog
        } else {
            Escape::Cancel
        }
    }

    /// Avatar `i` of the row was activated: its user, whose login it shows, or `None` for
    /// "Other…", which asks for a user name. Nothing off the row.
    pub fn choose(&mut self, i: usize) -> Option<User> {
        if self.screen != Screen::Row {
            return None;
        }
        self.focus.focused = Widget::Pill;
        self.typed = false;
        match self.users.get(i) {
            Some(user) => {
                self.screen = Screen::Login(user.clone());
                Some(user.clone())
            }
            None => {
                self.screen = Screen::UserName;
                None
            }
        }
    }

    /// The user name `typed` for "Other…", any at all: its user, whose login it shows (with
    /// a listed user's full name, else called by the name), unless it is blank.
    pub fn name_typed(&mut self, typed: &str) -> Option<User> {
        let name = typed.trim();
        if self.screen != Screen::UserName || name.is_empty() {
            return None;
        }
        let listed = self.users.iter().find(|user| user.name == name);
        let user = listed.cloned().unwrap_or_else(|| User {
            name: name.to_owned(),
            full_name: name.to_owned(),
        });
        self.screen = Screen::Login(user.clone());
        self.typed = true;
        Some(user)
    }

    /// Back from a user typed to the user name, focused on the field; else to the row,
    /// focused on the avatar left. Returns false, changing nothing, where there is no back:
    /// on the row, or at the user name with no row.
    pub fn back(&mut self) -> bool {
        let left = match &self.screen {
            Screen::Login(_) if self.typed => {
                self.screen = Screen::UserName;
                self.focus.focused = Widget::Pill;
                return true;
            }
            Screen::Row => return false,
            Screen::UserName if !self.has_row() => return false,
            Screen::Login(left) => self.users.iter().position(|user| user == left),
            Screen::UserName => None,
        };
        self.focus.focused = Widget::Avatar(left.unwrap_or(self.users.len()));
        self.screen = Screen::Row;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::{Action, Key};
    use Widget::*;

    fn user(name: &str, full_name: &str) -> User {
        User {
            name: name.into(),
            full_name: full_name.into(),
        }
    }

    fn two() -> Vec<User> {
        vec![user("alice", "Alice Liddell"), user("bob", "Bob Builder")]
    }

    /// What Enter does on `chooser`'s screen.
    fn enter(chooser: &mut Chooser) -> Action {
        let shown = chooser.shown();
        chooser.focus.key(Key::Activate, &shown)
    }

    #[test]
    fn one_user_starts_on_their_login_with_a_row_behind_it() {
        let alice = user("alice", "Alice");
        let mut chooser = Chooser::new(vec![alice.clone()], None, true, false);
        assert_eq!(chooser.screen, Screen::Login(alice.clone()));
        assert_eq!(chooser.shown(), [Pill, Users, Session, LogIn]);
        // Off the row, nothing is chosen.
        assert_eq!(chooser.choose(0), None);
        // "‹ Users": the row of alice and "Other…", on alice.
        assert!(chooser.back());
        assert_eq!(chooser.names(), ["Alice", OTHER]);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(0)));
        // "Other…" takes anyone: root, or a user who is not listed.
        assert_eq!(chooser.choose(1), None);
        assert_eq!(chooser.name_typed("root"), Some(user("root", "root")));
        assert!(!chooser.is_listed(&user("root", "root")) && chooser.is_listed(&alice));
    }

    #[test]
    fn with_no_one_listed_a_mistyped_name_can_be_typed_again() {
        let mut chooser = Chooser::new(Vec::new(), Some("root"), true, false);
        assert_eq!(chooser.screen, Screen::UserName);
        assert_eq!(chooser.shown(), [Pill, LogIn]);
        // No row to go back to from the user name.
        assert!(!chooser.back());
        assert_eq!(chooser.name_typed("rooot"), Some(user("rooot", "rooot")));
        assert_eq!(chooser.shown(), [Pill, Session, LogIn]);
        // Back to the user name, on the field.
        chooser.focus.focused = LogIn;
        assert!(chooser.back());
        assert_eq!(
            (&chooser.screen, chooser.focus.focused),
            (&Screen::UserName, Pill)
        );
        assert_eq!(chooser.name_typed("root"), Some(user("root", "root")));
    }

    #[test]
    fn the_row_starts_on_the_last_user() {
        let mut chooser = Chooser::new(two(), Some("bob"), true, false);
        assert_eq!(chooser.screen, Screen::Row);
        assert_eq!(chooser.shown(), [Avatar(0), Avatar(1), Avatar(2)]);
        assert_eq!(chooser.names(), ["Alice Liddell", "Bob Builder", OTHER]);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(1)));
        // One who is not listed (typed for "Other…" last time): on "Other…".
        let mut chooser = Chooser::new(two(), Some("root"), true, false);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(2)));
        // None: on the first.
        let mut chooser = Chooser::new(two(), None, true, false);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(0)));
    }

    #[test]
    fn choosing_shows_the_login_and_back_returns_to_the_avatar_left() {
        let mut chooser = Chooser::new(two(), None, true, false);
        assert_eq!(chooser.choose(1), Some(user("bob", "Bob Builder")));
        assert_eq!(chooser.shown(), [Pill, Users, Session, LogIn]);
        assert_eq!(chooser.focus.focused, Pill);
        assert!(chooser.back());
        assert_eq!(chooser.screen, Screen::Row);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(1)));
        // On the row, there is nothing to go back from.
        assert!(!chooser.back());
        // One session: no session chip.
        let mut chooser = Chooser::new(two(), None, false, false);
        chooser.choose(0);
        assert_eq!(chooser.shown(), [Pill, Users, LogIn]);
    }

    #[test]
    fn other_asks_for_a_user_name_and_back_goes_one_step() {
        let mut chooser = Chooser::new(two(), None, true, false);
        assert_eq!(chooser.choose(2), None);
        assert_eq!(chooser.screen, Screen::UserName);
        // No session chip until the user is known.
        assert_eq!(chooser.shown(), [Pill, Users, LogIn]);
        assert_eq!(chooser.name_typed("  "), None);
        assert_eq!(chooser.screen, Screen::UserName);
        // A listed user has their full name; another is called by the name typed.
        assert_eq!(
            chooser.name_typed(" bob\n"),
            Some(user("bob", "Bob Builder"))
        );
        // Typed, even a listed one: back to the user name, then to the row, on "Other…".
        assert!(chooser.back());
        assert_eq!(chooser.screen, Screen::UserName);
        assert_eq!(chooser.name_typed("dave"), Some(user("dave", "dave")));
        assert!(chooser.back());
        assert!(chooser.back());
        assert_eq!(chooser.screen, Screen::Row);
        assert_eq!(enter(&mut chooser), Action::Activate(Avatar(2)));
        // A user chosen from the row after one typed goes back to the row.
        chooser.choose(0);
        assert!(chooser.back());
        assert_eq!(chooser.screen, Screen::Row);
    }

    #[test]
    fn the_layout_indicator_is_last_where_it_opens_a_popup() {
        let mut chooser = Chooser::new(two(), None, false, /* layout_chip */ true);
        assert_eq!(chooser.shown(), [Avatar(0), Avatar(1), Avatar(2), Layout]);
        chooser.choose(0);
        assert_eq!(chooser.shown(), [Pill, Users, LogIn, Layout]);
        let shown = chooser.shown();
        for _ in 0..3 {
            chooser.focus.key(Key::Tab, &shown);
        }
        assert_eq!(
            chooser.focus.key(Key::Activate, &shown),
            Action::Activate(Layout)
        );
        // Typed there, it goes to the field.
        assert_eq!(chooser.focus.key(Key::Text, &shown), Action::Type);
        assert_eq!(chooser.focus.focused, Pill);
    }

    #[test]
    fn the_power_buttons_follow_log_in_and_the_dialog_holds_the_focus() {
        let mut chooser = Chooser::new(two(), None, false, /* layout_chip */ true);
        chooser.power = vec![crate::power::Action::Sleep, crate::power::Action::Restart];
        assert_eq!(
            chooser.shown(),
            [Avatar(0), Avatar(1), Avatar(2), Power(0), Power(1), Layout]
        );
        chooser.choose(0);
        assert_eq!(
            chooser.shown(),
            [Pill, Users, LogIn, Power(0), Power(1), Layout]
        );
        chooser.focus.focused = Power(1);
        assert_eq!(chooser.escape(), Escape::Cancel);
        chooser.open_dialog(crate::power::Action::Restart);
        assert_eq!(chooser.shown(), [Cancel, Confirm]);
        assert_eq!(chooser.focus.focused, Cancel);
        assert_eq!(chooser.escape(), Escape::CloseDialog);
        // Tab goes round its two buttons only, and typing does nothing.
        let shown = chooser.shown();
        chooser.focus.key(Key::Tab, &shown);
        chooser.focus.key(Key::Tab, &shown);
        assert_eq!(chooser.focus.focused, Cancel);
        assert_eq!(chooser.focus.key(Key::Text, &shown), Action::None);
        // Closed, the focus is back.
        assert_eq!(chooser.close_dialog(), Some(crate::power::Action::Restart));
        assert_eq!((chooser.focus.focused, chooser.dialog), (Power(1), None));
        assert_eq!(chooser.close_dialog(), None);
        // The login under it is as it was.
        assert!(matches!(chooser.screen, Screen::Login(_)));
    }

    #[test]
    fn waking_moves_the_focus_off_the_power_row() {
        let mut chooser = Chooser::new(two(), None, false, false);
        chooser.power = vec![crate::power::Action::Sleep];
        chooser.focus.focused = Power(0);
        chooser.resumed();
        assert_eq!(chooser.focus.focused, Avatar(0));
        chooser.choose(1);
        chooser.focus.focused = Power(0);
        chooser.resumed();
        assert_eq!(chooser.focus.focused, Pill);
        // Elsewhere, it stays.
        chooser.focus.focused = LogIn;
        chooser.resumed();
        assert_eq!(chooser.focus.focused, LogIn);
    }
}
