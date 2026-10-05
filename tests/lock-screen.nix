# NixOS VM test: the built-in lock screen, against a headless sway.
#
# The setup matches idle-lifecycle.nix (an autologin logind session, sway and the
# daemon as user services), but the config has no `locker`, so the daemon locks
# with its own ext-session-lock-v1 surfaces. sway sends `locked` as soon as it
# accepts the lock, before anything is drawn, so the test checks the screen's
# pixels with grim: the desktop is green, the lock screen #203040, and sway paints
# an output red while the session is locked by a client that died.
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

{ pkgs, idleManager }:

let
  user = "alice";
  uid = 1000;
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
      inherit uid;
    };
    services.getty.autologinUser = user;
    security.polkit.enable = true;

    environment.systemPackages = [ pkgs.sway pkgs.grim ];

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

    def wait_for_pixel(want, x=5, y=5):
        machine.wait_until_succeeds(
            PREFIX + f"WAYLAND_DISPLAY={display} grim -g '{x},{y} 1x1' -t ppm - | tail -c 3"
            + f" | od -An -tx1 | grep -qx ' {' '.join(want)}'",
            timeout=10,
        )

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
  '';
}
