# NixOS VM test: the built-in lock screen, against a headless sway.
#
# The setup matches idle-lifecycle.nix (an autologin logind session, sway and the
# daemon as user services), but the config has no `locker`, so the daemon locks
# with its own ext-session-lock-v1 surfaces. sway sends `locked` as soon as it
# accepts the lock, before anything is drawn, so the test checks the screen's
# pixels with grim: the desktop is green, the lock screen's plain background
# #202428 (where its gradients leave it alone, half-way down), and sway paints an
# output red while the session is locked by a client that died. The config moves
# the widgets from the default layout: the password field's top is 320 px from the
# top, centred. Keys are typed with wtype, through the virtual-keyboard protocol.
#
# Behaviours asserted:
#   1. an idle `lock` covers the output with the lock screen; logind unlock
#      uncovers it; each lock logs its latency
#   2. logind lock and unlock, five times: locked every time, the desktop back
#      every time; latency and memory are logged
#   3. an output added while locked gets a lock surface and never shows the
#      desktop; removing it is harmless
#   4. an unlock right after a lock request, possibly before `locked`, unlocks
#      without a protocol error
#   5. a second lock client is refused by the compositor; the daemon logs it and
#      carries on
#   6. the daemon killed while locked leaves the session locked; restarted, it
#      finds LockedHint set and locks again, without any user action
#   7. sleep waits until the lock screen has locked
#   8. a wrong password shows a dot per character, then dimmed dots while
#      checking, then the field's outline in gtk.css's error colour, and stays locked;
#      the right one unlocks; Enter-to-unlocked latency is logged
#   9. Escape clears the password, and Enter with none checks nothing
#  10. caps lock shows its line under the field
#  11. the `--auth` helper exits 0 only for the right password; the daemon is not
#      dumpable; no password reaches the journal
#  12. the fifth wrong password in a row starts a 30 s cooldown: no field, a
#      countdown in its place, and Enter checks nothing; after it, the right
#      password unlocks
#  13. the clock shows text; at scale 1.5 the lock screen is prepared before the
#      lock at the scale sway then asks for
#  14. a PNG wallpaper, then a JPEG one, given by the wallpaper command and taken
#      up on a reload (SIGHUP), is the background of the next lock, blurred and
#      toned, with the gradients darkening the top
#  15. a lock while the wallpaper is still loading shows the prepared frame at
#      once, and the new wallpaper once it is ready
#  16. ten reloads in a row leave the daemon's private memory as it was

{ pkgs, idleManager }:

