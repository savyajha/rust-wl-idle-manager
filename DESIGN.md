# Design

rust-wl-idle-manager watches the compositor's idle notifications, decides what
each one should lead to, and runs those commands: it starts the locker, spawns
commands, and suspends or hibernates. This document records why it works the
way it does.

## Idle detection

Idleness comes from the Wayland `ext-idle-notify-v1` protocol. Each configured
timeout gets its own notification, created with the timeout's delay, so the
compositor does the timing and sends `idled` and `resumed` for each one
separately. Events are reported by the timeout's position in the config.
The notifications use the first `wl_seat`; niri has a single seat.

By default a notification is created with `get_idle_notification`, which
respects idle inhibitors: while a client inhibits idling (e.g. a playing
video), the timeout does not fire. A timeout with `ignore-inhibit` uses
`get_input_idle_notification` instead, which counts only input. That request
needs version 2 of `ext_idle_notifier_v1`; with an older compositor, a config
that uses `ignore-inhibit` is an error at startup.

## Policy

What to do is decided by a pure state machine, `Policy`, with no I/O, timers or
clock, so every rule is unit-tested. It takes inputs (a timeout idled or
resumed; from logind, whether an idle inhibitor is held, whether the session is
active, lock and unlock requests, the session's `LockedHint`, sleep starting or
ending; whether the locker unit runs; and the wait for the lock timing out) and
returns commands for the I/O code to run. Today only the Wayland inputs are fed
in. Its state is a set of booleans, each holding the latest value reported by
its source, never a count, so a missed or repeated input cannot leave it skewed.

- A timeout's action runs unless the session is inactive (on another VT), or a
  logind idle inhibitor is held and the timeout is not `ignore-inhibit`.
- Its `on-resume` command runs only if the action ran. `resumed` means "no
  longer idle", which the compositor also sends when an inhibitor appears, so
  it does nothing else: it never unlocks.
- The locker is started (by `lock`, a logind lock request or sleep) only if it
  is not already running and `LockedHint` is not set.
- A logind unlock request unlocks the locker only while our locker unit is
  running.
- Before sleep, a sleep delay inhibitor holds it back: if the session is
  already locked, the inhibitor is released at once; otherwise the locker is
  started and the inhibitor is released when `LockedHint` is set or after a
  timeout of about 4 seconds, so a broken locker never blocks sleep.
- After waking, the sleep inhibitor is released if still held and taken
  again, and every idle notification is re-created, so the timers start from
  the wake. Before that, the `on-resume` command of every timeout whose action
  ran is run, since the old notifications will never send `resumed`.
- When the last logind idle inhibitor goes away, the notifications of timeouts
  that respect inhibitors and whose action has not run are re-created, so the
  ones it held back can fire again. Timeouts whose action ran keep their
  notifications, so their `on-resume` still waits for the user's return.

## Running commands

The locker and spawned commands run as transient systemd user units, started
with the user manager's `StartTransientUnit` D-Bus method rather than forked
from the daemon. Each gets its own unit and journal entries, inherits the user
manager's environment (including `WAYLAND_DISPLAY`), and leaves no child
process behind. The locker runs as `rust-wl-locker.service`; each spawn runs as
`rust-wl-idle-spawn-<pid>-<n>.service`, from the daemon's process ID and a
counter, so the names do not collide; if one ever does, the spawn is logged as
an error and not run.

Units are started with job mode `fail`. A lock while `rust-wl-locker.service`
is still loaded is refused by systemd with `UnitExists`; the daemon logs
"locker already running" and carries on, so systemd itself prevents a second
locker. Each unit is created with `CollectMode=inactive-or-failed`, which
unloads it as soon as it stops, even if it failed. Without it, a locker that
crashed would stay loaded as a failed unit and every later lock would be
refused.

`ExecStart` gets an absolute path. systemd searches only a fixed set of
directories for a bare program name, which on NixOS does not include the
usual profile directories, so the daemon resolves the name itself, much
like `systemd-run`: a name containing `/` is used as is, otherwise the first
executable file of that name in the daemon's own `PATH` is used.

Suspend, suspend-then-hibernate and hibernate are logind's `Suspend`,
`SuspendThenHibernate` and `Hibernate` methods on the system bus, called as
non-interactive, so polkit never prompts for a password.

The session and system buses are connected once at startup; failing to
connect to either is a startup error. A command that fails, because its
program is not found, systemd refuses the unit, or polkit refuses the
request, is logged as an error and the daemon keeps running. Commands are
run one at a time, in the order the policy returns them, and each waits only
for systemd or logind to accept the request, not for the program to finish.

## Event loop

The daemon runs on a single-threaded tokio runtime. The Wayland connection's
file descriptor is registered with tokio through `AsyncFd`, and events are read
with wayland-client's `prepare_read` and `read`, so no second event loop or
thread is needed. D-Bus goes through zbus on the same runtime.

## Losing the compositor

When the compositor goes away, reading from the Wayland socket fails. The
daemon logs the error and exits with status 1, so a service manager can
restart it. It does not try to reconnect.

## Logging

rust-wl-idle-manager logs to journald, falling back to stderr when journald is
unavailable. Values are interpolated into the message text rather than attached
as structured fields, because `journalctl` shows only the message by default.

## Exit status

rust-wl-idle-manager exits with 0 on SIGTERM and 1 on any error. Each error is
logged once; on compositor loss, wayland-backend also prints its own line to
stderr, which reaches the journal. Command-line usage errors exit with 2.
