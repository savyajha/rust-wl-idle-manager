# NixOS VM test: the greeter's power row and confirmation dialog, under greetd, on one
# output (1024x768, scale 1), with the default look.
#
# logind does the real work. Sleep and hibernate are made harmless: systemd-suspend and
# systemd-hibernate (which logind starts) only write a marker. Hibernation needs swap, so a
# second disk is swap and the resume device. Restart and shut down are real: the test waits
# for the kernel's last words on the console ("Restarting system", "Power down"; QEMU runs
# with -no-reboot, so a reboot ends it too) and boots the machine again.
#
# One user, alice, with one session.
#
# Behaviours asserted:
#   1. logind's answers for the greeter: the four buttons, drawn along the bottom after
#      Cancel (back to the row of alice and "Other…")
#   2. Sleep, by a click: no dialog, the marker; on resume the half-typed password is
#      wiped (the next attempt, typed in full, logs in), and the focus is off Sleep (Enter
#      does not sleep again)
#   3. Hibernate by the keyboard: Tab to the row, Right, Enter; the marker; then again
#      Enter does not hibernate again, and typing goes to the field
#   4. a call logind refuses (a sleep inhibitor held, which interactive=false cannot
#      override): the failure logged, a message on its line, no sleep
#   5. Restart: a dialog (its Cancel focused: Enter cancels), which holds the keys (typed
#      text goes nowhere and what was typed stays), Escape and a click outside cancel too,
#      each time leaving the screen as it was; then its Restart reboots, and the machine
#      comes back to the greeter
#   6. Shut Down by the keyboard: the dialog, Right to its Shut Down, Enter: power off

{ pkgs }:

let
  inherit (pkgs) lib;
  dir = "/var/lib/greeter-test";
  password = "alice pass";

  session = pkgs.writeShellScript "session" ''
    echo "$1" > ${dir}/session-$1
    while [ ! -e ${dir}/end ]; do ${pkgs.coreutils}/bin/sleep 0.1; done
    ${pkgs.coreutils}/bin/rm -f ${dir}/end
  '';
  sessions = pkgs.runCommand "sessions" { } ''
    mkdir -p $out/share/wayland-sessions
    cat > $out/share/wayland-sessions/alpha.desktop <<'EOF'
    [Desktop Entry]
    Name=Alpha
    Exec=${session} alpha
    EOF
  '';

  gpu = builtins.toJSON {
    driver = "virtio-gpu-pci";
    id = "gpu";
    max_outputs = 1;
    xres = 1024;
    yres = 768;
  };
  marker = name: [
    ""
    "${pkgs.coreutils}/bin/touch ${dir}/${name}"
  ];
