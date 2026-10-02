# rust-wl-idle-manager

An idle daemon for Wayland compositors, meant to replace hypridle.

> **Early work in progress.** It currently only detects idleness and logs it.
> It does not lock, run commands, or suspend yet.

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
`ignore-inhibit` makes a timeout count only input, ignoring idle inhibitors
such as a playing video. Unknown nodes and properties are rejected.

## Logs and exit status

rust-wl-idle-manager logs to journald, or to stderr when journald is
unavailable:

```sh
journalctl --user -t rust-wl-idle-manager
```

It exits with 0 on SIGTERM, 1 on an error (including losing the connection to
the compositor), and 2 on a command-line usage error.

## Testing

```sh
nix flake check
```

This runs `cargo fmt --check`, `cargo clippy -D warnings`, the unit tests, and
a NixOS VM test that runs the real binary against a headless sway. It needs
KVM.

## License

[MIT](LICENSE)
