# NixOS VM test: the real binary against a headless sway, in a real logind session.
#
# alice is logged in on tty1 by getty's autologin, which gives her a logind
# session and a user manager. sway runs headless as a user service and provides
# ext-idle-notify-v1. rust-wl-idle-manager runs as a user service connected to
# it, finds alice's session through logind, and the test reads its log lines
# from the journal. sway does not set LockedHint, so the test locker sets it
# itself, and clears it when SIGUSR1 asks it to unlock.
#
# Behaviours asserted:
#   1. each timeout idles once, after its own delay, in order, and its
#      command runs: the locker as a transient user unit, the spawns (found
#      in PATH) as transient units that are unloaded once they exit
#   2. input resumes every idle timeout, and the on-resume command runs; one
#      that is not found is logged as an error
#   3. after a resume, the timers start again; an idle lock while the session
#      is locked starts nothing
#   4. logind unlock and lock requests send the locker SIGUSR1 and start it;
#      a locker that fails is unloaded at once; an unlock with no locker
#      running is only logged
#   5. a Wayland idle inhibitor holds back every timeout but the
#      ignore-inhibit one
#   6. a logind idle inhibitor does the same; once it is released, the
#      timeouts it held back fire one full delay later, and the one that ran
#      is left alone
#   7. while the session is inactive (another VT), no idle action runs
#   8. sleep: the daemon holds a close-on-exec sleep delay inhibitor; before
#      sleep it starts the locker and releases the inhibitor once the session
#      is locked, before logind goes on; after the (failed) sleep it takes the
#      inhibitor again and restarts the timers
#   9. a second locker is refused by systemd; a locker that never locks holds
#      sleep back for only 4 s
#  10. the compositor going away -> exit with a failure status
#  11. SIGTERM -> clean exit 0
#  12. an invalid config -> exit 1 with a readable error
#  13. a usage error -> exit 2

{ pkgs, idleManager }:

