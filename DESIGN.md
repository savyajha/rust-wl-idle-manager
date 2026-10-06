# Design

rust-wl-idle-manager watches the compositor's idle notifications and its logind
session, decides what each change should lead to, and runs those commands: it
locks and unlocks the session, spawns commands, suspends or hibernates, and
holds sleep back until the session is locked. It locks with its own lock
screen, or, if the config names a `locker`, by starting that program. This
document records why it works the way it does.

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

What to do is decided by a pure state machine, `Policy`, with no I/O, timers
or clock, so every rule is unit-tested. It takes inputs (a timeout idled or
resumed; from logind, whether an idle inhibitor is held, whether the session
is active, lock and unlock requests, whether the lid is closed, sleep starting
or ending; whether the session is locked; and the wait for the lock timing
out) and returns commands for the I/O code to run. Its state is a set of
booleans, each holding the latest value reported by its source, never a count,
so a missed or repeated input cannot leave it skewed.

- A timeout's action runs unless the session is inactive (on another VT), or a
  logind idle inhibitor is held and the timeout is not `ignore-inhibit`.
- Its `on-resume` command runs only if the action ran. `resumed` means "no
  longer idle", which the compositor also sends when an inhibitor appears, so
  it does nothing else: it never unlocks.
- The session is locked (by `lock`, a logind lock request, the lid closing,
  or sleep) unless it is already locked. "Locked" comes from the built-in lock
  screen's `locked` event, or with a `locker`, from `LockedHint`; it is the only
  lock state the policy knows. A lock that is under way but not yet locked is
  requested again, which does nothing: the lock screen already has a lock
  pending, and systemd refuses a second locker (see below).
- The lid locks when it closes, not when it is already closed at startup.
- A logind unlock request always unlocks; when nothing is locked, nothing
  happens.
- Before sleep, a sleep delay inhibitor holds it back: if the session is
  already locked, the inhibitor is released at once; otherwise the session is
  locked and the inhibitor is released once it is, or after 4 seconds, so a
  broken locker never blocks sleep. While the session is inactive, it is
  locked but the inhibitor is released at once: a session in the background
  cannot show its lock promptly.
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
`PrepareForSleep` signal and the `BlockInhibited` (an idle inhibitor is held
when its colon-separated list contains `idle`) and `LidClosed` properties.
logind announces a change to each of these properties with
`PropertiesChanged`, which zbus applies to its property cache. A zbus property
stream yields the current value first and then the value after each change, so
the initial values come from the same streams as the changes, and none is fed
twice. A change may still repeat the previous value (logind announces
`BlockInhibited` whenever a block inhibitor comes or goes), and a repeated
value is harmless to the policy.

With a `locker`, `LockedHint` is the only signal of a lock that is really in
place: niri sets it once an `ext-session-lock-v1` client has fully locked the
session, not when a locker merely starts. The compositor (or the locker) must
set it; otherwise every lock waits the full 4 seconds before sleep. The
built-in lock screen gets the compositor's `locked` event directly, and reads
`LockedHint` only at startup (see below).

The lid closing locks only while logind's `Docked` is false (no dock and at
most one display), as logind itself ignores the lid while docked
(`HandleLidSwitchDocked`). logind announces no change of `Docked`, so it is
read afresh each time the lid closes, a D-Bus round trip before that lock. A
failed read counts as not docked, so the session locks: the lid closing may
be about to suspend the machine, and exiting then could let it sleep unlocked.

### Sleep

At startup the daemon takes a logind `sleep` inhibitor in `delay` mode. On
`PrepareForSleep(true)` it locks unless the session is locked, then releases
the inhibitor as soon as the session is locked, or after 4 seconds, which is
under logind's default `InhibitDelayMaxSec` of 5 seconds. Once every delay
inhibitor is released, logind goes on to sleep. On `PrepareForSleep(false)`,
after waking or a failed sleep, it takes a new inhibitor and re-creates every
idle notification.

There is at most one pending wait for the lock, and releasing the inhibitor
clears it. tokio's clock does not advance during suspend, so a wait left over
from an earlier sleep could otherwise end during the next one and release its
inhibitor early.

The inhibitor fd is close-on-exec, so no program the daemon executes can
inherit it. zbus receives file descriptors without `MSG_CMSG_CLOEXEC`, but
zvariant hands back a duplicate made with `F_DUPFD_CLOEXEC`; the original
closes with the reply message. Until then, the daemon's own children (the
authentication helper and the wallpaper command; other commands run as
systemd units) could inherit that original, so between fork and exec they
mark every fd above stderr close-on-exec (`close_range(3, ~0,
CLOSE_RANGE_CLOEXEC)`, which keeps the pipe on which std reports a failed
exec). This is best effort: where `close_range` fails (kernels before 5.11, or
a seccomp filter), the child is started anyway rather than leaving the
password unchecked.

