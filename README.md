# rust-wl-idle-manager

An idle daemon for Wayland compositors, meant to replace hypridle.

> **Early work in progress.** It locks, runs commands, and suspends after
> idle timeouts, follows logind's lock, unlock and sleep requests, and
> honours logind idle inhibitors. Its built-in lock screen unlocks with your
> password, but is still plain: no clock, wallpaper or text yet.

> **Please read before using**
>
> - This is a personal project, built for my own desktop. It is shared in case
>   it is useful to others, with no promise of support or stability.
> - It was written with the help of an AI assistant (Anthropic's Claude). Every
>   change was reviewed by me, and the behaviour is covered by the tests
>   described below, but you should review it yourself before relying on it.
> - It is built for and used with [niri](https://github.com/YaLTeR/niri) on
>   NixOS. The automated tests run it against sway in a NixOS VM. It may work
>   with other compositors that implement `ext-idle-notify-v1` and set logind's
>   `LockedHint` when locked.

## Configuration

rust-wl-idle-manager takes exactly one argument, the path to a
[KDL](https://kdl.dev) config file:

```sh
rust-wl-idle-manager --config /path/to/config.kdl
```

```kdl
locker "hyprlock"
timeout 300 { lock; }
timeout 600 {
    ignore-inhibit
    spawn "niri" "msg" "action" "power-off-monitors"
    on-resume "niri" "msg" "action" "power-on-monitors"
}
timeout 1200 { suspend-then-hibernate; }
```

Each `timeout` takes its delay in seconds and exactly one action: `lock`,
`suspend`, `suspend-then-hibernate`, `hibernate`, or `spawn` with a command.
`lock` locks the session, unless it is already locked.
`ignore-inhibit` makes a timeout count only input, ignoring idle inhibitors
such as a playing video. Unknown nodes and properties are rejected.

`locker` names the program that locks the session. Without it, the daemon
locks with its own lock screen, through `ext-session-lock-v1`, which the
compositor must support. The built-in lock screen covers every output with a
plain colour and a password field: type your password and press Enter
(Backspace deletes a character, Escape clears it). The field shows a dot per
character, turns blue while the password is checked and red when it was
wrong, and a yellow bar below it means caps lock is on. Only the right
password or `loginctl unlock-session` unlocks it. If the daemon dies while
locked, the session stays locked, and on its next start the daemon locks
again (the compositor lets the new lock replace the dead one), so the unit
should restart it on failure.

The password is checked with PAM, as the PAM service `rust-wl-idle-manager`,
by a short-lived helper process (the same binary, run as
`rust-wl-idle-manager --auth`). The service must exist. On NixOS, add this to
the **system** configuration:

```nix
security.pam.services.rust-wl-idle-manager = { };
```

Elsewhere, create `/etc/pam.d/rust-wl-idle-manager`, for instance with
`auth include login`. Layouts that need dead keys or compose sequences to type
the password's characters are not supported yet.

Besides the timeouts, it follows the user's logind session:

- `loginctl lock-session` locks; `loginctl unlock-session` unlocks. With a
  `locker`, unlocking sends `SIGUSR1` to the locker unit's main process,
  which hyprlock takes as "unlock" (so a wrapper script must `exec` the
  locker). The locker is never stopped, since a lock screen that exits
  without unlocking leaves the session locked.
- Closing the lid locks, whether or not logind then suspends, except while
  logind reports the system docked or with more than one display (its
  `Docked` property), when logind ignores the lid too.
- Before sleep, it locks and holds sleep back (with a logind delay
  inhibitor) until the session is locked, for at most 4 seconds. While the
  session is not in the foreground, it locks without holding sleep back.
- After waking, every timeout starts counting again.
- While a logind idle inhibitor is held (`systemd-inhibit --what=idle`),
  only `ignore-inhibit` timeouts fire; once it is released, the others count
  again from the release.
- While the session is not in the foreground (another VT), no timeout's
  action runs.

With a `locker`, the session is "locked" when its logind `LockedHint` is set.
The compositor must set it once the lock is in place, as niri does; with a
compositor that does not, an idle `lock` still starts the locker, but sleep
always waits the full 4 seconds, and a lock while locked starts the locker
again (systemd refuses the duplicate). The built-in lock screen learns it is
locked from the compositor itself, and reads `LockedHint` only at startup.

The locker and other commands run as transient systemd user units
(`rust-wl-locker.service` and `rust-wl-idle-spawn-*.service`), so they need a
systemd user session, and a program name without a `/` is looked up in the
daemon's `PATH`. The daemon needs a logind session: the user's primary session
(as logind reports it), or the one in `XDG_SESSION_ID`; without one it exits
at startup. It also exits if logind refuses it a sleep inhibitor; logind
grants one to an unprivileged user through polkit, so polkit must be running.
Suspending and hibernating go through logind, so the user must be allowed to
do so without a password (as in a local graphical session).

## Running it

As a systemd user unit, started with the graphical session:

```ini
[Unit]
Description=Idle manager
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=/path/to/rust-wl-idle-manager --config %h/.config/rust-wl-idle-manager/config.kdl
Restart=on-failure
# No core dumps: the daemon holds the password being typed.
LimitCORE=0

[Install]
WantedBy=graphical-session.target
```

The daemon also makes itself non-dumpable at startup, and keeps the typed
password in a buffer locked in memory, which it wipes after each attempt;
DESIGN.md has the details and their limits.

## Logs and exit status

rust-wl-idle-manager logs to journald, or to stderr when journald is
unavailable:

```sh
journalctl --user -t rust-wl-idle-manager
```

It exits with 0 on SIGTERM, 1 on an error (including losing the connection to
the compositor or the system bus, or finding no logind session), and 2 on a
command-line usage error. A command that fails, such as a program that is not
found, is logged and does not stop the daemon.

## Testing

```sh
nix flake check
```

This runs `cargo fmt --check`, `cargo clippy -D warnings`, the unit tests, and
two NixOS VM tests that run the real binary against a headless sway, in a
logind session from an autologin on a TTY: one with an external locker,
including the sleep handshake, and one with the built-in lock screen, which
checks the screen's pixels while locked, after unlocking, after an output is
added, and after the daemon is killed while locked, and types right and
wrong passwords with `wtype`. They need KVM.

## License

[MIT](LICENSE)
