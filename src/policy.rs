use std::mem;

use crate::config::{Action, Timeout};

/// Something that happened, from Wayland, logind or the authentication helper.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    /// The timeout at this index became idle (ext-idle-notify `idled`).
    Idled(usize),
    /// The timeout at this index is no longer idle (`resumed`).
    Resumed(usize),
    /// Whether a logind `idle` inhibitor is held (from `BlockInhibited`).
    Inhibited(bool),
    /// Whether our logind session is active (the `Active` property; false on another VT).
    SessionActive(bool),
    /// logind asked the session to lock (`Lock` signal, e.g. `loginctl lock-session`).
    LockRequested,
    /// logind asked the session to unlock (`Unlock` signal).
    UnlockRequested,
    /// Whether the session is locked: from the built-in lock screen (the compositor's
    /// `locked`, or the lock ending), or else logind's `LockedHint`, which the compositor
    /// sets once the locker has locked.
    Locked(bool),
    /// logind's `LidClosed`.
    LidClosed(bool),
    /// logind's `PrepareForSleep`: true before sleep, false after waking.
    PrepareForSleep(bool),
    /// The wait for the lock before sleep timed out.
    LockWaitTimedOut,
    /// A password was typed on the lock screen and Enter pressed.
    PasswordEntered,
    /// Whether the authentication helper accepted the password.
    Authenticated(bool),
}

/// What the I/O shell should do.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Lock with the built-in lock screen (unless a lock is already requested), or start
    /// the locker unit (refused by systemd if it already runs).
    Lock,
    /// Unlock the built-in lock screen, or ask the locker unit to unlock, with SIGUSR1.
    Unlock,
    /// Suspend through logind.
    Suspend,
    /// Suspend, then hibernate, through logind.
    SuspendThenHibernate,
    /// Hibernate through logind.
    Hibernate,
    /// Run this argv.
    Spawn(Vec<String>),
    /// Check the lock screen's password with the authentication helper.
    Authenticate,
    /// Start the timer that ends in `Input::LockWaitTimedOut` (about 4 s, chosen by the shell).
    WaitForLock,
    /// Let sleep go ahead, and stop waiting for the lock.
    ReleaseSleepInhibitor,
    /// Take a new sleep delay inhibitor, replacing any still held.
    TakeSleepInhibitor,
    /// Re-create the idle notifications at these indices so their timers start again.
    Rearm(Vec<usize>),
}

/// Decides what to do about each input. Every field is the latest value from its
/// source, never a count, so a missed or repeated input cannot leave it skewed.
pub struct Policy {
    timeouts: Vec<Timeout>,
    /// Whether each timeout's action ran since it last became idle.
    ran: Vec<bool>,
    inhibited: bool,
    active: bool,
    locked: bool,
    lid_closed: bool,
    /// Between `PrepareForSleep(true)` and releasing the sleep inhibitor.
    waiting_for_lock: bool,
}

impl Policy {
    /// A policy for `timeouts`, indexed as in `Input::Idled` and `Input::Resumed`.
    pub fn new(timeouts: Vec<Timeout>) -> Self {
        Self {
            ran: vec![false; timeouts.len()],
            timeouts,
            inhibited: false,
            active: true,
            locked: false,
            // So that a lid already closed at startup locks nothing.
            lid_closed: true,
            waiting_for_lock: false,
        }
    }

