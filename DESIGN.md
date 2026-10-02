# Design

rust-wl-idle-manager watches the compositor's idle notifications and its logind
session, decides what each change should lead to, and runs those commands: it
starts and unlocks the locker, spawns commands, suspends or hibernates, and
holds sleep back until the session is locked. This document records why it
works the way it does.

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
ending; and the wait for the lock timing out) and returns commands for the I/O
code to run. Its state is a set of booleans, each holding the latest value
reported by its source, never a count, so a missed or repeated input cannot
leave it skewed.

- A timeout's action runs unless the session is inactive (on another VT), or a
  logind idle inhibitor is held and the timeout is not `ignore-inhibit`.
- Its `on-resume` command runs only if the action ran. `resumed` means "no
  longer idle", which the compositor also sends when an inhibitor appears, so
  it does nothing else: it never unlocks.
- The locker is started (by `lock`, a logind lock request or sleep) unless
  `LockedHint` is set. `LockedHint` is the only "locked" state the policy
  knows: a locker that is running but has not locked yet is started again,
  and systemd refuses the duplicate (see below).
- A logind unlock request always asks the locker unit to unlock; if none is
  running, systemd says so and nothing happens.
- Before sleep, a sleep delay inhibitor holds it back: if the session is
  already locked, the inhibitor is released at once; otherwise the locker is
  started and the inhibitor is released when `LockedHint` is set or after 4
  seconds, so a broken locker never blocks sleep.
- After waking, the sleep inhibitor is released if still held and taken
  again, and every idle notification is re-created, so the timers start from
  the wake. Before that, the `on-resume` command of every timeout whose action
  ran is run, since the old notifications will never send `resumed`.
- When the last logind idle inhibitor goes away, the notifications of timeouts
  that respect inhibitors and whose action has not run are re-created, so the
  ones it held back can fire again. Timeouts whose action ran keep their
  notifications, so their `on-resume` still waits for the user's return.

## logind

The daemon follows one logind session: the user's primary session, from the
`Display` property of `/org/freedesktop/login1/user/self`. That works from a
user service, which belongs to no session itself. If the user has no primary
session, the session named by `XDG_SESSION_ID` is used instead; with neither,
the daemon exits with an error at startup.

From that session it watches the `Lock` and `Unlock` signals and the
`LockedHint` and `Active` properties, and from logind's manager the
`PrepareForSleep` signal and the `BlockInhibited` property (an idle inhibitor
is held when its colon-separated list contains `idle`). logind announces a
change to each of these properties with `PropertiesChanged`, which zbus
applies to its property cache. A zbus property stream yields the current
value first and then the value after each change, so the initial values come
from the same streams as the changes, and none is fed twice. A change may
still repeat the previous value (logind announces `BlockInhibited` whenever a
block inhibitor comes or goes), and a repeated value is harmless to the policy.

`LockedHint` is the only signal of a lock that is really in place: niri sets
it once an `ext-session-lock-v1` client has fully locked the session, not
when a locker merely starts. The compositor (or the locker) must set it;
otherwise every lock waits the full 4 seconds before sleep.

### Sleep

At startup the daemon takes a logind `sleep` inhibitor in `delay` mode. On
`PrepareForSleep(true)` it starts the locker unless the session is locked,
then releases the inhibitor as soon as `LockedHint` is set, or after 4
seconds, which is under logind's default `InhibitDelayMaxSec` of 5 seconds.
Once every delay inhibitor is released, logind goes on to sleep. On
`PrepareForSleep(false)`, after waking or a failed sleep, it takes a new
inhibitor and re-creates every idle notification.

There is at most one pending wait for the lock, and releasing the inhibitor
clears it. tokio's clock does not advance during suspend, so a wait left over
from an earlier sleep could otherwise end during the next one and release its
inhibitor early.

The inhibitor fd is close-on-exec, so no program the daemon executed could
inherit it. zbus receives file descriptors without `MSG_CMSG_CLOEXEC`, but
zvariant hands back a duplicate made with `F_DUPFD_CLOEXEC`; the original
closes with the reply message. Nothing is executed by the daemon itself
today, since commands run as systemd units, but the fd stays private either
way.

### Re-creating notifications

To restart a timeout's timer, its notification is destroyed and created again
with the same delay and the same request (`get_idle_notification` or
`get_input_idle_notification`). Events from the destroyed notification that
were read but not yet handed to the policy are dropped, and wayland-client
discards any that arrive later, so a stale `idled` or `resumed` cannot be
taken for one from the new notification.

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

To unlock, the daemon sends `SIGUSR1` to the locker unit's main process with
systemd's `KillUnit`, which is how hyprlock is asked to unlock. Only the main
process gets it, so a wrapper script must `exec` the locker. If no locker
unit is loaded, systemd answers `NoSuchUnit`, which is only logged. The
locker is never stopped, since stopping sends `SIGTERM`: under
`ext-session-lock-v1`, a locker that exits without unlocking leaves the
session locked, by design of the protocol.

`ExecStart` gets an absolute path. systemd searches only a fixed set of
directories for a bare program name, which on NixOS does not include the
usual profile directories, so the daemon resolves the name itself, much
like `systemd-run`: an absolute path is used as is, otherwise the first
executable file of that name in the daemon's own `PATH` is used. A relative
path containing `/` is an error, since it has no meaning to systemd.

Suspend, suspend-then-hibernate and hibernate are logind's `Suspend`,
`SuspendThenHibernate` and `Hibernate` methods on the system bus, called as
non-interactive, so polkit never prompts for a password.

The session and system buses are connected once at startup; failing to
connect to either, to read logind's properties, or to take the sleep
inhibitor, is a startup error. A command that fails, because its program is
not found, systemd refuses the unit, or polkit refuses the request, is logged
as an error and the daemon keeps running. Commands are run one at a time, in
the order the policy returns them, and each waits only for systemd or logind
to accept the request, not for the program to finish.

## Event loop

The daemon runs on a single-threaded tokio runtime. The Wayland connection's
file descriptor is registered with tokio through `AsyncFd`, and events are read
with wayland-client's `prepare_read` and `read`, so no second event loop or
thread is needed. D-Bus goes through zbus on the same runtime.

## Losing the compositor or the bus

When the compositor goes away, reading from the Wayland socket fails. When the
system bus connection is lost, zbus ends the signal streams. Either way the
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