let
  user = "alice";
  uid = 1000;
  password = "correct horse";
  wrongPassword = "wrong guess";
  runtimeDir = "/run/user/${toString uid}";

  # Solid colours: blurring changes nothing, so each pixel is the colour, toned.
  wallpapers = pkgs.runCommand "wallpapers" { nativeBuildInputs = [ pkgs.imagemagick ]; } ''
    mkdir $out
    magick -size 64x36 xc:'#4080c0' $out/blue.png
    magick -size 64x36 xc:'#c06030' -quality 95 $out/orange.jpg
  '';

  # Prints /etc/wallpaper, after the seconds in /etc/wallpaper-delay, if any.
  wallpaperCommand = pkgs.writeShellScript "wallpaper" ''
    ${pkgs.coreutils}/bin/sleep "$(${pkgs.coreutils}/bin/cat /etc/wallpaper-delay 2>/dev/null || echo 0)"
    exec ${pkgs.coreutils}/bin/cat /etc/wallpaper
  '';

  config = pkgs.writeText "idle.kdl" ''
    timeout 5 { lock; }
    lock-screen {
        background { wallpaper-command "${wallpaperCommand}"; }
        clock {
            anchor "top"
            offset 0 40
            size 100
            weight "semibold"
        }
        name { anchor "bottom"; offset 0 40; }
        password { anchor "top"; offset 0 320; }
    }
  '';

  mkIdleManager = {
    description = "Idle manager under test";
    after = [ "sway.service" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${idleManager}/bin/rust-wl-idle-manager --config ${config}";
      ExecReload = "${pkgs.coreutils}/bin/kill -HUP $MAINPID";
      Restart = "on-failure";
      LimitCORE = 0;
      # Long enough for the test to see the session left locked by the killed daemon.
      RestartSec = 3;
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "lock-screen";

  nodes.machine = { pkgs, ... }: {
    users.users.${user} = {
      isNormalUser = true;
      inherit uid password;
    };
    services.getty.autologinUser = user;
    security.polkit.enable = true;
    # The lock screen's PAM service: NixOS's default stack (pam_unix via unix_chkpwd).
    security.pam.services.rust-wl-idle-manager = { };

    environment.systemPackages = [ pkgs.sway pkgs.grim pkgs.wtype ];
    fonts.packages = [ pkgs.adwaita-fonts ];

    systemd.user.services.sway = {
      description = "Headless sway";
      environment = {
        WLR_BACKENDS = "headless";
        WLR_RENDERER = "pixman";
        WLR_LIBINPUT_NO_DEVICES = "1";
      };
      # sway draws the solid background with swaybg, which it finds in PATH.
      path = [ pkgs.bash pkgs.swaybg ];
      serviceConfig.ExecStart = "${pkgs.sway}/bin/sway --config ${pkgs.writeText "sway-config" ''
        output * bg #00ff00 solid_color
      ''}";
    };

    systemd.user.services.idle-manager = mkIdleManager;
    # The same daemon a second time, as another lock client.
    systemd.user.services.idle-manager-second = mkIdleManager;

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    import json, re, statistics

    PREFIX = "sudo -u ${user} XDG_RUNTIME_DIR=${runtimeDir} "
    PLAIN = [0x20, 0x24, 0x28]
    DESKTOP = [0x00, 0xff, 0x00]
    ABANDONED = [0xff, 0x00, 0x00]
    WHITE = [0xff, 0xff, 0xff]
    # The error colour that gtk.css names.
    ERROR = [0xff, 0x40, 0x40]

    def over(bg, alpha, fg=WHITE):
        """`fg` at `alpha` over `bg`."""
        return [round(b * (1 - alpha) + f * alpha) for b, f in zip(bg, fg)]

    def toned(rgb):
        """A wallpaper colour after brightness 0.8 and saturation 1.1, as the daemon tones it."""
        r, g, b = [c * 0.8 for c in rgb]
        luma = 0.2126 * r + 0.7152 * g + 0.0722 * b
        return [max(0, min(255, round(luma + (c - luma) * 1.1))) for c in (r, g, b)]

    def near(a, b, tolerance=3):
        return all(abs(x - y) <= tolerance for x, y in zip(a, b))

    def uctl(cmd):
        return machine.succeed(PREFIX + cmd)

    def cursor():
        out = machine.succeed("journalctl -n 1 --show-cursor --no-pager")
        return out.strip().splitlines()[-1].removeprefix("-- cursor: ")

    def journal_cmd(since, tag):
        return f"journalctl --no-pager -o cat -t {tag} --after-cursor='{since}'"

    def journal(since, tag="rust-wl-idle-manager"):
        return machine.succeed(journal_cmd(since, tag))

    def wait_for_log(since, text, timeout=30, tag="rust-wl-idle-manager"):
        machine.wait_until_succeeds(f"{journal_cmd(since, tag)} | grep -qF '{text}'", timeout=timeout)

    def swaymsg(args):
        return uctl(f"env SWAYSOCK=$(ls ${runtimeDir}/sway-ipc.*.sock) swaymsg {args}")

    def region(x, y, w, h):
        """The colours of a rectangle of the layout, row by row, as lists of three ints."""
        size = w * h * 3
        out = uctl(f"WAYLAND_DISPLAY={display} grim -g '{x},{y} {w}x{h}' -t ppm - | tail -c {size} | od -An -tu1 -v")
        values = [int(v) for v in out.split()]
        assert len(values) == size, f"grim gave {len(values)} values, not {size}"
        return [values[i:i + 3] for i in range(0, size, 3)]

    def pixel(x, y):
        return region(x, y, 1, 1)[0]

    def wait_for_colour(check, x, y, timeout=10, what=""):
        """Wait until the colour at (x, y) passes `check`."""
        def ok(_):
            return check(pixel(x, y))
        with machine.nested(f"waiting for {what or 'a colour'} at ({x}, {y})"):
            retry(ok, timeout)

    def wait_for_pixel(want, x=None, y=None, timeout=10):
        x, y = probe if x is None else (x, y)
        wait_for_colour(lambda colour: colour == want, x, y, timeout, str(want))

    def wait_briefly_for(check, x, y, what):
        """Poll quickly, for a colour shown only for a moment."""
        for _ in range(100):
            if check(pixel(x, y)):
                return
        assert False, f"never saw {what} at ({x}, {y}); last {pixel(x, y)}"

    def marked(x, y, w, h, background):
        """How many pixels of a rectangle differ clearly from `background`: text or shapes."""
        return sum(not near(colour, background, 40) for colour in region(x, y, w, h))

    def wtype(args):
        uctl(f"WAYLAND_DISPLAY={display} wtype {args}")

    def type_file(path):
        """Type a file's text: no password appears on a command line (sudo logs those)."""
        uctl(f"sh -c 'WAYLAND_DISPLAY={display} wtype - < {path}'")

    def log_time(since, text):
        """When the first line containing `text` after `since` was logged, in seconds."""
        out = machine.succeed(f"journalctl --no-pager -o short-unix -t rust-wl-idle-manager --after-cursor='{since}'")
        return float(next(line for line in out.splitlines() if text in line).split()[0])

    def latencies(since):
        """The (drawn, locked) latencies in ms that the lock after `since` logged."""
        text = journal(since)
        ms = lambda what: float(re.findall(rf"{what} ([0-9.]+) ms after the request", text)[0])
        return ms("lock screen drawn"), ms("locked")

    def main_pid(unit="idle-manager.service"):
        return uctl(f"systemctl --user show -p MainPID --value {unit}").strip()

    def memory(pid):
        """The daemon's memory in kB: resident (/proc/<pid>/status) and proportional
        (smaps_rollup, which shares each shared page among the processes mapping it)."""
        status = machine.succeed(f"cat /proc/{pid}/status /proc/{pid}/smaps_rollup")
        fields = ["VmRSS", "RssAnon", "RssFile", "RssShmem", "Pss", "Pss_Anon", "Pss_File", "Pss_Shmem"]
        return {f: int(re.findall(rf"^{f}:\s+(\d+) kB", status, re.M)[0]) for f in fields}

    def threads(pid):
        return machine.succeed(f"cat /proc/{pid}/task/*/comm").splitlines()

    def lock(background=PLAIN):
        """Lock through logind and wait for the lock screen."""
        since = cursor()
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, f"session {session} locked")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(background)
        return since

    def unlock():
        """Unlock through logind and wait for the desktop."""
        since = cursor()
        machine.succeed(f"loginctl unlock-session {session}")
        wait_for_log(since, f"session {session} not locked")
        wait_for_pixel(DESKTOP)

    def set_locked_hint(value):
        uctl(
            f"busctl call org.freedesktop.login1 {session_path}"
            + f" org.freedesktop.login1.Session SetLockedHint b {value}"
        )

    def stats(xs):
        return {"median": statistics.median(xs), "min": min(xs), "max": max(xs), "n": len(xs)}

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@${toString uid}.service")
    machine.wait_until_succeeds("loginctl show-user ${user} -p Display --value | grep .")
    session = machine.succeed("loginctl show-user ${user} -p Display --value").strip()
    session_path = machine.succeed(
        "busctl call org.freedesktop.login1 /org/freedesktop/login1"
        + f" org.freedesktop.login1.Manager GetSession s {session}"
    ).split()[1].strip('"')
    uctl("systemctl --user start sway.service")
    machine.wait_until_succeeds("ls ${runtimeDir}/wayland-? ${runtimeDir}/sway-ipc.*.sock")
    display = machine.succeed("basename ${runtimeDir}/wayland-?").strip()
    uctl(f"systemctl --user set-environment WAYLAND_DISPLAY={display}")
    output = [o for o in json.loads(swaymsg("-t get_outputs")) if o["name"] == "HEADLESS-1"][0]["rect"]
    # Half-way down the left edge, where the gradients leave the background alone.
    probe = (output["x"] + 5, output["y"] + output["height"] // 2)
    wait_for_pixel(DESKTOP)
    # A persistent virtual keyboard, so the seat always has one (as a laptop does);
    # otherwise each wtype adds the capability and its keys race the daemon's keymap.
    uctl(f"systemd-run --user --unit=keyboard -E WAYLAND_DISPLAY={display} ${pkgs.wtype}/bin/wtype -s 3600000")
    # GTK's font and an error colour, as GTK and matugen would leave them.
    uctl("mkdir -p /home/${user}/.config/gtk-4.0")
    uctl("sh -c 'printf \"[Settings]\\ngtk-font-name=Adwaita Sans 11\\n\" > /home/${user}/.config/gtk-4.0/settings.ini'")
    uctl("sh -c 'echo \"@define-color error_color #ff4040;\" > /home/${user}/.config/gtk-4.0/gtk.css'")
    # Root writes the passwords to files, so that no command line carries them.
    machine.succeed("printf %s '${password}' > /etc/right-password")
    machine.succeed("printf %s '${wrongPassword}' > /etc/wrong-password")
    # The field is 176 × 30 with its top 320 px down: its middle dot (of an odd count),
    # a point inside it clear of the dots and text, its top edge, and the caps lock
    # line 10 px under it.
    middle = output["x"] + output["width"] // 2
    dot = (middle, output["y"] + 335)
    inside = (middle - 70, output["y"] + 335)
    edge = (middle - 70, output["y"] + 320)
    caps_line = (middle - 70, output["y"] + 358, 140, 22)
    hint = (middle - 60, output["y"] + 322, 120, 26)
    FIELD = over(PLAIN, 0.2)

    with subtest("an idle lock shows the lock screen; logind unlock removes it"):
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        wait_for_log(since, f"watching 1 timeouts from ${config} in session {session}")
        pid = main_pid()
        # Prepared before the first lock, and no wallpaper yet (/etc/wallpaper is missing).
        wait_for_log(since, "lock screen prepared for 1280×720 at scale 1 (1280×720 px) in")
        wait_for_log(since, "keeping the background as it was")
        machine.sleep(1)
        idle_memory = memory(pid)
        idle_threads = threads(pid)
        wait_for_log(since, "idle after 5 s (timeout 0)", timeout=15)
        wait_for_log(since, "lock requested from the compositor")
        wait_for_log(since, f"session {session} locked")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(PLAIN)
        assert "at lock time" not in journal(since), "the lock screen was prepared at lock time"
        machine.log(f"first lock: drawn, locked after {latencies(since)} ms")
        machine.log("startup: " + "; ".join(l for l in journal(since).splitlines() if "prepared" in l))
        unlock()

    with subtest("logind lock and unlock, five times"):
        drawn, locked, locked_memory = [], [], []
        for _ in range(5):
            since = lock()
            d, l = latencies(since)
            drawn.append(d)
            locked.append(l)
            locked_memory.append(memory(pid))
            unlock()
        unlocked_memory = memory(pid)
        summary = {
            "request to first commit (ms)": stats(drawn),
            "request to locked (ms)": stats(locked),
            "memory idle (kB)": idle_memory,
            "memory locked (kB)": locked_memory,
            "memory after unlock (kB)": unlocked_memory,
            "threads idle": idle_threads,
            "outputs": [(o["name"], o["rect"]) for o in json.loads(swaymsg("-t get_outputs"))],
        }
        machine.log("lock screen measurements: " + json.dumps(summary))

    with subtest("the clock shows text, and the layout comes from the config"):
        lock()
        # The clock's 100 px digits, 40 px from the top; the field is not shown at rest.
        assert marked(middle - 150, output["y"] + 40, 300, 120, PLAIN) > 500
        assert pixel(*inside) == PLAIN
        assert marked(*hint, PLAIN) > 50, "no hint where the field goes"
        resting = region(*hint)
        unlock()

    with subtest("an output added while locked gets a lock surface"):
        since = lock()
        swaymsg("create_output")
        new = [o for o in json.loads(swaymsg("-t get_outputs")) if o["name"] != "HEADLESS-1"][0]
        x, y = new["rect"]["x"] + 5, new["rect"]["y"] + new["rect"]["height"] // 2
        for _ in range(50):
            colour = pixel(x, y)
            assert colour != DESKTOP, "the new output showed the desktop"
            if colour == PLAIN:
                break
        assert colour == PLAIN, f"the new output shows {colour}, not the lock screen"
        swaymsg(f"output {new['name']} unplug")
        wait_for_pixel(PLAIN)
        unlock()
        assert main_pid() == pid, "the daemon restarted"

    with subtest("an unlock right after the lock request unlocks"):
        # sway sends `locked` at once, so the deferred-unlock branch is rarely
        # if ever hit here; this proves only that no protocol error occurs.
        # niri waits for drawn surfaces, where the branch is checked by hand.
        since = cursor()
        for _ in range(5):
            step = cursor()
            machine.succeed(f"loginctl lock-session {session}; loginctl unlock-session {session}")
            wait_for_log(step, f"session {session} not locked")
            wait_for_pixel(DESKTOP)
        text = journal(since)
        machine.log(f"unlock deferred until locked: {text.count('unlock deferred until locked')} of 5")
        assert main_pid() == pid, "the daemon restarted"

    with subtest("a second lock client is refused, and carries on"):
        since = lock()
        uctl("systemctl --user start idle-manager-second.service")
        wait_for_log(since, "watching 1 timeouts")
        second = main_pid("idle-manager-second.service")
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, "the compositor refused or ended our lock")
        assert main_pid("idle-manager-second.service") == second, "the second daemon restarted"
        uctl("systemctl --user stop idle-manager-second.service")
        assert pixel(*probe) == PLAIN
        unlock()

    with subtest("killed while locked, the daemon locks again on restart"):
        lock()
        # As niri does once the lock is in place; sway does not set the hint.
        set_locked_hint("true")
        since = cursor()
        machine.succeed(f"kill -9 {pid}")
        wait_for_pixel(ABANDONED)
        wait_for_log(since, f"session {session} is still locked; locking again")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(PLAIN)
        pid = main_pid()
        unlock()
        set_locked_hint("false")

    with subtest("sleep waits until the lock screen has locked"):
        since = cursor()
        machine.succeed("systemctl suspend")
        wait_for_log(since, "preparing for sleep")
        wait_for_log(since, "sleep inhibitor released")
        wait_for_log(since, "Performing sleep operation", tag="systemd-sleep")
        text = journal(since)
        machine.log(text)
        order = [
            "preparing for sleep",
            "lock requested from the compositor",
            f"session {session} locked",
            "sleep inhibitor released",
        ]
        assert sorted(order, key=text.index) == order, text
        assert "lock wait timed out" not in text, text
        wait_for_log(since, "back from sleep", timeout=60)
        wait_for_pixel(PLAIN)
        unlock()

    with subtest("a wrong password fails; the right one unlocks"):
        lock()
        assert pixel(*inside) == PLAIN
        since = cursor()
        type_file("/etc/wrong-password")
        # The frosted field, with the middle one of 11 dots, drawn into at most two
        # 1280×720 buffers however fast the keys come.
        wait_for_pixel(WHITE, *dot)
        assert memory(pid)["RssShmem"] <= 2 * 3600
        assert near(pixel(*inside), FIELD), pixel(*inside)
        wtype("-k Return")
        wait_for_log(since, "checking the password")
        # Checking: the field and its dots at half their opacity. pam_unix delays a
        # failure by about 2 s; then the failure shows for 1.5 s, without dots.
        dimmed = over(over(PLAIN, 0.1), 0.5)
        wait_briefly_for(lambda c: near(c, dimmed), *dot, "dimmed dots")
        wait_briefly_for(lambda c: c == ERROR, *edge, "the error outline")
        assert near(pixel(*dot), FIELD), "the dots were not cleared"
        wait_for_log(since, "password rejected")
        assert pixel(*probe) == PLAIN
        wait_for_pixel(PLAIN, *inside)
        assert main_pid() == pid, "the daemon restarted"
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        wait_for_log(since, "password accepted")
        wait_for_log(since, f"session {session} not locked")
        wait_for_pixel(DESKTOP)
        unlock_ms = (log_time(since, "not locked") - log_time(since, "checking the password")) * 1000
        machine.log(f"Enter to unlocked: {unlock_ms:.1f} ms")
        # The pool grew for the frames drawn while typing; the unlock replaced it.
        machine.sleep(1)
        machine.log("memory after typing and unlocking (kB): " + json.dumps(memory(pid)))

    with subtest("Escape clears the password; Enter with none checks nothing"):
        since = lock()
        wtype("abc")
        wait_for_pixel(WHITE, *dot)
        wtype("-k Escape")
        wait_for_pixel(PLAIN, *inside)
        wtype("-k Return")
        machine.sleep(1)
        assert "checking the password" not in journal(since)
        assert pixel(*inside) == PLAIN
        unlock()

    with subtest("caps lock shows its line"):
        lock()
        assert marked(*caps_line, PLAIN) == 0
        # Caps lock held (not toggled) for 3 s: wtype sends it as a depressed modifier.
        uctl(f"systemd-run --user --unit=caps-lock -E WAYLAND_DISPLAY={display} ${pkgs.wtype}/bin/wtype -M capslock -s 3000 -m capslock")
        retry(lambda _: marked(*caps_line, PLAIN) > 20, 10)
        retry(lambda _: marked(*caps_line, PLAIN) == 0, 10)
        unlock()

    with subtest("five wrong passwords in a row start a cooldown"):
        lock()
        for i in range(5):
            since = cursor()
            type_file("/etc/wrong-password")
            wtype("-k Return")
            wait_for_log(since, "password rejected")
        wait_for_log(since, "5 wrong passwords; waiting 30 s")
        started = log_time(since, "5 wrong passwords")
        # A countdown where the field goes: it changes from one second to the next.
        wait_for_pixel(PLAIN, *inside)
        first = region(*hint)
        machine.sleep(2)
        assert region(*hint) != first, "the countdown did not count"
        # Typing does nothing: no field, and Enter checks nothing.
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        machine.sleep(1)
        assert "checking the password" not in journal(since)
        assert pixel(*inside) == PLAIN
        # Still counting 28–29 s in: the cooldown really lasts about 30 s, not just "a while".
        machine.sleep(max(0, int(started + 28 - float(machine.succeed("date +%s.%N"))) + 1))
        assert region(*hint) != resting, "the cooldown ended before 28 s"
        retry(lambda _: region(*hint) == resting, 40)
        ended = float(machine.succeed("date +%s.%N"))
        machine.log(f"cooldown over after at most {ended - started:.1f} s")
        assert ended - started >= 30
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        wait_for_log(since, "password accepted")
        wait_for_pixel(DESKTOP)

    with subtest("at scale 1.5, the lock screen is prepared before the lock"):
        since = cursor()
        swaymsg("output HEADLESS-1 scale 1.5")
        wait_for_log(since, "lock screen prepared for 853×480 at scale 1.5 (1280×720 px) in")
        # The output is 480 px high now.
        probe = (output["x"] + 5, output["y"] + 240)
        lock()
        text = journal(since)
        assert "at lock time" not in text, text
        machine.log(f"scale 1.5: drawn, locked after {latencies(since)} ms")
        # Drawn pixel for pixel: the middle of three dots is white in the output's own
        # pixels, where a buffer of the wrong size, scaled, would blur it. The field's
        # top is at 480 of 720 px, its middle 22.5 px below, in the middle of the width.
        wtype("abc")
        at = len("P6\n1280 720\n255\n") + (502 * 1280 + 640) * 3
        def dot_is_white(_):
            uctl(f"WAYLAND_DISPLAY={display} grim -o HEADLESS-1 -t ppm /home/${user}/frame.ppm")
            return machine.succeed(f"od -An -tu1 -j {at} -N 3 /home/${user}/frame.ppm").split() == ["255"] * 3
        retry(dot_is_white, 10)
        wtype("-k Escape")
        unlock()
        since = cursor()
        swaymsg("output HEADLESS-1 scale 1")
        wait_for_log(since, "lock screen prepared for 1280×720 at scale 1 (1280×720 px) in")
        probe = (output["x"] + 5, output["y"] + output["height"] // 2)

    with subtest("a wallpaper, then another, taken up on SIGHUP"):
        for path, colour in [("${wallpapers}/blue.png", [0x40, 0x80, 0xc0]), ("${wallpapers}/orange.jpg", [0xc0, 0x60, 0x30])]:
            since = cursor()
            machine.succeed(f"echo {path} > /etc/wallpaper")
            uctl("systemctl --user reload idle-manager.service")
            wait_for_log(since, f"wallpaper {path} ready in")
            want = toned(colour)
            machine.succeed(f"loginctl lock-session {session}")
            wait_for_log(since, "lock screen drawn")
            wait_for_colour(lambda c: near(c, want), *probe, what=f"the wallpaper {want}")
            # The top gradient darkens it.
            top = pixel(probe[0], output["y"] + 5)
            assert all(t <= w * 0.75 for t, w in zip(top, want)), f"{top} is not darker than {want}"
            uctl(f"WAYLAND_DISPLAY={display} grim /home/${user}/lock-screen-{path[-3:]}.png")
            unlock()
        assert main_pid() == pid, "the daemon restarted"
        machine.copy_from_machine("/home/${user}/lock-screen-jpg.png")

    with subtest("a lock while the wallpaper loads shows the prepared frame, then the wallpaper"):
        machine.succeed("echo 3 > /etc/wallpaper-delay; echo ${wallpapers}/blue.png > /etc/wallpaper")
        since = cursor()
        uctl("systemctl --user reload idle-manager.service")
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(toned([0xc0, 0x60, 0x30]))
        assert "ready in" not in journal(since), "the wallpaper was ready before the lock"
        assert "at lock time" not in journal(since)
        machine.log(f"locked while loading: drawn, locked after {latencies(since)} ms")
        wait_for_colour(lambda c: near(c, toned([0x40, 0x80, 0xc0])), *probe, what="the new wallpaper")
        unlock()
        machine.succeed("rm /etc/wallpaper-delay")

    with subtest("ten reloads leave the private memory as it was"):
        # Held off the 5 s idle lock, which would draw frames of its own.
        since = cursor()
        uctl("systemd-run --user --unit=no-idle systemd-inhibit --what=idle sleep 60")
        wait_for_log(since, "logind idle inhibitor held")
        before = memory(pid)
        for _ in range(10):
            since = cursor()
            uctl("systemctl --user reload idle-manager.service")
            wait_for_log(since, "wallpaper ${wallpapers}/blue.png ready in")
            wait_for_log(since, "lock screen prepared for")
        machine.sleep(1)
        after = memory(pid)
        machine.log(f"memory before ten reloads: {json.dumps(before)}; after: {json.dumps(after)}")
        assert after["RssAnon"] - before["RssAnon"] < 1024, (before, after)
        assert after["RssShmem"] == before["RssShmem"], (before, after)
        uctl("systemctl --user stop no-idle.service")

    with subtest("the helper checks the password; the daemon keeps it private"):
        helper = "${idleManager}/bin/rust-wl-idle-manager --auth"
        uctl(f"sh -c '{helper} < /etc/right-password'")
        machine.fail(PREFIX + f"sh -c '{helper} < /etc/wrong-password'")
        machine.fail(PREFIX + f"sh -c '{helper} < /dev/null'")
        # Not dumpable: the daemon's /proc files belong to root.
        machine.fail(PREFIX + f"ls /proc/{pid}/fd")
        machine.succeed(f"ls /proc/{pid}/fd")
        machine.fail("journalctl --no-pager -o cat | grep -F -e '${password}' -e '${wrongPassword}'")
  '';
}
