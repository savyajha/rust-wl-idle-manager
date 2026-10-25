# NixOS VM test: the greeter's indicator row, under greetd, on one output (1024x768, scale
# 1): the keyboard-layout picker with three layouts, and the battery from UPower.
#
# The greeter's config has the layouts "de", "us" and "us(dvorak)" (the compositor's own
# keymap is the default "us"). One user, alice, is listed; the test adds yves, whose UID
# (40011) is past login.defs's UID_MAX, so he logs in through "Other…". The user name typed
# there is a visible field, typed with the layout chosen: "yves" needs the "us" layout's Y,
# and the keys A P G I D type "alice" only in the Dvorak variant. yves's password has an
# "@", which "de" types with AltGr (the right Alt), and "us" with Shift+2.
#
# The VM has no battery and no UPower: none runs, and the bus cannot start one. For the
# battery subtest the test starts a fake (fakeUpower below), whose DisplayDevice is present,
# 50 % and discharging; on SIGUSR1 90 % and charging; on SIGUSR2 not present (UPower with
# no battery). The battery is checked by comparing the indicator row's pixels.
#
# Behaviours asserted:
#   1. the layouts and their short names; without UPower, no battery is shown
#   2. Tab reaches the layout indicator, last after Log In; Enter opens its popup, Down and
#      Enter choose "us"; the row's pixels change; the user name typed for "Other…" is then
#      typed with "us" (yves logs in, typed with the Y key), and the layout he logged in with
#      is remembered for him
#   3. after a restart, alice (with none remembered) has the first layout again, the row
#      looks as before; yves, typed with "de", gets his "us" back; then, at his prompt, a
#      click on the indicator and Up and Enter choose "de", whose AltGr types his "@"
#   4. the variant "us(dvorak)", chosen with the keyboard: its keys type "alice"
#   5. in one greeter, with no restart: UPower starting shows the battery (the row
#      changes); a change of its properties redraws it; UPower stopping hides it, and
#      starting again shows it again; no battery present hides it

{ pkgs }:

let
  inherit (pkgs) lib;
  dir = "/var/lib/greeter-test";
  yvesPassword = "yves@home";

  # Writes its owner, then waits until the test ends it.
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

  # UPower's DisplayDevice, as much of it as the greeter reads.
  fakeUpower =
    pkgs.writers.writePython3 "fake-upower"
      {
        libraries = [
          pkgs.python3Packages.dbus-python
          pkgs.python3Packages.pygobject3
        ];
      }
      ''
        import signal
        import dbus
        import dbus.mainloop.glib
        import dbus.service
        from gi.repository import GLib

        DEVICE = "org.freedesktop.UPower.Device"
        PROPERTIES = "org.freedesktop.DBus.Properties"


        class Device(dbus.service.Object):
            def __init__(self, bus):
                path = "/org/freedesktop/UPower/devices/DisplayDevice"
                super().__init__(bus, path)
                self.properties = {
                    "IsPresent": dbus.Boolean(True),
                    "Percentage": dbus.Double(50.0),
                    "State": dbus.UInt32(2),
                }

            @dbus.service.method(PROPERTIES, in_signature="ss", out_signature="v")
            def Get(self, interface, name):
                return self.properties[name]

            @dbus.service.method(PROPERTIES, in_signature="s", out_signature="a{sv}")
            def GetAll(self, interface):
                return self.properties

            @dbus.service.signal(PROPERTIES, signature="sa{sv}as")
            def PropertiesChanged(self, interface, changed, invalidated):
                pass

            def change(self, changed):
                self.properties.update(changed)
                self.PropertiesChanged(DEVICE, changed, [])
                return True

            def charge(self):
                changed = {"Percentage": dbus.Double(90.0), "State": dbus.UInt32(1)}
                return self.change(changed)

            def remove(self):
                return self.change({"IsPresent": dbus.Boolean(False)})


        dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
        bus = dbus.SystemBus()
        name = dbus.service.BusName("org.freedesktop.UPower", bus)
        device = Device(bus)
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, device.charge)
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR2, device.remove)
        GLib.MainLoop().run()
      '';

  upowerPolicy = pkgs.writeTextDir "share/dbus-1/system.d/fake-upower.conf" ''
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
      "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <policy user="root"><allow own="org.freedesktop.UPower"/></policy>
      <policy context="default"><allow send_destination="org.freedesktop.UPower"/></policy>
    </busconfig>
  '';

  gpu = builtins.toJSON {
    driver = "virtio-gpu-pci";
    id = "gpu";
    max_outputs = 1;
    xres = 1024;
    yres = 768;
  };