let
  user = "alice";
  uid = 1000;
  runtimeDir = "/run/user/${toString uid}";
  busctl = "${pkgs.systemd}/bin/busctl";
  sleep = "${pkgs.coreutils}/bin/sleep";

  # Sets LockedHint on its own session, as niri does once the lock is drawn, and
  # clears it on SIGUSR1. `wait` returns on a signal, so the trap runs at once.
  locker = pkgs.writeShellScript "test-locker" ''
    read -r _ _ session < <(${busctl} get-property org.freedesktop.login1 \
      /org/freedesktop/login1/user/self org.freedesktop.login1.User Display)
    session=''${session//\"/}
    hint() {
      ${busctl} call org.freedesktop.login1 "$session" org.freedesktop.login1.Session \
        SetLockedHint b "$1"
    }
    trap 'hint false; exit 0' USR1
    hint true
    while true; do ${sleep} 1 & wait $!; done
  '';

  # A locker that never locks, so sleep must not wait for it for long.
  brokenLocker = pkgs.writeShellScript "test-locker-broken" ''
    trap 'exit 0' USR1
    while true; do ${sleep} 1 & wait $!; done
  '';

  # The spawns write markers to /tmp.
  config = pkgs.writeText "idle.kdl" ''
    locker "${locker}"
    timeout 3 { lock; on-resume "no-such-program"; }
    timeout 6 { spawn "touch" "/tmp/spawned"; on-resume "touch" "/tmp/resumed"; }
    timeout 9 { ignore-inhibit; spawn "touch" "/tmp/ignored-inhibit"; }
  '';

  brokenLockerConfig = pkgs.writeText "idle-broken-locker.kdl" ''
    locker "${brokenLocker}"
    timeout 300 { lock; }
  '';

  badConfig = pkgs.writeText "idle-bad.kdl" ''
    locker "true"
    timeout 3 { lock; }
    timout 6 { lock; }
  '';

  mkIdleManager = configFile: {
    description = "Idle manager under test";
    after = [ "sway.service" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${idleManager}/bin/rust-wl-idle-manager --config ${configFile}";
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "idle-lifecycle";

  nodes.machine = { pkgs, ... }: {
    users.users.${user} = {
      isNormalUser = true;
      inherit uid;
    };
    # A real logind session on tty1, which also starts alice's user manager.
    services.getty.autologinUser = user;
    # logind asks polkit whether alice may take inhibitors; a desktop has it anyway.
    security.polkit.enable = true;

    environment.systemPackages = [ pkgs.sway ];
    # foot holds the idle inhibitor; it needs a font to start.
    fonts.packages = [ pkgs.dejavu_fonts ];

    # An empty config: no bar, no exec, nothing but the compositor.
    systemd.user.services.sway = {
      description = "Headless sway";
      environment = {
        WLR_BACKENDS = "headless";
        WLR_RENDERER = "pixman";
        WLR_LIBINPUT_NO_DEVICES = "1";
      };
      # sway runs `exec` commands through `sh` from PATH.
      path = [ pkgs.bash ];
      serviceConfig.ExecStart = "${pkgs.sway}/bin/sway --config ${pkgs.writeText "sway-config" ""}";
    };

    systemd.user.services.idle-manager = mkIdleManager config;
    systemd.user.services.idle-manager-broken-locker = mkIdleManager brokenLockerConfig;
    systemd.user.services.idle-manager-bad = mkIdleManager badConfig;

    virtualisation.memorySize = 1024;
    # QEMU's guest has no working S3 (virtio-pci refuses it), so suspend fails at once.
    # Only "deep": systemd would otherwise fall back to s2idle, which can freeze the guest.
    systemd.sleep.settings.Sleep = {
      SuspendState = "mem";
      MemorySleepMode = "deep";
    };
  };

  testScript = ''
    PREFIX = "sudo -u ${user} XDG_RUNTIME_DIR=${runtimeDir} "

    def uctl(cmd):
        return machine.succeed(PREFIX + cmd)

    def cursor():
        out = machine.succeed("journalctl -n 1 --show-cursor --no-pager")
        return out.strip().splitlines()[-1].removeprefix("-- cursor: ")

    def journal_cmd(since, tag):
        """The journal of `tag` after cursor `since`, with Unix timestamps."""
        return f"journalctl --no-pager -o short-unix -t {tag} --after-cursor='{since}'"

    def journal(since, tag="rust-wl-idle-manager"):
        return machine.succeed(journal_cmd(since, tag))

    def wait_for_log(since, text, timeout=30, tag="rust-wl-idle-manager"):
        """The timestamp of the first line of `tag` containing `text` after cursor `since`."""
        machine.wait_until_succeeds(f"{journal_cmd(since, tag)} | grep -qF '{text}'", timeout=timeout)
        for line in journal(since, tag).splitlines():
            if text in line:
                return float(line.split()[0])

    def swaymsg(args):
        return uctl(f"env SWAYSOCK=$(ls ${runtimeDir}/sway-ipc.*.sock) swaymsg {args}")

    def now():
        return float(machine.succeed("date +%s.%N"))

    def start_sway():
        uctl("systemctl --user start sway.service")
        machine.wait_until_succeeds("ls ${runtimeDir}/wayland-? ${runtimeDir}/sway-ipc.*.sock")
        display = machine.succeed("basename ${runtimeDir}/wayland-?").strip()
        uctl(f"systemctl --user set-environment WAYLAND_DISPLAY={display}")

    def exit_status(unit):
        return uctl(f"systemctl --user show -p ExecMainStatus --value {unit}").strip()

    def locked_hint():
        return machine.succeed(f"loginctl show-session {session} -p LockedHint --value").strip()

    def unlock():
        """Ask logind to unlock, and wait until the locker has unlocked and exited."""
        since = cursor()
        machine.succeed(f"loginctl unlock-session {session}")
        wait_for_log(since, f"session {session} not locked")
        wait_until_unloaded("rust-wl-locker.service")

    def wait_until_unloaded(unit):
        machine.wait_until_succeeds(
            PREFIX + f"systemctl --user show -p LoadState --value {unit} | grep -qx not-found"
        )

    def our_inhibitors():
        """Our lines in logind's inhibitor list."""
        out = machine.succeed("systemd-inhibit --list --no-legend")
        return [line for line in out.splitlines() if "rust-wl-idle-manager" in line]

    def suspend():
        """Ask logind to suspend; the suspend fails in this VM, after the full handshake."""
        since = cursor()
        machine.log("/sys/power: " + machine.succeed("cat /sys/power/state /sys/power/mem_sleep").replace("\n", "; "))
        machine.succeed("systemctl suspend")
        return since

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@${toString uid}.service")
    # The autologin session on tty1 is alice's primary session, which the daemon finds.
    machine.wait_until_succeeds("loginctl show-user ${user} -p Display --value | grep .")
    session = machine.succeed("loginctl show-user ${user} -p Display --value").strip()
    machine.log(machine.succeed(f"loginctl show-session {session}"))
    session_path = machine.succeed(
        "busctl call org.freedesktop.login1 /org/freedesktop/login1"
        + f" org.freedesktop.login1.Manager GetSession s {session}"
    ).split()[1].strip('"')
    start_sway()

    with subtest("each timeout idles once, in order"):
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        t0 = wait_for_log(since, f"watching 3 timeouts from ${config} in session {session}")
        pid = uctl("systemctl --user show -p MainPID --value idle-manager.service").strip()
        status = machine.succeed(f"cat /proc/{pid}/status").splitlines()
        fields = ("VmRSS", "RssAnon", "RssFile", "RssShmem")
        memory = [" ".join(line.split()) for line in status if line.split(":")[0] in fields]
        machine.log("memory idle with a locker: " + "; ".join(memory))
        t = [
            wait_for_log(since, f"idle after {secs} s (timeout {i})")
            for i, secs in enumerate([3, 6, 9])
        ]
        machine.log(f"idle after {[round(x - t0, 3) for x in t]} s")
        for want, got in zip([3, 6, 9], t):
            assert want - 0.5 < got - t0 < want + 1.5, f"timeout {want}: idled after {got - t0} s"
        assert t[0] < t[1] < t[2], f"out of order: {t}"
        text = journal(since)
        assert text.count("idle after 3 s (timeout 0)") == 1, text
        assert text.count("starting the locker") == 1, text
        uctl("systemctl --user is-active rust-wl-locker.service")
        wait_for_log(since, f"session {session} locked")
        assert locked_hint() == "yes"
        machine.wait_for_file("/tmp/spawned")
        machine.wait_for_file("/tmp/ignored-inhibit")
        spawn_units = PREFIX + "systemctl --user list-units --all --no-legend 'rust-wl-idle-spawn-*'"
        # Fails, and is retried, while systemctl fails or lists a unit.
        machine.wait_until_succeeds(f'units=$({spawn_units}) && test -z "$units"')

    with subtest("input resumes every timeout"):
        since = cursor()
        sent = now()
        swaymsg("seat - cursor move 10 10")
        resumed = [
            wait_for_log(since, f"resumed (timeout {i})")
            for i in range(3)
        ]
        machine.log(f"resumed after {[round(x - sent, 3) for x in resumed]} s")
        assert all(x - sent < 2 for x in resumed), f"slow resume: {resumed} vs {sent}"
        machine.wait_for_file("/tmp/resumed")
        wait_for_log(since, "spawning no-such-program: no-such-program not found in PATH")

    with subtest("after a resume, the timers start again; while locked, a lock starts nothing"):
        again = wait_for_log(since, "idle after 3 s (timeout 0)")
        machine.log(f"idle again {round(again - sent, 3)} s after the input")
        assert 2.5 < again - sent < 4.5, f"re-idled after {again - sent} s"
        wait_for_log(since, "idle after 9 s (timeout 2)")
        text = journal(since)
        assert "starting the locker" not in text, text
        uctl("systemctl --user is-active idle-manager.service")

    with subtest("logind unlock and lock requests"):
        since = cursor()
        unlock()
        wait_for_log(since, f"unlock requested for session {session}")
        wait_for_log(since, "unlocking the locker")
        assert locked_hint() == "no"

        since = cursor()
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, f"lock requested for session {session}")
        wait_for_log(since, "starting the locker")
        wait_for_log(since, f"session {session} locked")
        assert locked_hint() == "yes"

        # A killed locker fails; CollectMode=inactive-or-failed must still unload it.
        uctl("systemctl --user kill -s KILL rust-wl-locker.service")
        wait_until_unloaded("rust-wl-locker.service")
        # The hint stays set, as a compositor keeps the session locked when its locker dies.
        machine.succeed(f"loginctl unlock-session {session}")
        wait_for_log(since, "no locker running to unlock")
        uctl(
            f"busctl call org.freedesktop.login1 {session_path}"
            + " org.freedesktop.login1.Session SetLockedHint b false"
        )
        wait_for_log(since, f"session {session} not locked")
        uctl("systemctl --user is-active idle-manager.service")

    with subtest("a Wayland idle inhibitor holds back all but the ignore-inhibit timeout"):
        swaymsg("exec ${pkgs.foot}/bin/foot")
        machine.wait_until_succeeds(
            PREFIX + "env SWAYSOCK=$(ls ${runtimeDir}/sway-ipc.*.sock) swaymsg -t get_tree"
            + " | grep -q '\"app_id\": \"foot\"'",
            timeout=30,
        )
        swaymsg("'[app_id=foot] inhibit_idle open'")
        since = cursor()
        sent = now()
        swaymsg("seat - cursor move 20 20")
        idled = wait_for_log(since, "idle after 9 s (timeout 2)")
        machine.log(f"timeout 2 idled {round(idled - sent, 3)} s after the input")
        assert 8.5 < idled - sent < 11, f"timeout 2 idled after {idled - sent} s"
        text = journal(since)
        machine.log(text)
        for line in ["idle after 3 s (timeout 0)", "idle after 6 s (timeout 1)"]:
            assert line not in text, f"inhibited, yet logged {line!r}:\n{text}"
        # Written again since the input, not left over from an earlier idle.
        machine.wait_until_succeeds(f"test $(stat -c %Y /tmp/ignored-inhibit) -ge {int(sent) + 5}")
        swaymsg("'[app_id=foot] kill'")
        machine.wait_until_fails("pgrep foot")

    with subtest("a logind idle inhibitor holds back timeouts until it is released"):
        since = cursor()
        uctl(
            "systemd-run --user --unit=idle-inhibitor"
            + " /run/current-system/sw/bin/systemd-inhibit --what=idle --who=test --why=test"
            + " ${sleep} 600"
        )
        wait_for_log(since, "logind idle inhibitor held")
        since = cursor()
        swaymsg("seat - cursor move 30 30")
        wait_for_log(since, "idle after 9 s (timeout 2)")
        text = journal(since)
        machine.log(text)
        assert "idle after 3 s (timeout 0)" in text and "idle after 6 s (timeout 1)" in text, text
        assert "starting the locker" not in text, text
        assert "spawning touch /tmp/spawned" not in text, text
        assert "spawning touch /tmp/ignored-inhibit" in text, text

        since = cursor()
        uctl("systemctl --user stop idle-inhibitor.service")
        released = wait_for_log(since, "no logind idle inhibitor")
        # Timeout 2 ran, so it keeps its notification: its resume is still to come.
        wait_for_log(since, "rearming timeouts [0, 1]")
        t = [
            wait_for_log(since, f"idle after {secs} s (timeout {i})")
            for i, secs in enumerate([3, 6])
        ]
        machine.log(f"idle {[round(x - released, 3) for x in t]} s after the release")
        assert 2.5 < t[0] - released < 4.5, f"timeout 0 idled {t[0] - released} s after"
        assert 5.5 < t[1] - released < 7.5, f"timeout 1 idled {t[1] - released} s after"
        wait_for_log(since, "starting the locker")
        wait_for_log(since, "spawning touch /tmp/spawned")
        machine.sleep(4)
        text = journal(since)
        assert "timeout 2" not in text, text
        unlock()

    with subtest("while the session is inactive, no idle action runs"):
        since = cursor()
        machine.succeed("chvt 2")
        wait_for_log(since, f"session {session} inactive")
        swaymsg("seat - cursor move 40 40")
        wait_for_log(since, "idle after 9 s (timeout 2)")
        text = journal(since)
        machine.log(text)
        assert "idle after 3 s (timeout 0)" in text, text
        # The on-resume of timeouts that ran before the switch still runs (the input resumes them).
        for line in ["starting the locker", "spawning touch /tmp/spawned", "spawning touch /tmp/ignored"]:
            assert line not in text, f"inactive, yet logged {line!r}:\n{text}"
        machine.succeed("chvt 1")
        wait_for_log(since, f"session {session} active")

    with subtest("sleep waits for the lock"):
        inhibitors = our_inhibitors()
        machine.log(f"inhibitors: {inhibitors}")
        assert len(inhibitors) == 1, inhibitors
        assert "sleep" in inhibitors[0] and "delay" in inhibitors[0], inhibitors
        # The inhibitor fd must be close-on-exec: O_CLOEXEC is 02000000 in fdinfo's octal flags.
        pid = uctl("systemctl --user show -p MainPID --value idle-manager.service").strip()
        flags = machine.succeed(
            f"for fd in /proc/{pid}/fd/*; do case $(readlink $fd) in /run/systemd/inhibit/*.ref)"
            + f" grep '^flags:' /proc/{pid}/fdinfo/$(basename $fd);; esac; done"
        ).split()
        machine.log(f"inhibitor fd flags: {flags}")
        assert len(flags) == 2 and int(flags[1], 8) & 0o2000000, flags

        since = suspend()
        prepared = wait_for_log(since, "preparing for sleep")
        wait_for_log(since, "starting the locker")
        locked = wait_for_log(since, f"session {session} locked")
        released = wait_for_log(since, "sleep inhibitor released")
        slept = wait_for_log(since, "Performing sleep operation", tag="systemd-sleep")
        machine.log(
            f"released {round(released - prepared, 3)} s after PrepareForSleep,"
            + f" sleep {round(slept - released, 3)} s later"
        )
        assert released - prepared < 2, f"released {released - prepared} s after PrepareForSleep"
        assert locked <= released, "the inhibitor was released before the session was locked"
        order = ["preparing for sleep", f"session {session} locked", "sleep inhibitor released"]
        text = journal(since)
        assert sorted(order, key=text.index) == order, text
        assert released < slept, "logind went on before the inhibitor was released"
        back = wait_for_log(since, "back from sleep", timeout=60)
        wait_for_log(since, "rearming timeouts [0, 1, 2]")
        text = journal(since)
        machine.log(text)
        assert "lock wait timed out" not in text, text
        order = ["back from sleep", "sleep inhibitor taken", "rearming timeouts [0, 1, 2]"]
        assert sorted(order, key=text.index) == order, text
        assert len(our_inhibitors()) == 1, our_inhibitors()
        # The timers start again from the wake; while locked, the lock starts nothing.
        idled = wait_for_log(since, "idle after 3 s (timeout 0)")
        assert 2.5 < idled - back < 4.5, f"timeout 0 idled {idled - back} s after waking"
        assert journal(since).count("starting the locker") == 1
        unlock()
        uctl("systemctl --user stop idle-manager.service")

    with subtest("a second locker is refused; a locker that never locks delays sleep 4 s"):
        since = cursor()
        uctl("systemctl --user start idle-manager-broken-locker.service")
        wait_for_log(since, "watching 1 timeouts")
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, "starting the locker")
        uctl("systemctl --user is-active rust-wl-locker.service")
        machine.succeed(f"loginctl lock-session {session}")
        wait_for_log(since, "locker already running")

        since = suspend()
        prepared = wait_for_log(since, "preparing for sleep")
        timed_out = wait_for_log(since, "lock wait timed out; releasing the sleep inhibitor")
        released = wait_for_log(since, "sleep inhibitor released")
        slept = wait_for_log(since, "Performing sleep operation", tag="systemd-sleep")
        machine.log(
            f"released {round(released - prepared, 3)} s after PrepareForSleep,"
            + f" sleep {round(slept - released, 3)} s later"
        )
        assert 3.8 < timed_out - prepared < 4.6, f"timed out {timed_out - prepared} s after"
        # logind itself gives up on delay inhibitors after 5 s.
        assert released < slept < prepared + 5, "logind went on before the release, or after 5 s"
        wait_for_log(since, "back from sleep", timeout=60)
        wait_for_log(since, "sleep inhibitor taken")
        assert locked_hint() == "no"

        since = cursor()
        machine.succeed(f"loginctl unlock-session {session}")
        wait_for_log(since, "unlocking the locker")
        wait_until_unloaded("rust-wl-locker.service")
        uctl("systemctl --user stop idle-manager-broken-locker.service")

    with subtest("compositor loss -> exit with failure"):
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        wait_for_log(since, "watching 3 timeouts")
        uctl("systemctl --user stop sway.service")
        machine.wait_until_succeeds(PREFIX + "systemctl --user is-failed idle-manager.service", timeout=10)
        assert exit_status("idle-manager.service") == "1"
        wait_for_log(since, "Wayland compositor")

    with subtest("SIGTERM -> clean exit"):
        start_sway()
        uctl("systemctl --user reset-failed")
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        wait_for_log(since, "watching 3 timeouts")
        uctl("systemctl --user stop idle-manager.service")
        wait_for_log(since, "SIGTERM received, exiting")
        machine.fail(PREFIX + "systemctl --user is-failed idle-manager.service")
        assert exit_status("idle-manager.service") == "0"

    with subtest("invalid config -> exit 1 with a readable error"):
        since = cursor()
        uctl("systemctl --user start idle-manager-bad.service")
        machine.wait_until_succeeds(PREFIX + "systemctl --user is-failed idle-manager-bad.service", timeout=10)
        assert exit_status("idle-manager-bad.service") == "1"
        text = journal(since)
        machine.log(text)
        assert "unexpected node `timout`" in text and "line 3" in text, text

    with subtest("usage error -> exit 2"):
        status, _ = machine.execute("${idleManager}/bin/rust-wl-idle-manager --bogus")
        assert status == 2, f"expected exit 2, got {status}"
  '';
}