    /// Update the state from `input` and return the commands to run, in order.
    ///
    /// The order matters in three places: `Lock` comes before `WaitForLock`,
    /// `ReleaseSleepInhibitor` before `TakeSleepInhibitor`, and on-resume spawns before
    /// `Rearm`.
    pub fn handle(&mut self, input: Input) -> Vec<Command> {
        let command = match input {
            Input::Idled(i) => self.idled(i),
            Input::Resumed(i) => self.resume(i),
            Input::Inhibited(inhibited) => {
                if !mem::replace(&mut self.inhibited, inhibited) || inhibited {
                    return Vec::new();
                }
                // Restart the timers the inhibitor held back. Timeouts whose action ran
                // keep their notifications, so their `resumed` still arrives.
                let held: Vec<_> = (0..self.ran.len())
                    .filter(|&i| !self.ran[i] && !self.timeouts[i].ignore_inhibit)
                    .collect();
                (!held.is_empty()).then_some(Command::Rearm(held))
            }
            // No rearm: the compositor reports activity when its VT becomes active again.
            Input::SessionActive(active) => {
                self.active = active;
                None
            }
            Input::LockRequested => self.lock(),
            // Unlocking when not locked does nothing. Only these two unlock.
            Input::UnlockRequested => Some(Command::Unlock),
            Input::Authenticated(ok) => ok.then_some(Command::Unlock),
            Input::PasswordEntered => Some(Command::Authenticate),
            Input::Locked(locked) => {
                self.locked = locked;
                if locked { self.release() } else { None }
            }
            Input::LidClosed(closed) => {
                let was_closed = mem::replace(&mut self.lid_closed, closed);
                if closed && !was_closed {
                    self.lock()
                } else {
                    None
                }
            }
            Input::PrepareForSleep(true) if self.waiting_for_lock => None,
            Input::PrepareForSleep(true) if self.locked => Some(Command::ReleaseSleepInhibitor),
            // A session in the background cannot show its lock promptly, so sleep does not
            // wait for it.
            Input::PrepareForSleep(true) if !self.active => {
                return vec![Command::Lock, Command::ReleaseSleepInhibitor];
            }
            // A lock already under way is not started twice (systemd refuses a second locker);
            // the wait still applies.
            Input::PrepareForSleep(true) => {
                self.waiting_for_lock = true;
                return vec![Command::Lock, Command::WaitForLock];
            }
            // Restart every timer from the wake. The old notifications never send
            // `resumed`, so pending on-resume spawns run first.
            Input::PrepareForSleep(false) => {
                let mut commands = Vec::from_iter(self.release());
                commands.push(Command::TakeSleepInhibitor);
                commands.extend((0..self.ran.len()).filter_map(|i| self.resume(i)));
                commands.push(Command::Rearm((0..self.ran.len()).collect()));
                return commands;
            }
            // A broken locker must never block sleep; outside a wait, the timer is stale.
            Input::LockWaitTimedOut => self.release(),
        };
        command.into_iter().collect()
    }

    /// Run timeout `i`'s action, unless the session is inactive or an inhibitor applies.
    fn idled(&mut self, i: usize) -> Option<Command> {
        if !self.active || (self.inhibited && !self.timeouts[i].ignore_inhibit) {
            return None;
        }
        self.ran[i] = true;
        match &self.timeouts[i].action {
            Action::Lock => self.lock(),
            Action::Suspend => Some(Command::Suspend),
            Action::SuspendThenHibernate => Some(Command::SuspendThenHibernate),
            Action::Hibernate => Some(Command::Hibernate),
            Action::Spawn(argv) => Some(Command::Spawn(argv.clone())),
        }
    }

    /// Clear `ran[i]`; if the action had run, return the timeout's on-resume spawn, if any.
    fn resume(&mut self, i: usize) -> Option<Command> {
        if !mem::take(&mut self.ran[i]) {
            return None;
        }
        self.timeouts[i].on_resume.clone().map(Command::Spawn)
    }

    /// Lock, unless the session is already locked.
    fn lock(&self) -> Option<Command> {
        (!self.locked).then_some(Command::Lock)
    }

