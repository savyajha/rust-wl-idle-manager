use std::mem;

use crate::config::{Action, Timeout};

/// Something that happened, from Wayland or logind.
#[allow(dead_code)] // logind inputs arrive in step 4
#[derive(Debug, PartialEq)]
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
    /// The session's logind `LockedHint`, set by the compositor once the lock screen is drawn.
    LockedHint(bool),
    /// Whether the locker unit is active.
    LockerRunning(bool),
    /// logind's `PrepareForSleep`: true before sleep, false after waking.
    PrepareForSleep(bool),
    /// The wait for the lock before sleep timed out.
    LockWaitTimedOut,
}

/// What the I/O shell should do.
#[derive(Debug, PartialEq)]
pub enum Command {
    StartLocker,
    /// Ask the locker unit to unlock, with SIGUSR1.
    UnlockLocker,
    Suspend,
    SuspendThenHibernate,
    Hibernate,
    /// Run this argv.
    Spawn(Vec<String>),
    /// Start the timer that ends in `Input::LockWaitTimedOut` (about 4 s, chosen by the shell).
    WaitForLock,
    ReleaseSleepInhibitor,
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
    locked_hint: bool,
    locker_running: bool,
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
            locked_hint: false,
            locker_running: false,
            waiting_for_lock: false,
        }
    }

    /// Update the state from `input` and return the commands to run, in order.
    ///
    /// The order matters in three places: `StartLocker` comes before `WaitForLock`,
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
            // Only our own locker can be unlocked.
            Input::UnlockRequested => self.locker_running.then_some(Command::UnlockLocker),
            Input::LockedHint(hint) => {
                self.locked_hint = hint;
                if hint { self.release() } else { None }
            }
            Input::LockerRunning(running) => {
                self.locker_running = running;
                None
            }
            Input::PrepareForSleep(true) if self.waiting_for_lock => None,
            Input::PrepareForSleep(true) if self.locked_hint => {
                Some(Command::ReleaseSleepInhibitor)
            }
            // If the locker runs but has not drawn the lock yet, only wait.
            Input::PrepareForSleep(true) => {
                self.waiting_for_lock = true;
                let mut commands = Vec::from_iter(self.lock());
                commands.push(Command::WaitForLock);
                return commands;
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

    /// Start the locker, unless it is running or the session is already locked.
    fn lock(&self) -> Option<Command> {
        (!self.locker_running && !self.locked_hint).then_some(Command::StartLocker)
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
            StartLocker,
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
        policy.handle(Input::LockerRunning(true));
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
    fn the_locker_is_not_started_while_running_or_locked() {
        let mut policy = policy();
        assert_eq!(policy.handle(Input::LockRequested), vec![StartLocker]);
        policy.handle(Input::LockerRunning(true));
        assert_eq!(policy.handle(Input::LockRequested), vec![]);
        assert_eq!(policy.handle(Input::Idled(0)), vec![]);
        policy.handle(Input::LockerRunning(false));
        policy.handle(Input::LockedHint(true));
        assert_eq!(policy.handle(Input::LockRequested), vec![]);
        assert_eq!(policy.handle(Input::Idled(0)), vec![]);
        policy.handle(Input::LockedHint(false));
        assert_eq!(policy.handle(Input::LockRequested), vec![StartLocker]);
    }

    #[test]
    fn unlock_needs_our_locker_running() {
        let mut policy = policy();
        policy.handle(Input::LockedHint(true));
        assert_eq!(policy.handle(Input::UnlockRequested), vec![]);
        policy.handle(Input::LockerRunning(true));
        assert_eq!(policy.handle(Input::UnlockRequested), vec![UnlockLocker]);
    }

    #[test]
    fn sleep_starts_the_locker_and_waits_for_the_hint() {
        let mut policy = policy();
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![StartLocker, WaitForLock]
        );
        assert_eq!(policy.handle(Input::PrepareForSleep(true)), vec![]);
        policy.handle(Input::LockerRunning(true));
        assert_eq!(policy.handle(Input::LockedHint(false)), vec![]);
        assert_eq!(
            policy.handle(Input::LockedHint(true)),
            vec![ReleaseSleepInhibitor]
        );
        // The timer is stale now.
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
    }

    #[test]
    fn sleep_with_the_locker_starting_only_waits() {
        let mut policy = policy();
        policy.handle(Input::LockerRunning(true));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![WaitForLock]
        );
        assert_eq!(
            policy.handle(Input::LockedHint(true)),
            vec![ReleaseSleepInhibitor]
        );
    }

    #[test]
    fn sleep_when_locked_releases_at_once() {
        let mut policy = policy();
        policy.handle(Input::LockedHint(true));
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
        assert_eq!(policy.handle(Input::LockedHint(true)), vec![]);
    }

    #[test]
    fn waking_runs_pending_on_resume_spawns_before_rearming_all() {
        let mut policy = policy();
        policy.handle(Input::Idled(1));
        policy.handle(Input::Idled(2));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(true)),
            vec![StartLocker, WaitForLock]
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
        assert_eq!(policy.handle(Input::LockedHint(true)), vec![]);
        assert_eq!(policy.handle(Input::LockWaitTimedOut), vec![]);
    }

    #[test]
    fn waking_after_a_release_only_retakes_the_inhibitor_and_rearms() {
        let mut policy = policy();
        policy.handle(Input::LockedHint(true));
        policy.handle(Input::PrepareForSleep(true));
        assert_eq!(
            policy.handle(Input::PrepareForSleep(false)),
            vec![TakeSleepInhibitor, Rearm(vec![0, 1, 2])]
        );
    }
}
