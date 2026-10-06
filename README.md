# rust-wl-idle-manager

An idle daemon for Wayland compositors, meant to replace hypridle.

> **Early work in progress.** It locks, runs commands, and suspends after
> idle timeouts, follows logind's lock, unlock and sleep requests, and
> honours logind idle inhibitors. Its built-in lock screen shows the blurred
> wallpaper, the date and time, and your name, and unlocks with your password.

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

`locker` names the program that locks the session; a config with a `locker`
cannot also have a `lock-screen` block. Without it, the daemon locks with its
own lock screen, through `ext-session-lock-v1`, which the
compositor must support. The built-in lock screen covers every output with
the blurred wallpaper, the date and a large clock at the top, and your
initial, name and a password field at the bottom: type your password and
press Enter (Backspace deletes a character, Escape clears it). The field shows
a dot per character, dims while the password is checked, and is outlined in
the error colour when it was wrong; "Caps Lock is on" shows under it. After
five wrong passwords in a row, it says "Try again in N s" and takes no input
for 30 s. Only the right password or `loginctl unlock-session` unlocks it. If the daemon dies
while locked, the session stays locked, and on its next start the daemon locks
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

### The lock screen's look

Without a `lock-screen` block, the lock screen is the default below, which has
no wallpaper (a plain dark background). A `lock-screen` block lists the
widgets to draw, and only those. This is the default, plus a wallpaper command,
with the defaults of the other `background`, `avatar` and `password`
properties in comments:

```kdl
lock-screen {
    background {
        // A command that prints the wallpaper's path (PNG or JPEG) on its first line;
        // without one, or until it has worked once, the plain `color`.
        wallpaper-command "sh" "-c" "awww query | grep -o '/.*'"
        // blur 24              // radius, in the wallpaper's pixels
        // brightness 0.8
        // saturation 1.1
        // gradient 0.35 0.45   // the dark gradients at the top and the bottom
        // color "#202428"
    }
    date {
        anchor "top"
        offset 0 64
        size 26
        weight "semibold"
        color "#ffffff" 0.88
        format "%A %-d %B"
    }
    clock {
        anchor "top"
        offset 0 80
        size 152
        weight "semibold"
        letter-spacing -5
        color "#ffffff" 0.82
        format "%H:%M"
    }
    avatar {
        anchor "bottom"
        offset 0 149
        // diameter 64
        // color "#ffffff"
    }
    name {
        anchor "bottom"
        offset 0 121
        weight "semibold"
    }
    password {
        anchor "bottom"
        offset 0 56
        // width 176; height 30; radius 15; dot-size 4
        // color "#ffffff"; error-color "error_color"
    }
}
```

- `anchor` is one of `top-left`, `top`, `top-right`, `left`, `center` (the
  default), `right`, `bottom-left`, `bottom` and `bottom-right`; `offset x y`
  moves the widget away from the edges it is anchored to (inwards), or right
  and down along a centred axis, in logical pixels. A text widget's box is its
  line of text; the password widget's is the field with the caps lock line
  under it.
- Text uses GTK's default font (`gtk-font-name` in
  `~/.config/gtk-4.0/settings.ini`), at `size` logical pixels (default 15),
  `weight` (`thin`, `ultralight`, `light`, `semilight`, `book`, `normal`, the
  default, `medium`, `semibold`, `bold`, `ultrabold`, `heavy`, `ultraheavy`)
  and `letter-spacing`. `format` is strftime-like (GLib's
  `g_date_time_format`), shown as of each minute; `name` shows the full name
  from the passwd database, or the login name.
- A colour is `#rrggbb`, `#rrggbbaa`, or the name of one of GTK's colours
  defined in `~/.config/gtk-4.0/gtk.css` (`@define-color <name> #hex;`, as
  matugen writes them), optionally followed by an opacity it is multiplied by.
  A name gtk.css does not define (it changes with the wallpaper) is logged and
  shows white.
- Every widget is optional, except `password`: without it nothing could
  unlock. Each output is drawn at its own scale, including fractional ones.

The config is checked when it is loaded, and the daemon does not start with a
bad one; each error says where it is. Besides unknown or repeated widgets and
properties and values of the wrong type:

| Property | Allowed |
|---|---|
| `size`, `diameter`, `height` | 1 to 1000 |
| `width` | 1 to 4000 |
| `radius` | 0 to 500 |
| `dot-size` | 1 to 100 |
| `letter-spacing` | −100 to 100 |
| `offset` | −10000 to 10000, each |
| `blur` | 0 to 500 |
| `brightness`, `saturation` | 0 to 10 |
| `gradient`, a colour's opacity | 0 to 1 |
| a colour | `#` and 6 or 8 hex digits, or a name of letters, digits, `_` and `-` |
| `format` | one GLib can use |
| `wallpaper-command`, `locker`, `spawn`, `on-resume` | a program, then any arguments |

A text too long for its size is cut off at 8192 pixels.

The wallpaper command runs (for at most 5 seconds), and gtk.css is read, at
startup and on a reload (SIGHUP), so a wallpaper or theme change can tell the
daemon:

```sh
systemctl --user reload rust-wl-idle-manager
```

with `ExecReload=kill -HUP $MAINPID` in the unit (see below). Sending the
signal some other way, it must reach only the main process (`systemctl --user
kill --kill-whom=main -s HUP rust-wl-idle-manager`): the daemon's children are
the wallpaper command and the password check, which a SIGHUP would end. A
reload while the wallpaper is still loading loads it once more afterwards; a
wallpaper that fails to load leaves the last one in place. Nothing is loaded at
lock time, so a lock shows the last wallpaper the daemon was told about.

### logind

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
# Reload the wallpaper and GTK's colours (systemctl --user reload); on NixOS, give
# kill's full path, such as ${pkgs.coreutils}/bin/kill.
ExecReload=kill -HUP $MAINPID
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
added, after the daemon is killed while locked, at scale 1.5, and with a PNG
and then a JPEG wallpaper taken up on SIGHUP, and types right and wrong
passwords with `wtype`. They need KVM.

## License

[MIT](LICENSE)