in
pkgs.testers.runNixOSTest {
  name = "greeter-indicators";

  nodes.machine = {
    imports = [ ../greeter-module.nix ];

    virtualisation.memorySize = 1024;
    virtualisation.qemu.options = [
      "-vga none"
      "-device ${lib.escapeShellArg gpu}"
    ];

    users.users.alice = {
      isNormalUser = true;
      description = "Alice Liddell";
      password = "unused";
    };

    programs.rust-wl-greeter = {
      enable = true;
      sessions = "${sessions}/share/wayland-sessions";
      keyboardLayouts = [
        "de"
        "us"
        "us(dvorak)"
      ];
    };

    services.dbus.packages = [ upowerPolicy ];
    systemd.services.fake-upower.serviceConfig = {
      Type = "dbus";
      BusName = "org.freedesktop.UPower";
      ExecStart = fakeUpower;
    };

    fonts.packages = [ pkgs.adwaita-fonts ];
    systemd.tmpfiles.rules = [ "d ${dir} 0777 root root -" ];
  };

  testScript = ''
    import re
    ${builtins.readFile ./screen.py}

    LOG = "journalctl --no-pager -o cat -t rust-wl-greeter"
    DIR = "${dir}"
    STATE = "/var/lib/rust-wl-greeter/state"
    YVES_PASSWORD = "${yvesPassword}"
    LAYOUT = (1024, 768)
    # The indicator row, top right: 24 high, 8 from the top, ending 12 from the right edge;
    # the layout's chip is rightmost with no battery, its centre about 20 in from its end.
    ROW = (824, 8, 188, 24)
    CHIP = (992, 20)
    # The "‹ Users" chip, left of the default look's field (176x30, centred, its bottom
    # 156 + 10 + the caps lock line's 16 from the screen's).
    USERS = (512 - 88 - 12 - 40, 768 - 156 - 26 - 15)
    def other():
        """"Other…" on the row of alice and it: right of her, 120 apart, centred."""
        return (512 + 60, 768 - 249 - 32)

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
        """Do `action`; wait until `text` is logged once more than before."""
        n = count(text)
        action()
        wait_for_line(text, n + 1)

    def type_de(text):
        """Type `text` as the "de" layout reads the keys: Y and Z are swapped."""
        machine.send_chars(text.translate(str.maketrans("yzYZ", "zyZY")))

    def row():
        """The indicator row's pixels."""
        width, pixels = screen()
        x0, y0, w, h = ROW
        return [pixels[(y * width + x0) * 3:(y * width + x0 + w) * 3] for y in range(y0, y0 + h)]

    def restart_greetd():
        n = count("input ready")
        machine.succeed("systemctl restart greetd")
        wait_for_line("input ready", n + 1)

    def logged_in(user, before):
        """The session (the only one) must start as `user`, since the log was `before`, and
        at once: a wrong password fails here; then it is ended."""
        def started(_):
            after = read_log()[len(before):]
            assert "wrong password" not in after, after
            return "starting session" in after
        retry(started, 30)
        path = f"{DIR}/session-alpha"
        machine.wait_for_file(path)
        owner = machine.succeed(f"stat -c %U {path} && rm {path}").strip()
        assert owner == user, owner
        n = count("input ready")
        machine.succeed(f"touch {DIR}/end")
        wait_for_line("input ready", n + 1)

    def to_other():
        """From alice's login, by "‹ Users" and a click: "Other…"'s user name."""
        wait_for_more("back to the users", lambda: click(*USERS))
        wait_for_more("asking for a user name", lambda: click(*other()))

    machine.start()
    machine.wait_for_unit("greetd.service")
    machine.succeed(
        "useradd -m -u 40011 yves",
        f"echo 'yves:{YVES_PASSWORD}' | chpasswd",
    )

    with subtest("the layouts' short names; without UPower, no battery"):
        wait_for_line("prompt: secret")
        wait_for_line("no battery from UPower: ")
        wait_for_line("battery: none")
        text = read_log()
        assert "layouts: de (DE), us (US), us(dvorak) (US2); DE chosen (first)" in text, text
        # Once the screen shows it: the layout's white text.
        retry(lambda _: any(max(line) > 200 for line in row()), 10)
        plain = row()

    with subtest("Tab, Enter, Down, Enter choose us; yves's name is typed with it"):
        # The field, "‹ Users", Log In, then the indicator.
        for _ in range(3):
            machine.send_key("tab")
        wait_for_more("layout popup shown", lambda: machine.send_key("ret"))
        machine.send_key("down")
        wait_for_more("layout us chosen", lambda: machine.send_key("ret"))
        to_other()
        # Without the focus ring, which the clicks hid.
        retry(lambda _: row() != plain, 10)
        us = row()
        # With "de", the Y key would type "zves", who does not exist.
        prompts = count("prompt: secret")
        machine.send_chars("yves\n")
        wait_for_line("greeting a user who is not listed")
        wait_for_line("prompt: secret", prompts + 1)
        before = read_log()
        machine.send_chars(YVES_PASSWORD + "\n")
        logged_in("yves", before)
        assert 'layout "yves" "us"' in machine.succeed(f"cat {STATE}")

    with subtest("each user's layout after a restart; AltGr types with de"):
        before = read_log()
        restart_greetd()
        wait_for_line("prompt: secret", before.count("prompt: secret") + 1)
        after = read_log()[len(before):]
        # alice has none remembered: the first, de, and the row as it first was.
        assert "DE chosen (first)" in after, after
        retry(lambda _: row() == plain, 10)
        to_other()
        type_de("yves\n")
        wait_for_line("layout us (remembered)")
        retry(lambda _: row() == us, 10)
        # At his prompt, by a click: the popup, then Up and Enter, de.
        wait_for_line("prompt: secret", before.count("prompt: secret") + 2)
        wait_for_more("layout popup shown", lambda: click(*CHIP))
        machine.send_key("up")
        wait_for_more("layout de chosen", lambda: machine.send_key("ret"))
        # The compositor draws the pointer: away from the row.
        move(100, 700)
        retry(lambda _: row() == plain, 10)
        # "@" is AltGr+Q in "de" (and Shift+2 types '"').
        before = read_log()
        type_de("yves")
        machine.send_key("alt_r-q")
        type_de("home\n")
        logged_in("yves", before)
        assert 'layout "yves" "de"' in machine.succeed(f"cat {STATE}")

    with subtest("a variant: us(dvorak), whose A P G I D type alice"):
        wait_for_line("DE chosen (first)", 3)
        for _ in range(3):
            machine.send_key("tab")
        wait_for_more("layout popup shown", lambda: machine.send_key("ret"))
        machine.send_key("down")
        machine.send_key("down")
        wait_for_more("layout us(dvorak) chosen", lambda: machine.send_key("ret"))
        to_other()
        greeted = count("greeting alice (Alice Liddell)")
        machine.send_chars("apgid\n")
        wait_for_line("greeting alice (Alice Liddell)", greeted + 1)

    with subtest("the battery follows UPower in a running greeter"):
        # A greeter with "de", as at first: the row as it was.
        restart_greetd()
        retry(lambda _: row() == plain, 10)
        wait_for_more("battery: 50%", lambda: machine.succeed("systemctl start fake-upower"))
        retry(lambda _: row() != plain, 10)
        half = row()
        charge = lambda: machine.succeed("systemctl kill -s USR1 fake-upower")
        wait_for_more("battery: 90% charging", charge)
        retry(lambda _: row() not in [plain, half], 10)
        # UPower gone, and the bus cannot start one: hidden.
        wait_for_more("battery: none", lambda: machine.succeed("systemctl stop fake-upower"))
        retry(lambda _: row() == plain, 10)
        # Back: shown again, as it is now.
        wait_for_more("battery: 50%", lambda: machine.succeed("systemctl start fake-upower"))
        retry(lambda _: row() == half, 10)
        # No battery present: hidden.
        remove = lambda: machine.succeed("systemctl kill -s USR2 fake-upower")
        wait_for_more("battery: none", remove)
        retry(lambda _: row() == plain, 10)

    print(read_log())
  '';
}
