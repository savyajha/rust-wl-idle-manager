# NixOS VM test: the real binary against a headless sway.
#
# sway runs headless as a user service and provides ext-idle-notify-v1.
# rust-wl-idle-manager runs as a user service connected to it, and the test
# reads its log lines from the journal.
#
# Behaviours asserted:
#   1. each timeout idles once, after its own delay, in order, and its
#      command runs: the locker as a transient user unit, the spawns (found
#      in PATH) as transient units that are unloaded once they exit
#   2. input resumes every idle timeout, and the on-resume command runs; one
#      that is not found is logged as an error
#   3. after a resume, the timers start again; a second lock while the
#      locker runs is logged and the daemon keeps running; a locker that
#      fails is unloaded at once
#   4. an idle inhibitor holds back every timeout but the ignore-inhibit one
#   5. the compositor going away -> exit with a failure status
#   6. SIGTERM -> clean exit 0
#   7. an invalid config -> exit 1 with a readable error
#   8. a usage error -> exit 2

{ pkgs, idleManager }:

let
  user = "alice";
  uid = 1000;
  runtimeDir = "/run/user/${toString uid}";

  # The spawns write markers to /tmp. Timeout 2 spawns rather than suspends: the
  # lingering test user has no logind session, so polkit would refuse a suspend.
  config = pkgs.writeText "idle.kdl" ''
    locker "${pkgs.coreutils}/bin/sleep" "600"
    timeout 3 { lock; on-resume "no-such-program"; }
    timeout 6 { spawn "touch" "/tmp/spawned"; on-resume "touch" "/tmp/resumed"; }
    timeout 9 { ignore-inhibit; spawn "touch" "/tmp/ignored-inhibit"; }
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
      linger = true;
    };

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
    systemd.user.services.idle-manager-bad = mkIdleManager badConfig;

    virtualisation.memorySize = 1024;
  };

  testScript = ''
    PREFIX = "sudo -u ${user} XDG_RUNTIME_DIR=${runtimeDir} "

    def uctl(cmd):
        return machine.succeed(PREFIX + cmd)

    def cursor():
        out = machine.succeed("journalctl -n 1 --show-cursor --no-pager")
        return out.strip().splitlines()[-1].removeprefix("-- cursor: ")

    def journal_cmd(since):
        """The daemon's journal after cursor `since`, with Unix timestamps."""
        return f"journalctl --no-pager -o short-unix -t rust-wl-idle-manager --after-cursor='{since}'"

    def journal(since):
        return machine.succeed(journal_cmd(since))

    def wait_for_log(since, text, timeout=30):
        """The timestamp of the daemon's first line containing `text` after cursor `since`."""
        machine.wait_until_succeeds(f"{journal_cmd(since)} | grep -qF '{text}'", timeout=timeout)
        for line in journal(since).splitlines():
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

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@${toString uid}.service")
    start_sway()

    with subtest("each timeout idles once, in order"):
        since = cursor()
        uctl("systemctl --user start idle-manager.service")
        t0 = wait_for_log(since, "watching 3 timeouts")
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

    with subtest("after a resume, the timers start again"):
        again = wait_for_log(since, "idle after 3 s (timeout 0)")
        machine.log(f"idle again {round(again - sent, 3)} s after the input")
        assert 2.5 < again - sent < 4.5, f"re-idled after {again - sent} s"
        wait_for_log(since, "locker already running")
        uctl("systemctl --user is-active idle-manager.service")
        # A killed locker fails; CollectMode=inactive-or-failed must still unload it.
        uctl("systemctl --user kill -s KILL rust-wl-locker.service")
        machine.wait_until_succeeds(
            PREFIX + "systemctl --user show -p LoadState --value rust-wl-locker.service | grep -qx not-found"
        )

    with subtest("an idle inhibitor holds back all but the ignore-inhibit timeout"):
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

    with subtest("compositor loss -> exit with failure"):
        since = cursor()
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
