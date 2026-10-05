# NixOS VM test: the built-in lock screen, against a headless sway.
#
# The setup matches idle-lifecycle.nix (an autologin logind session, sway and the
# daemon as user services), but the config has no `locker`, so the daemon locks
# with its own ext-session-lock-v1 surfaces. sway sends `locked` as soon as it
# accepts the lock, before anything is drawn, so the test checks the screen's
# pixels with grim: the desktop is green, the lock screen #203040, and sway paints
# an output red while the session is locked by a client that died. Keys are typed
# with wtype, through the virtual-keyboard protocol.
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
#   8. a wrong password shows "checking", then "failed", and stays locked; the
#      right one unlocks; Enter-to-unlocked latency is logged
#   9. Escape clears the password, and Enter with none checks nothing
#  10. caps lock shows its bar
#  11. the `--auth` helper exits 0 only for the right password; the daemon is not
#      dumpable; no password reaches the journal
#  12. the fifth wrong password in a row starts a 30 s cooldown: the field turns
#      grey and Enter checks nothing; after it, the right password unlocks

{ pkgs, idleManager }:

let
  user = "alice";
  uid = 1000;
  password = "correct horse";
  wrongPassword = "wrong guess";
  runtimeDir = "/run/user/${toString uid}";

  config = pkgs.writeText "idle.kdl" ''
    timeout 5 { lock; }
  '';

  mkIdleManager = {
    description = "Idle manager under test";
    after = [ "sway.service" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${idleManager}/bin/rust-wl-idle-manager --config ${config}";
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
    LOCK = ["20", "30", "40"]
    DESKTOP = ["00", "ff", "00"]
    ABANDONED = ["ff", "00", "00"]
    FIELD = ["30", "48", "60"]
    DOT = ["e0", "e8", "f0"]
    CHECKING = ["30", "70", "c0"]
    FAILED = ["c0", "30", "30"]
    CAPS_LOCK = ["e0", "a0", "20"]
    COOLDOWN = ["58", "58", "58"]

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

    def pixel(x, y):
        """The colour at (x, y) in the layout, as three hex bytes."""
        return uctl(f"WAYLAND_DISPLAY={display} grim -g '{x},{y} 1x1' -t ppm - | tail -c 3 | od -An -tx1").split()

    def wait_for_pixel(want, x=5, y=5, timeout=10):
        machine.wait_until_succeeds(
            PREFIX + f"WAYLAND_DISPLAY={display} grim -g '{x},{y} 1x1' -t ppm - | tail -c 3"
            + f" | od -An -tx1 | grep -qx ' {' '.join(want)}'",
            timeout=timeout,
        )

    def wait_briefly_for_pixel(want, x, y):
        """Poll quickly, for a colour shown only for a moment."""
        for _ in range(100):
            if pixel(x, y) == want:
                return
        assert False, f"never saw {want} at ({x}, {y}); last {pixel(x, y)}"

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
        """The daemon's resident memory in kB, from /proc/<pid>/status."""
        status = machine.succeed(f"cat /proc/{pid}/status")
        fields = ["VmRSS", "RssAnon", "RssFile", "RssShmem"]
        return {f: int(re.findall(rf"^{f}:\s+(\d+) kB", status, re.M)[0]) for f in fields}

    def lock():
        """Lock through logind and wait for the lock screen."""
        since = cursor()
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, f"session {session} locked")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(LOCK)
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
    wait_for_pixel(DESKTOP)
    # A persistent virtual keyboard, so the seat always has one (as a laptop does);
    # otherwise each wtype adds the capability and its keys race the daemon's keymap.
    uctl(f"systemd-run --user --unit=keyboard -E WAYLAND_DISPLAY={display} ${pkgs.wtype}/bin/wtype -s 3600000")
    # Root writes the passwords to files, so that no command line carries them.
    machine.succeed("printf %s '${password}' > /etc/right-password")
    machine.succeed("printf %s '${wrongPassword}' > /etc/wrong-password")
    output = [o for o in json.loads(swaymsg("-t get_outputs")) if o["name"] == "HEADLESS-1"][0]["rect"]
    # The password field's centre (the middle dot of an odd count), and its caps lock bar.
    centre = (output["x"] + output["width"] // 2, output["y"] + output["height"] // 2)
    caps_bar = (centre[0], centre[1] + 25 + 10 + 3)

    with subtest("an idle lock shows the lock screen; logind unlock removes it"):
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        wait_for_log(since, f"watching 1 timeouts from ${config} in session {session}")
        pid = main_pid()
        # Once the outputs are known and the pool is prepared, before the idle lock.
        machine.sleep(1)
        idle_memory = memory(pid)
        wait_for_log(since, "idle after 5 s (timeout 0)", timeout=15)
        wait_for_log(since, "lock requested from the compositor")
        wait_for_log(since, f"session {session} locked")
        wait_for_log(since, "lock screen drawn")
        wait_for_pixel(LOCK)
        machine.log(f"first lock: drawn, locked after {latencies(since)} ms")
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
            "outputs": [(o["name"], o["rect"]) for o in json.loads(swaymsg("-t get_outputs"))],
        }
        machine.log("lock screen measurements: " + json.dumps(summary))

    with subtest("an output added while locked gets a lock surface"):
        since = lock()
        swaymsg("create_output")
        new = [o for o in json.loads(swaymsg("-t get_outputs")) if o["name"] != "HEADLESS-1"][0]
        x = new["rect"]["x"] + 5
        for _ in range(50):
            colour = pixel(x, 5)
            assert colour != DESKTOP, "the new output showed the desktop"
            if colour == LOCK:
                break
        assert colour == LOCK, f"the new output shows {colour}, not the lock screen"
        swaymsg(f"output {new['name']} unplug")
        wait_for_pixel(LOCK)
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
        assert pixel(5, 5) == LOCK
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
        wait_for_pixel(LOCK)
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
        wait_for_pixel(LOCK)
        unlock()

    with subtest("a wrong password fails; the right one unlocks"):
        lock()
        wait_for_pixel(FIELD, *centre)
        since = cursor()
        type_file("/etc/wrong-password")
        wait_for_pixel(DOT, *centre)
        wtype("-k Return")
        wait_for_log(since, "checking the password")
        # pam_unix delays a failure by about 2 s; then the failure shows for 1.5 s.
        wait_briefly_for_pixel(CHECKING, *centre)
        wait_briefly_for_pixel(FAILED, *centre)
        wait_for_log(since, "password rejected")
        assert pixel(5, 5) == LOCK
        wait_for_pixel(FIELD, *centre)
        assert main_pid() == pid, "the daemon restarted"
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        wait_for_log(since, "password accepted")
        wait_for_log(since, f"session {session} not locked")
        wait_for_pixel(DESKTOP)
        unlock_ms = (log_time(since, "not locked") - log_time(since, "checking the password")) * 1000
        machine.log(f"Enter to unlocked: {unlock_ms:.1f} ms")

    with subtest("Escape clears the password; Enter with none checks nothing"):
        since = lock()
        wtype("abc")
        wait_for_pixel(DOT, *centre)
        wtype("-k Escape")
        wait_for_pixel(FIELD, *centre)
        wtype("-k Return")
        machine.sleep(1)
        assert "checking the password" not in journal(since)
        assert pixel(*centre) == FIELD
        unlock()

    with subtest("caps lock shows its bar"):
        lock()
        assert pixel(*caps_bar) == LOCK
        # Caps lock held (not toggled) for 3 s: wtype sends it as a depressed modifier.
        uctl(f"systemd-run --user --unit=caps-lock -E WAYLAND_DISPLAY={display} ${pkgs.wtype}/bin/wtype -M capslock -s 3000 -m capslock")
        wait_for_pixel(CAPS_LOCK, *caps_bar)
        wait_for_pixel(LOCK, *caps_bar)
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
        wait_for_pixel(COOLDOWN, *centre)
        # Typing does nothing: no dots, and Enter checks nothing.
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        machine.sleep(1)
        assert "checking the password" not in journal(since)
        assert pixel(*centre) == COOLDOWN
        # Still grey 28–29 s in: the cooldown really lasts about 30 s, not just "a while".
        machine.sleep(max(0, int(started + 28 - float(machine.succeed("date +%s.%N"))) + 1))
        assert pixel(*centre) == COOLDOWN, "the cooldown ended before 28 s"
        wait_for_pixel(FIELD, *centre, timeout=40)
        ended = float(machine.succeed("date +%s.%N"))
        machine.log(f"cooldown over after at most {ended - started:.1f} s")
        assert ended - started >= 30
        since = cursor()
        type_file("/etc/right-password")
        wtype("-k Return")
        wait_for_log(since, "password accepted")
        wait_for_pixel(DESKTOP)

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