in
pkgs.testers.runNixOSTest {
  name = "greeter-power";

  nodes.machine = {
    imports = [ ../greeter-module.nix ];

    virtualisation.memorySize = 1024;
    virtualisation.qemu.options = [
      "-vga none"
      "-device ${lib.escapeShellArg gpu}"
    ];
    # Swap for hibernation, as the resume device.
    virtualisation.emptyDiskImages = [ 2048 ];
    boot.kernelParams = [ "resume=/dev/vdb" ];
    systemd.services.test-swap = {
      wantedBy = [ "multi-user.target" ];
      before = [ "greetd.service" ];
      serviceConfig.Type = "oneshot";
      script = "${pkgs.util-linux}/bin/mkswap /dev/vdb && ${pkgs.util-linux}/bin/swapon /dev/vdb";
    };
    systemd.services.systemd-suspend.serviceConfig.ExecStart = marker "suspended";
    systemd.services.systemd-hibernate.serviceConfig.ExecStart = marker "hibernated";

    users.users.alice = {
      isNormalUser = true;
      description = "Alice Liddell";
      inherit password;
    };

    programs.rust-wl-greeter = {
      enable = true;
      sessions = "${sessions}/share/wayland-sessions";
    };

    fonts.packages = [ pkgs.adwaita-fonts ];
    systemd.tmpfiles.rules = [ "d ${dir} 0777 root root -" ];
  };

  testScript = ''
    import re
    ${builtins.readFile ./screen.py}

    # This boot's: a reboot starts another.
    LOG = "journalctl -b --no-pager -o cat -t rust-wl-greeter"
    DIR = "${dir}"
    PASSWORD = "${password}"
    LAYOUT = (1024, 768)
    PLAIN = [0x20, 0x24, 0x28]
    # The bottom row: Cancel, then the power buttons; boxes 72x56, 16 apart, centred, 24
    # from the bottom.
    def power_button(i, n=5):
        x0 = 512 - (n * 72 + (n - 1) * 16) / 2
        return (x0 + 36 + (i + 1) * 88, 768 - 24 - 28)
    def power_box(i, n=5):
        x, y = power_button(i, n)
        return (x - 36, y - 28, 72, 56)
    # The dialog: 320x132, centred; its buttons 120x32, 20 from its bottom, 16 apart.
    CANCEL = (512 - 8 - 60, 318 + 132 - 20 - 16)
    CONFIRM = (512 + 8 + 60, 318 + 132 - 20 - 16)
    DIALOG = (352, 318, 320, 132)
    # The default look's field: 176 wide, centred, 558 to 587 from the top; the dots' middle
    # row, left of its button.
    FIELD_ROW = (424, 573, 146, 1)
    # The message line, 32 under the field, 20 high.
    MESSAGE = (424, 587 + 32, 176, 20)

    def read_log():
        return machine.succeed(LOG)

    def count(text):
        return read_log().count(text)

    def wait_for_line(text, n=1, timeout=60):
        try:
            command = f"test $({LOG} | grep -cF -- '{text}') -ge {n}"
            machine.wait_until_succeeds(command, timeout=timeout)
        except Exception:
            print(read_log())
            raise

    def wait_for_more(text, action):
        n = count(text)
        action()
        wait_for_line(text, n + 1)

    def area(rect):
        width, pixels = screen()
        x0, y0, w, h = [round(v) for v in rect]
        rows = range(y0, y0 + h)
        return [list(pixels[(y * width + x) * 3:][:3]) for y in rows for x in range(x0, x0 + w)]

    def marked(rect):
        return sum(any(abs(p[c] - PLAIN[c]) > 40 for c in range(3)) for p in area(rect))

    def dots():
        """The dots in the field: runs of opaque white along its middle row."""
        whites = [min(p) > 240 for p in area(FIELD_ROW)]
        return sum(white and not before for before, white in zip([False] + whites, whites))

    def wait_for_dots(n):
        with machine.nested(f"waiting for {n} dots"):
            retry(lambda _: dots() == n, 20)

    def greeter_ready():
        """The greeter of this boot, at alice's prompt, with its power row."""
        wait_for_line("prompt: secret")
        wait_for_line("power: ")
        wait_for_line("input ready")

    def log_in():
        """Type the password and Enter: the session must start at once (a wrong password
        fails here); then it is ended, and the next greeter waited for."""
        before = read_log()
        machine.send_chars(PASSWORD + "\n")
        def started(_):
            after = read_log()[len(before):]
            assert "wrong password" not in after, after
            return "starting session" in after
        retry(started, 30)
        machine.wait_for_file(f"{DIR}/session-alpha")
        machine.succeed(f"rm {DIR}/session-alpha")
        n, inputs = count("prompt: secret"), count("input ready")
        machine.succeed(f"touch {DIR}/end")
        wait_for_line("prompt: secret", n + 1)
        # Its compositor taking input: a key or click before that is lost.
        wait_for_line("input ready", inputs + 1)

    def restart_greetd():
        n = count("input ready")
        machine.succeed("systemctl restart greetd")
        wait_for_line("input ready", n + 1)

    machine.start()
    machine.wait_for_unit("greetd.service")

    with subtest("logind's answers: the four buttons along the bottom"):
        greeter_ready()
        text = read_log()
        want = "power: sleep yes, hibernate yes, restart yes, shut down yes"
        assert want in text, text
        retry(lambda _: all(marked(power_box(i)) > 0 for i in range(4)), 10)

    with subtest("Sleep, by a click: at once, and what was typed is wiped on resume"):
        machine.send_chars("abc")
        wait_for_dots(3)
        click(*power_button(0))
        machine.wait_for_file(f"{DIR}/suspended")
        wait_for_line("resumed")
        assert "dialog" not in read_log()
        wait_for_dots(0)
        # The focus is off Sleep: Enter (in the empty field) does nothing.
        machine.succeed(f"rm {DIR}/suspended")
        machine.send_key("ret")
        log_in()
        machine.fail(f"test -e {DIR}/suspended")

    with subtest("Hibernate by the keyboard: Tab to the row, Right, Enter"):
        # From the field (empty: no arrow), Cancel, then the power buttons.
        for _ in range(2):
            machine.send_key("tab")
        machine.send_key("right")
        machine.send_key("ret")
        machine.wait_for_file(f"{DIR}/hibernated")
        wait_for_line("resumed", 2)
        # The focus is off Hibernate (where the keyboard left it): Enter does nothing, and
        # what is typed next, after it, goes to the field.
        machine.succeed(f"rm {DIR}/hibernated")
        machine.send_key("ret")
        machine.send_chars("x")
        wait_for_dots(1)
        # The greeter asked once (the marker comes later, if at all).
        assert count("power: hibernate") == 1, read_log()
        machine.fail(f"test -e {DIR}/hibernated")
        machine.send_key("backspace")
        wait_for_dots(0)

    with subtest("a call logind refuses (a sleep inhibitor) says so, generically"):
        machine.succeed(
            "systemd-run --unit=inhibit systemd-inhibit --who=test-inhibitor --what=sleep"
            " --mode=block ${pkgs.coreutils}/bin/sleep infinity"
        )
        machine.wait_until_succeeds("systemd-inhibit --list --no-legend | grep -q test-inhibitor")
        assert marked(MESSAGE) == 0
        wait_for_more("power: sleep failed", lambda: click(*power_button(0)))
        # "Could not sleep" on the message line (logind's error is in the log alone).
        retry(lambda _: marked(MESSAGE) > 0, 10)
        machine.fail(f"test -e {DIR}/suspended")
        machine.succeed("systemctl stop inhibit")

    with subtest("Restart: Cancel, Escape and a click outside cancel; Restart reboots"):
        machine.send_chars("ab")
        wait_for_dots(2)
        # Cancel is focused: Enter cancels.
        wait_for_more("dialog: Restart now?", lambda: click(*power_button(2)))
        retry(lambda _: marked(DIALOG) > 0, 10)
        # Typing goes nowhere while it is open.
        machine.send_chars("cd")
        wait_for_more("dialog cancelled", lambda: machine.send_key("ret"))
        # Gone from the screen: where it was is the background again.
        retry(lambda _: marked(DIALOG) == 0, 10)
        wait_for_dots(2)
        wait_for_more("dialog: Restart now?", lambda: click(*power_button(2)))
        retry(lambda _: marked(DIALOG) > 0, 10)
        wait_for_more("dialog cancelled", lambda: machine.send_key("esc"))
        retry(lambda _: marked(DIALOG) == 0, 10)
        wait_for_more("dialog: Restart now?", lambda: click(*power_button(2)))
        retry(lambda _: marked(DIALOG) > 0, 10)
        wait_for_more("dialog cancelled", lambda: click(100, 100))
        retry(lambda _: marked(DIALOG) == 0, 10)
        # Nothing was lost, or sent: the login goes on.
        assert dots() == 2, dots()
        assert "power: restart" not in read_log()
        wait_for_more("dialog: Restart now?", lambda: click(*power_button(2)))
        click(*CONFIRM)
        machine.wait_for_console_text("reboot: Restarting system")
        machine.wait_for_shutdown()
        machine.start()
        machine.wait_for_unit("greetd.service")
        greeter_ready()

    with subtest("Shut Down by the keyboard: the dialog, Right, Enter: power off"):
        # Cancel, then the four power buttons.
        for _ in range(5):
            machine.send_key("tab")
        wait_for_more("dialog: Shut down now?", lambda: machine.send_key("ret"))
        machine.send_key("right")
        machine.send_key("ret")
        machine.wait_for_console_text("reboot: Power down")
        machine.wait_for_shutdown()
  '';
}