### Re-creating notifications

To restart a timeout's timer, its notification is destroyed and created again
with the same delay and the same request (`get_idle_notification` or
`get_input_idle_notification`). Events from the destroyed notification that
were read but not yet handed to the policy are dropped, and wayland-client
discards any that arrive later, so a stale `idled` or `resumed` cannot be
taken for one from the new notification.

## The built-in lock screen

Without a `locker`, the daemon locks the session itself with
`ext-session-lock-v1`, on the same Wayland connection and event queue as the
idle notifications. Locking is a few requests on an open connection, with no
process to start and no D-Bus round trip, so the compositor can show the lock
within milliseconds. smithay-client-toolkit tracks the outputs and
provides the session lock and shared-memory buffers.

A lock request creates the lock and one lock surface per output. When the
compositor configures a surface with its size, it is drawn and committed (see
"Drawing" below). Buffers come from one shared-memory pool. A buffer is
destroyed once the compositor releases it. Each lock logs the time from the
request to the first committed surface and to the compositor's `locked`
event; both are measured while dispatching and logged after the requests are
flushed, so logging never delays a commit.

The `locked` event tells the policy the session is locked. niri sends it once
every output shows a lock surface; sway sends it as soon as it accepts the
lock. The lock ends with `unlock_and_destroy` and the surfaces are destroyed,
and the policy hears the session is unlocked. Only two things unlock it: the
right password (the helper exiting with 0) and a logind unlock request. Not
`resumed`, a signal, or the daemon dying. Either way the policy turns it into
the same unlock command. If it comes before `locked`, the lock ends as soon
as `locked` arrives, since destroying a lock the compositor has already
locked is a protocol error.

The compositor sends `finished` when it refuses the lock, for instance
because another client holds a live lock, or ends it. The daemon logs it,
destroys the lock, and carries on as unlocked.

An output added while locked gets a lock surface as soon as the compositor
announces it; until then the compositor shows nothing on it. A removed
output's surface is destroyed.

If the daemon dies while locked, the compositor keeps the session locked
(niri blanks the outputs, sway paints them red). At startup, if `LockedHint`
is set, the daemon locks at once; niri and sway let a new lock replace the
dead one. This relies on the compositor setting `LockedHint` while locked:
niri does, and keeps it set after the lock client dies; sway does not, so
there the session stays behind the dead lock until the next lock. With
`Restart=on-failure` in the unit, a crash on niri ends in a short blank
screen and a new lock, never an unlock.

Without a `locker`, a compositor that lacks `ext-session-lock-v1` is an error
at startup.

### Keyboard and password field

The seat is bound once, at most at version 9, and serves both the idle
notifications and the keyboard. With the built-in lock screen, the first
keyboard the seat offers is taken, and handled directly: its keymap is
compiled by xkbcommon (mapped read-only from the compositor's fd), and its
modifiers update the xkbcommon state. Keys act only while a lock exists: the
lock surfaces are the daemon's only surfaces, so it never sees keys meant for
anything else. Enter submits the password, Backspace deletes one character
and Escape clears it, each recognised by its keysym. Any other key types the
character xkbcommon gives for it as UTF-32 (`key_get_utf32`), so any layout
works, unless it is a control character (as with Ctrl held) or Alt or Logo is
held. There is no compose table, so a dead key types nothing. While a
password is being checked, keys do nothing, and an empty password is never
submitted.

Backspace and characters repeat at the compositor's rate and delay. Binding
the seat below version 10 keeps the compositor from repeating keys itself;
the entry keeps its own timers instead, and `Wayland::next` waits on the
nearest of them alongside the Wayland socket: the next key repeat, the end of
a failure display or of a cooldown, and forgetting the password. A repeated
character is the password's last character typed again, so no copy of it is
kept for repeating.

What the lock screen shows of the entry is its `Look`: the state, the number
of characters, and caps lock. At rest it shows "Enter Password"; while typing,
a field with a dot per character (as many as fit); while the password is
checked, the field and its dots at half their opacity (the entry keeps the
number of characters submitted, never the characters, since the password is
wiped as soon as the helper has it); for 1.5 s after a failure (or until the
next key), the field without dots, outlined in the error colour; during a
cooldown, "Try again in N s" in place of the field, counting down each second.
"Caps Lock is on" shows under the field whenever caps lock is. Each time
`Wayland::next` goes round, it compares the look with what is shown, and
redraws every surface, in full, only if it changed. The entry's state machine,
`Entry`, does no I/O apart from one log line when a cooldown starts; the time
is passed in, and it is unit-tested. The password itself is kept by a type of
its own, `Password` (the buffer described under "Protecting the password"),
which holds all of its unsafe code.

### Drawing

The lock screen is drawn with cairo, and its text with Pango (through
pangocairo), the text stack GTK uses, without GTK: the same font, optical size
(Adwaita Sans is a variable font with a `wght` and an `opsz` axis, which Pango
sets from the size), fallback and shaping as GTK applications. The font is
GTK's default, `gtk-font-name` in `$XDG_CONFIG_HOME/gtk-4.0/settings.ini` (by
default "Sans"), at the size and weight each widget asks for. Colours are
literal `#rrggbb[aa]` or GTK's named colours from `gtk-4.0/gtk.css`: only its
`@define-color <name> #hex;` lines are read (matugen writes them), and a
missing file or name falls back to built-in defaults, logged. Colours are
parsed once, when the config is loaded, and names are checked each time
gtk.css is read (at startup and on a reload): each one it lacks is logged then,
once, and shows white.

