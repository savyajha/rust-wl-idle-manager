# rust-wl-idle-manager

An idle daemon for Wayland compositors, meant to replace hypridle.

> **Early work in progress.** It locks, runs commands, and suspends after
> idle timeouts, follows logind's lock, unlock and sleep requests, and
> honours logind idle inhibitors.

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
`lock` starts the `locker` command, unless the session is already locked.
`ignore-inhibit` makes a timeout count only input, ignoring idle inhibitors
such as a playing video. Unknown nodes and properties are rejected.

Besides the timeouts, it follows the user's logind session:

- `loginctl lock-session` starts the locker; `loginctl unlock-session` sends
  `SIGUSR1` to the locker unit's main process, which hyprlock takes as
  "unlock" (so a wrapper script must `exec` the locker). The locker is never
  stopped, since a lock screen that exits without unlocking leaves the
  session locked.
- Before sleep, it starts the locker and holds sleep back (with a logind
  delay inhibitor) until the session is locked, for at most 4 seconds.
- After waking, every timeout starts counting again.
- While a logind idle inhibitor is held (`systemd-inhibit --what=idle`),
  only `ignore-inhibit` timeouts fire; once it is released, the others count
  again from the release.
- While the session is not in the foreground (another VT), no timeout's
  action runs.

The session is "locked" when its logind `LockedHint` is set. The compositor
must set it once the lock is in place, as niri does; with a compositor that
does not, an idle `lock` still starts the locker, but sleep always waits the
full 4 seconds, and a lock while locked starts the locker again (systemd
refuses the duplicate).

The locker and other commands run as transient systemd user units
(`rust-wl-locker.service` and `rust-wl-idle-spawn-*.service`), so they need a
systemd user session, and a program name without a `/` is looked up in the
daemon's `PATH`. The daemon needs a logind session: the user's primary session
(as logind reports it), or the one in `XDG_SESSION_ID`; without one it exits
at startup. It also exits if logind refuses it a sleep inhibitor; logind
grants one to an unprivileged user through polkit, so polkit must be running.
Suspending and hibernating go through logind, so the user must be allowed to
do so without a password (as in a local graphical session).

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
a NixOS VM test that runs the real binary against a headless sway, in a
logind session from an autologin on a TTY, including the sleep handshake. It
needs KVM.

## License

[MIT](LICENSE)
