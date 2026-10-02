# rust-wl-idle-manager

An idle daemon for Wayland compositors, meant to replace hypridle.

> **Early work in progress.** It locks, runs commands, and suspends after
> idle timeouts. It does not yet react to logind lock, unlock or sleep
> requests, or to logind idle inhibitors.

> **Please read before using**
>
> - This is a personal project, built for my own desktop. It is shared in case
>   it is useful to others, with no promise of support or stability.
> - It was written with the help of an AI assistant (Anthropic's Claude). Every
>   change was reviewed by me, and the behaviour is covered by the tests
>   described below, but you should review it yourself before relying on it.
> - It is intended for [niri](https://github.com/YaLTeR/niri), but so far it
>   has only been tested with sway, in a NixOS VM test. It may work with other
>   compositors that implement `ext-idle-notify-v1`.

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
`lock` starts the `locker` command, unless it is already running.
`ignore-inhibit` makes a timeout count only input, ignoring idle inhibitors
such as a playing video. Unknown nodes and properties are rejected.

The locker and other commands run as transient systemd user units
(`rust-wl-locker.service` and `rust-wl-idle-spawn-*.service`), so they need a
systemd user session, and a program name without a `/` is looked up in the
daemon's `PATH`. Suspending and hibernating go through logind, so the user
must be allowed to do so without a password (as in a local graphical
session).

## Logs and exit status

rust-wl-idle-manager logs to journald, or to stderr when journald is
unavailable:

```sh
journalctl --user -t rust-wl-idle-manager
```

It exits with 0 on SIGTERM, 1 on an error (including losing the connection to
the compositor), and 2 on a command-line usage error. A command that fails,
such as a program that is not found, is logged and does not stop the daemon.

## Testing

```sh
nix flake check
```

This runs `cargo fmt --check`, `cargo clippy -D warnings`, the unit tests, and
a NixOS VM test that runs the real binary against a headless sway. It needs
KVM.

## License

[MIT](LICENSE)