The layout comes from the config's `lock-screen` block: a widget each for the
background, date, clock, avatar, name and password field, each placed at one
of nine anchors of the output and moved by an offset in logical pixels, away
from the edges it is anchored to. Without the block, the default
(`DEFAULT_LOCK_SCREEN` in config.rs) is the look of the design it was made
from. A widget's box is its text's logical box (as Pango lays it out), the
avatar's circle, or the password field with the caps lock line under it.

Everything a frame needs is prepared ahead of time, per output, in a `Scene`:
the background at a quarter of the output's physical size (the blurred
wallpaper scaled to cover it, or a plain colour, with a dark gradient at the
top and the bottom), and every text rendered at the output's scale and placed:
the date, clock and name, the initial, "Enter Password", "⇪ Caps Lock is on"
and the arrow "→" (the two symbols are text too, in GTK's font or its
fallback). Drawing a frame paints into a shared-memory buffer through a cairo
surface over it: the background scaled up with bilinear filtering (about 4 ms
at 2560 × 1600), the texts composited, and a few shapes filled: the frosted
circle, field and button (white at a low opacity over the blurred background,
with a one-pixel inner outline) and the dots.

While unlocked, each output's first frame (at rest, as a lock begins) is also
drawn ahead of time, into a buffer of its own in the pool, kept. A lock
attaches that buffer and commits: no drawing at all between the request and
the first commit. If the compositor still holds that buffer from the last
lock, or it shows something else (caps lock, say), the first frame is drawn
then instead. The log says which: "lock screen drawn (attached)" or
"(painted)", with the time since the request. The frame is drawn again
whenever its scene changes: at startup, when an output appears or changes, on
a reload, and each minute the clock or date changes.

Each output has at most two buffers: the one the compositor shows and one to
draw the next frame in, each remembered with what it shows, so that one that
already shows the frame wanted is attached again without drawing. When the
compositor holds both, the output waits: its release of one is an event, which
wakes the loop, and the latest look is drawn then. So typing never takes more
than two frames of memory per output, and a key shows within a frame or two
(about 1.2 ms from the key event to the commit of its frame in the VM). After
an unlock, the pool, which grew to two frames per output (and cannot shrink),
is replaced by a new one holding just the next lock's first frames.

No font is loaded, no glyph rasterised, no image decoded and no file read on
the way from a lock request to its first commit. Rendering while locked happens
only for: an output that appears during a lock, or a lock surface whose
preferred scale is not the expected one (both logged "at lock time"); a reload
while locked (the lock screen is prepared again and redrawn); the restart
re-lock (L9), which may lock before the outputs are known; each new minute of
the clock; and the countdown, once a second during a cooldown (a cooldown
never starts with a lock). A fade-in and a shake after a wrong password are
not done (yet).

If preparing an output's lock screen or drawing a frame fails, the error is
logged and the frame is the background's plain colour, so a lock never shows
nothing. If rendering the clock, the date or the countdown fails later, the
error is logged and the old text stays. Rendering fails only when the
environment does (say, a broken fontconfig): the config is checked when it is loaded, so that a config that
loads can always be drawn: every number has a range (sizes up to 1000 logical
pixels, the field up to 4000 wide), colours, formats and commands are checked,
and a text longer than 8192 pixels is cut off.