    /// Stop waiting for the lock and let sleep go ahead, if we were waiting.
    fn release(&mut self) -> Option<Command> {
        mem::take(&mut self.waiting_for_lock).then_some(Command::ReleaseSleepInhibitor)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use Command::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn timeout(action: Action, on_resume: Option<&[&str]>, ignore_inhibit: bool) -> Timeout {
        Timeout {
            after: Duration::from_secs(1),
            action,
            on_resume: on_resume.map(argv),
            ignore_inhibit,
        }
    }

    /// 0: lock; 1: monitors off with an on-resume, ignoring inhibitors; 2: suspend.
    fn policy() -> Policy {
        Policy::new(vec![
            timeout(Action::Lock, None, false),
            timeout(Action::Spawn(argv(&["off"])), Some(&["on"]), true),
            timeout(Action::Suspend, None, false),
        ])
    }

    #[test]
    fn each_action_becomes_its_command() {
        let mut policy = Policy::new(vec![
            timeout(Action::Lock, None, false),
            timeout(Action::Suspend, None, false),
            timeout(Action::SuspendThenHibernate, None, false),
            timeout(Action::Hibernate, None, false),
            timeout(Action::Spawn(argv(&["a", "b"])), None, false),
        ]);
        let want = [
            Lock,
            Suspend,
            SuspendThenHibernate,
            Hibernate,
            Spawn(argv(&["a", "b"])),
        ];
        for (i, want) in want.into_iter().enumerate() {
            assert_eq!(policy.handle(Input::Idled(i)), vec![want]);
        }
    }

    #[test]
    fn an_inhibitor_suppresses_all_but_ignore_inhibit_timeouts() {
        let mut policy = policy();
        policy.handle(Input::Inhibited(true));
        assert_eq!(policy.handle(Input::Idled(0)), vec![]);
        assert_eq!(policy.handle(Input::Idled(1)), vec![Spawn(argv(&["off"]))]);
        assert_eq!(policy.handle(Input::Idled(2)), vec![]);
        assert_eq!(policy.ran, [false, true, false]);
    }

    #[test]
    fn an_inactive_session_suppresses_every_timeout() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::SessionActive(false)), vec![]);
        for i in 0..3 {
            assert_eq!(policy.handle(Input::Idled(i)), vec![]);
        }
        assert_eq!(policy.ran, [false; 3]);
        assert_eq!(policy.handle(Input::SessionActive(true)), vec![]);
        assert_eq!(policy.handle(Input::Idled(2)), vec![Suspend]);
    }

    #[test]
    fn on_resume_runs_only_if_the_action_ran() {
        let mut policy = policy();
        policy.handle(Input::SessionActive(false));
        policy.handle(Input::Idled(1));
        policy.handle(Input::SessionActive(true));
        assert_eq!(policy.handle(Input::Resumed(1)), vec![]);
        policy.handle(Input::Idled(1));
        assert_eq!(policy.handle(Input::Resumed(1)), vec![Spawn(argv(&["on"]))]);
        assert_eq!(policy.handle(Input::Resumed(1)), vec![]);
    }

    #[test]
    fn resumed_never_unlocks() {
        let mut policy = policy();
        policy.handle(Input::Idled(0));
        policy.handle(Input::Locked(true));
        assert_eq!(policy.handle(Input::Resumed(0)), vec![]);
    }

    #[test]
    fn clearing_the_inhibitor_rearms_once() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::Inhibited(false)), vec![]);
        assert_eq!(policy.handle(Input::Inhibited(true)), vec![]);
        assert_eq!(policy.handle(Input::Inhibited(true)), vec![]);
        assert_eq!(
            policy.handle(Input::Inhibited(false)),
            vec![Rearm(vec![0, 2])]
        );
        assert_eq!(policy.handle(Input::Inhibited(false)), vec![]);
    }

    #[test]
    fn clearing_the_inhibitor_rearms_only_timeouts_it_held_back() {
        let mut policy = policy();
        policy.handle(Input::Idled(0));
        policy.handle(Input::Inhibited(true));
        assert_eq!(policy.handle(Input::Idled(1)), vec![Spawn(argv(&["off"]))]);
        assert_eq!(policy.handle(Input::Idled(2)), vec![]);
        // Monitors stay off: timeout 1 ran, so it is not re-created and its on-resume waits.
        assert_eq!(policy.handle(Input::Inhibited(false)), vec![Rearm(vec![2])]);
        assert_eq!(policy.ran, [true, true, false]);
        assert_eq!(policy.handle(Input::Resumed(1)), vec![Spawn(argv(&["on"]))]);
    }

    #[test]
    fn clearing_the_inhibitor_with_nothing_held_back_does_nothing() {
        let mut policy = Policy::new(vec![timeout(Action::Lock, None, true)]);
        policy.handle(Input::Inhibited(true));
        assert_eq!(policy.handle(Input::Inhibited(false)), vec![]);
    }

    #[test]
    fn no_lock_while_locked() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::LockRequested), vec![Lock]);
        // Not locked yet, so a second request locks again, which the shell makes harmless.
        assert_eq!(policy.handle(Input::LockRequested), vec![Lock]);
        policy.handle(Input::Locked(true));
        assert_eq!(policy.handle(Input::LockRequested), vec![]);
        assert_eq!(policy.handle(Input::Idled(0)), vec![]);
        policy.handle(Input::Locked(false));
        assert_eq!(policy.handle(Input::LockRequested), vec![Lock]);
    }

    #[test]
    fn unlock_always_unlocks() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::UnlockRequested), vec![Unlock]);
        policy.handle(Input::Locked(true));
        assert_eq!(policy.handle(Input::UnlockRequested), vec![Unlock]);
    }

    #[test]
    fn only_a_right_password_unlocks() {
        let mut policy = policy();
        policy.handle(Input::Locked(true));
        assert_eq!(policy.handle(Input::PasswordEntered), vec![Authenticate]);
        assert_eq!(policy.handle(Input::Authenticated(false)), vec![]);
        assert_eq!(policy.handle(Input::Authenticated(true)), vec![Unlock]);
    }

    #[test]
    fn sleep_locks_and_waits_until_locked() {
        let mut policy = policy();
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![Lock, WaitForLock]
        );
        assert_eq!(policy.handle(Input::PrepareForSleep(true)), vec![]);
        assert_eq!(policy.handle(Input::Locked(false)), vec![]);
        assert_eq!(
            policy.handle(Input::Locked(true)),
            vec![ReleaseSleepInhibitor]
        );
        // The timer is stale now.
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
    }

    #[test]
    fn sleep_while_locking_locks_again_and_waits() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::Idled(0)), vec![Lock]);
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![Lock, WaitForLock]
        );
        assert_eq!(
            policy.handle(Input::Locked(true)),
            vec![ReleaseSleepInhibitor]
        );
    }

    #[test]
    fn sleep_when_locked_releases_at_once() {
        let mut policy = policy();
        policy.handle(Input::Locked(true));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![ReleaseSleepInhibitor]
        );
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
    }

    #[test]
    fn a_lock_wait_timeout_releases_the_inhibitor() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
        policy.handle(Input::PrepareForSleep(true));
        assert_eq!(
            policy.handle(Input::LockWaitTimedOut),
            vec![ReleaseSleepInhibitor]
        );
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
        assert_eq!(policy.handle(Input::Locked(true)), vec![]);
    }

    #[test]
    fn waking_runs_pending_on_resume_spawns_before_rearming_all() {
        let mut policy = policy();
        policy.handle(Input::Idled(1));
        policy.handle(Input::Idled(2));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![Lock, WaitForLock]
        );
        // Still waiting, so the held inhibitor is released before it is taken again.
        assert_eq!(
            policy.handle(Input::PrepareForSleep(false)),
            vec![
                ReleaseSleepInhibitor,
                TakeSleepInhibitor,
                Spawn(argv(&["on"])),
                Rearm(vec![0, 1, 2])
            ]
        );
        assert_eq!(policy.ran, [false; 3]);
        // Waiting stopped, so a late hint or timer releases nothing.
        assert_eq!(policy.handle(Input::Locked(true)), vec![]);
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
    }

    #[test]
    fn waking_after_a_release_only_retakes_the_inhibitor_and_rearms() {
        let mut policy = policy();
        policy.handle(Input::Locked(true));
        policy.handle(Input::PrepareForSleep(true));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(false)),
            vec![TakeSleepInhibitor, Rearm(vec![0, 1, 2])]
        );
    }

    #[test]
    fn sleep_while_inactive_locks_without_waiting() {
        let mut policy = policy();
        policy.handle(Input::SessionActive(false));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![Lock, ReleaseSleepInhibitor]
        );
        assert_eq!(policy.handle(Input::Locked(true)), vec![]);
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
        policy.handle(Input::PrepareForSleep(false));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![ReleaseSleepInhibitor]
        );
    }

    #[test]
    fn closing_the_lid_locks() {
        let mut policy = policy();
        // Closed at startup: nothing to lock for, as the lid did not just close.
        assert_eq!(policy.handle(Input::LidClosed(true)), vec![]);
        assert_eq!(policy.handle(Input::LidClosed(false)), vec![]);
        assert_eq!(policy.handle(Input::LidClosed(true)), vec![Lock]);
        assert_eq!(policy.handle(Input::LidClosed(true)), vec![]);
        policy.handle(Input::LidClosed(false));
        policy.handle(Input::Locked(true));
        assert_eq!(policy.handle(Input::LidClosed(true)), vec![]);
    }
}
