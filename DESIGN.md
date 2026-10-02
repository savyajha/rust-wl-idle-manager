# Design

rust-wl-idle-manager watches the compositor's idle notifications, decides what
each one should lead to and, for now, logs those commands without running them.
This document records why it works the way it does.

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

## Event loop

The daemon runs on a single-threaded tokio runtime. The Wayland connection's
file descriptor is registered with tokio through `AsyncFd`, and events are read
with wayland-client's `prepare_read` and `read`, so no second event loop or
thread is needed.

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