The clock is checked after each flush, so a new minute is never rendered
between a lock request and its first commit; a timer wakes the loop at the
next minute. The check runs once the minute changes, and compares the
formatted text with what is shown, so it also catches up at once after a
suspend, when tokio's timers are late.

#### The wallpaper

The `background` widget's `wallpaper-command` prints the wallpaper's path, on
its first non-empty line. It runs at startup (once the sleep inhibitor is held) and on a reload
(SIGHUP), as a child process, for at most 5 seconds. A reload while one is
running loads the wallpaper once more after it. The file's header is read
first, and an image over 64 megapixels is refused. It is decoded (PNG by
cairo, JPEG by zune-jpeg, straight into cairo's pixel layout), scaled down to a
quarter by cairo (which averages the pixels it replaces), blurred with three
box blurs (approximating a Gaussian, with the configured radius divided by
four), and toned (brightness, then saturation, as CSS's filters do), on one of
tokio's blocking threads: for a 2560 × 1440 PNG, decoding alone takes about 120
ms, which must not hold up the event loop. Only the small result is kept (640
× 360 × 4 bytes for that wallpaper); every output's lock screen is then
prepared again from it. If the command fails or its file cannot be decoded,
the error is logged and the background stays as it was (at startup, the plain
colour). The home-manager unit that follows the wallpaper reloads the daemon
(`systemctl --user reload rust-wl-idle-manager`), which also rereads gtk.css,
so matugen's new colours arrive with the new wallpaper.

#### Scale

Each output is drawn at its real scale. With `wp-fractional-scale-v1` and
`wp-viewporter` (niri and sway have both), each lock surface gets a viewport
whose destination is its logical size, and a buffer of logical size × scale,
rounded as the protocol asks, except that where that is within a pixel of the
output's mode, the mode's size is used (niri reports a 2560 px panel at 1.5×
as 1707 logical pixels, and 1707 × 1.5 rounds to 2561). Without them, the
output's integer scale is the buffer scale. The compositor says the scale it
prefers only once a surface exists, so until it has, the lock screen is
prepared at the scale it will most likely prefer: the current mode's width
over the logical width, to the nearest 1/120. The scale it does prefer is
kept for later locks. Text and shapes are rendered at the physical size, and
placed on whole physical pixels.

#### Memory

Per output, the pool holds the prepared first frame (W × H × 4 bytes of shared
memory: 16 MB for a 2560 × 1600 panel at 1.5×, 3.7 MB for 1280 × 720 at 1×),
the scene holds the background at a sixteenth of that (1 MB at 2560 × 1600)
and its texts (a few hundred kB at 1.5×, the clock being the largest), and the
blurred wallpaper takes another megabyte or so. While locked, the pool grows
to two frames per output, and is replaced at unlock.
Pango, fontconfig, cairo and GLib add about 3 MB of private memory (more with
many fonts installed), and their libraries' pages, which are shared with
every GTK application on the system. Decoding a wallpaper briefly takes tens
of megabytes, which are handed back to the system afterwards (`malloc_trim`).
With a `locker`, none of this is allocated: no outputs are tracked, no font
map is created, and there is no buffer pool, session lock, password buffer
or keymap.

### Checking the password

On Enter, the daemon starts its helper, `rust-wl-idle-manager --auth` (from
`/proc/self/exe`, so it is the same binary even if the file has been replaced),
writes the password to the helper's stdin pipe and closes it. The helper
reads it into a fixed buffer, runs PAM's `pam_authenticate` for the user it
runs as, with the service `rust-wl-idle-manager`, wipes the buffer, and
exits with 0 on success and 1 otherwise. The PAM conversation answers the
first password prompt with the password and aborts on a second one, as
swaylock does: pam_systemd_home asks again itself after a wrong password, and
there is no second password to give. Like GDM's reauthentication, a
failing `pam_acct_mgmt` (such as an expired password) is logged but does not
fail the check, so the lock screen can never lock its user out. pam_unix
already delays a failure by about 2 s, so there is no extra delay.

The daemon unlocks only on the exit status of its own child. It waits for
it at most 10 s, then kills it and counts a failure. One check runs at a
time; a new one replaces (and kills) one left from a lock that has since
ended, and an answer that arrives after its lock ended is ignored.

After every fifth failed attempt in a row, a 30 s cooldown follows, in
which keys do nothing and no password is taken. The count is kept only in
`Entry`'s memory: it resets on a right password, on any unlock and when a
lock starts, and is never saved. With pam_unix's delay, guessing runs at
about 30 attempts a minute; the cooldown slows sustained guessing without
any way to lock the user out. `pam_faillock` is deliberately not used:
with a lock screen, it can lock the account so that the lock screen itself
cannot unlock it, and recovering needs a TTY and root. The cooldown's
deadline uses the same clock as the entry's other timers, `Instant`
(`CLOCK_MONOTONIC`), which stops during suspend, so a cooldown that spans a
suspend lasts longer; that is harmless.

PAM runs in a separate process for each attempt, rather than on a thread of
the daemon, because a PAM conversation blocks and PAM modules keep their
own copies of the password; all of it dies with the helper. It costs about
3 ms per attempt (process start and loading libpam), next to the
`unix_chkpwd` process pam_unix starts anyway. The binary links
libpam, so it is mapped in the daemon too, but only the helper calls it: PAM
is reached only from `--auth`, which `main` handles before the daemon's
runtime is even built.

### Protecting the password

The typed password necessarily lives in the daemon, which receives the
keystrokes. It is handled like this:

- It is kept in one 1 KiB buffer, page-aligned, allocated once at startup and
  never moved or grown. Typing past its end is ignored.
- The buffer is locked in memory with `mlock`, so it is never swapped out.
  If that is refused (`RLIMIT_MEMLOCK`), a warning is logged once and the
  buffer is used anyway.
- It is wiped, with volatile writes the compiler cannot remove, as soon as
  the helper has its copy (the password is handed out in a guard that wipes
  it when dropped, on every path), after every answer, on Escape, on unlock,
  when a lock starts, after 30 s without typing, and before sleep, since the
  machine may go on to hibernate. The sleep wipe is attached to releasing the
  sleep inhibitor: every `PrepareForSleep(true)` path ends in that release
  (the policy's tests prove it), and sleep cannot start before it.
- A typed character goes from xkbcommon (`key_get_utf32`) straight into the
  buffer, never through a `String`.
- At startup the daemon and the helper make themselves non-dumpable
  (`prctl(PR_SET_DUMPABLE, 0)`): no core dumps, and other processes of the
  user cannot ptrace them or read their memory; their `/proc/PID` files
  belong to root. The unit should also set `LimitCORE=0`.
- Neither the buffer nor a key is `Debug`, and nothing derived from the
  password is logged or put in an error.
- The helper gets the password only on its stdin pipe, never in its
  arguments or environment, and reads it unbuffered into its own fixed
  buffer, which it wipes before exiting normally. A panic or a kill ends it
  without that wipe; its memory goes with the process.

This does not protect against root or the kernel, against anything that can
read the keystrokes on their way through the compositor and the Wayland
socket, or against memory written to a hibernation image before the wipe
before sleep. Raw keycodes stay in wayland-backend's receive buffers until
overwritten (swaylock shares this limit), and the copy nonstick makes for
PAM in the helper is not wiped.

## Running commands

A configured locker and spawned commands run as transient systemd user units,
started with the user manager's `StartTransientUnit` D-Bus method rather than
forked from the daemon. Each gets its own unit and journal entries, inherits
the user manager's environment (including `WAYLAND_DISPLAY`), and leaves no
child process behind. The locker runs as `rust-wl-locker.service`; each spawn
runs as `rust-wl-idle-spawn-<pid>-<n>.service`, from the daemon's process ID
and a counter, so the names do not collide; if one ever does, the spawn is
logged as an error and not run.

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

The daemon runs on a single-threaded tokio runtime. Its one Wayland
connection's file descriptor is registered with tokio through `AsyncFd`, and
events are read with wayland-client's `prepare_read` and `read`, so no second
event loop is needed. While it waits for the socket it also waits for the lock
screen's nearest timer (key repeat and the like) and the clock's next minute.
The only other threads are GLib's own and, while a wallpaper is being decoded,
one of tokio's blocking threads; cairo and Pango objects stay on the main
thread, and the decoding thread hands back plain pixels. The
authentication helper is a tokio child process, awaited in the main loop.
Requests are flushed after every dispatch and right after a lock request, so
a lock never waits for other work. D-Bus goes
through zbus on the same runtime. Inputs from the Wayland queue are taken
before those from D-Bus: an unlock queues the lock screen's `Locked(false)`
as it runs, and it must reach the policy before a lock request, which the
policy would otherwise ignore as already locked.

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
The `--auth` helper exits with 0 for the right password and 1 otherwise.
