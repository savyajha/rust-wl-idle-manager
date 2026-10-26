# NixOS VM test: the greeter's power row and dialog at a fractional scale, 1.6 (as on the
# laptop's panel), on one output of 1024x768 (640x480 logical), with the default look: the
# pointer and the keyboard reach the buttons. Sleep only writes a marker; no restart or
# power off is confirmed. One user, alice, and one session; no swap, so no Hibernate.
# Tap-to-click and clickfinger are on; QEMU has no touchpad, so only the compositor's flags
# are checked. The cursor theme is capitaine-cursors at size 24, as on the laptop.
#
# Behaviours asserted:
#   1. the three buttons (Sleep, Restart, Shut Down) are drawn; the compositor runs with --tap
#      and --clickfinger
#   2. the pointer is the theme's 48 px image (24 at scale 1.6, rounded up to a size the
#      theme has), drawn over the background with its hotspot at the pointer
#   3. a click on Restart opens the dialog; a click on its Cancel closes it, the focus back
#      on Restart
#   4. Tab (to Shut Down) and Enter open the dialog; Escape closes it
#   5. a right click on Restart does nothing: only the left button clicks
#   6. a click on Sleep sleeps: the marker

{ pkgs }:

let
  inherit (pkgs) lib;
  dir = "/var/lib/greeter-test";
  session = pkgs.writeShellScript "session" ''
    while [ ! -e ${dir}/end ]; do ${pkgs.coreutils}/bin/sleep 0.1; done
  '';
  sessions = pkgs.runCommand "sessions" { } ''
    mkdir -p $out/share/wayland-sessions
    cat > $out/share/wayland-sessions/alpha.desktop <<'EOF'
    [Desktop Entry]
    Name=Alpha
    Exec=${session}
    EOF
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
  name = "greeter-power-scale";

  nodes.machine = {
    imports = [ ../greeter-module.nix ];

    virtualisation.memorySize = 1024;
    virtualisation.qemu.options = [
      "-vga none"
      "-device ${lib.escapeShellArg gpu}"
    ];
    systemd.services.systemd-suspend.serviceConfig.ExecStart = [
      ""
      "${pkgs.coreutils}/bin/touch ${dir}/suspended"
    ];

    users.users.alice = {
      isNormalUser = true;
      password = "unused";
    };

    programs.rust-wl-greeter = {
      enable = true;
      sessions = "${sessions}/share/wayland-sessions";
      scales.Virtual-1 = 1.6;
      tapToClick = true;
      clickfinger = true;
      cursorTheme = "${pkgs.capitaine-cursors}/share/icons/capitaine-cursors";
    };

    fonts.packages = [ pkgs.adwaita-fonts ];
    systemd.tmpfiles.rules = [ "d ${dir} 0777 root root -" ];
  };

  testScript = ''
    import re
    import struct
    ${builtins.readFile ./screen.py}

    LOG = "journalctl --no-pager -o cat -t rust-wl-greeter"
    DIR = "${dir}"
    PLAIN = [0x20, 0x24, 0x28]
    SCALE = 1.6
    # Logical pixels, as the pointer has them.
    LAYOUT = (640, 480)
    # The power row: three buttons 72x56, 16 apart, centred, 24 from the bottom.
    def power_button(i, n=3):
        x0 = 320 - (n * 72 + (n - 1) * 16) / 2
        return (x0 + 36 + i * 88, 480 - 24 - 28)
    # The dialog: 320x132, centred; its buttons 120x32, 20 from its bottom, 16 apart.
    CANCEL = (320 - 8 - 60, 240 - 66 + 132 - 20 - 16)

    def xcursor(path, size):
        """The image of a nominal size in an Xcursor file: its width, height, hotspot, and its
        premultiplied ARGB pixels."""
        data = open(path, "rb").read()
        (n,) = struct.unpack_from("<I", data, 12)
        for i in range(n):
            _, nominal, at = struct.unpack_from("<III", data, 16 + 12 * i)
            if nominal == size:
                w, h, x, y = struct.unpack_from("<IIII", data, at + 16)
                return w, h, (x, y), struct.unpack_from(f"<{w * h}I", data, at + 36)
        raise Exception(f"no {size} px image in {path}")

    def over(argb, rgb):
        """A premultiplied ARGB pixel over an RGB one."""
        alpha = argb >> 24
        return [(argb >> s & 0xff) + c * (255 - alpha) / 255 for s, c in zip([16, 8, 0], rgb)]

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

    def wait_for_more(text, action, timeout=20):
        n = count(text)
        action()
        wait_for_line(text, n + 1, timeout)

    def marked(rect):
        """How many pixels of a logical rectangle differ clearly from the plain background."""
        width, pixels = screen()
        x0, y0, w, h = [round(v * SCALE) for v in rect]
        rows = range(y0, y0 + h)
        area = [pixels[(y * width + x) * 3:][:3] for y in rows for x in range(x0, x0 + w)]
        return sum(any(abs(p[c] - PLAIN[c]) > 40 for c in range(3)) for p in area)

    machine.start()
    machine.wait_for_unit("greetd.service")
    wait_for_line("prompt: secret")
    wait_for_line("power: sleep yes, hibernate na, restart yes, shut down yes")
    wait_for_line("input ready")

    with subtest("the three buttons are drawn"):
        def boxes(_):
            return all(marked((x - 36, y - 28, 72, 56)) > 0 for x, y in map(power_button, range(3)))
        retry(boxes, 10)
        machine.succeed("pgrep -f '^/nix/store/[^ ]*/bin/rust-wl-greeter-compositor .*--tap --clickfinger'")

    with subtest("the pointer is the theme's 48 px image, its hotspot at the pointer"):
        assert "cursor theme: sizes [24, 30, 36, 48, 60, 72]" in read_log(), read_log()
        theme = "${pkgs.capitaine-cursors}/share/icons/capitaine-cursors"
        w, h, (hx, hy), image = xcursor(f"{theme}/cursors/left_ptr", 48)
        # The pointer at logical (100, 120), which the tablet's range has exactly (5120 and
        # 8192 of 32768): physical (160, 192), less the hotspot.
        x0, y0 = 160 - hx, 192 - hy
        def block():
            width, pixels = screen()
            rows = range(y0, y0 + h)
            return [pixels[(y * width + x) * 3:][:3] for y in rows for x in range(x0, x0 + w)]
        # No cursor before the first motion: the background, for the image to be drawn over.
        expected = [over(argb, rgb) for argb, rgb in zip(image, block())]
        move(100, 120)
        def drawn(_):
            off = sum(any(abs(a - b) > 2 for a, b in zip(p, e)) for p, e in zip(block(), expected))
            print(f"{off} of {w * h} pixels unlike the theme's image")
            return off == 0
        retry(drawn, 10)

    with subtest("a click on Restart opens the dialog; a click on its Cancel closes it"):
        wait_for_more("dialog: Restart now?", lambda: click(*power_button(1)))
        wait_for_more("dialog cancelled", lambda: click(*CANCEL))

    with subtest("Tab to Shut Down, Enter: the dialog; Escape closes it"):
        # The focus is back on Restart, where the click left it: Tab, Shut Down.
        machine.send_key("tab")
        wait_for_more("dialog: Shut down now?", lambda: machine.send_key("ret"))
        wait_for_more("dialog cancelled", lambda: machine.send_key("esc"))

    with subtest("a right click on Restart does nothing"):
        def dialogs():
            return [line for line in read_log().splitlines() if "dialog: " in line]
        before = len(dialogs())
        x, y = power_button(1)
        move(x + 1, y)
        move(x, y)
        for down in [True, False]:
            input_events({"type": "btn", "data": {"down": down, "button": "right"}})
        # Then a left click on Shut Down: its dialog is the first since, as the greeter gets
        # the clicks in order, so the right click opened none.
        wait_for_more("dialog: Shut down now?", lambda: click(*power_button(2)))
        new = dialogs()[before:]
        assert len(new) == 1 and "Shut down now?" in new[0], new
        wait_for_more("dialog cancelled", lambda: machine.send_key("esc"))

    with subtest("a click on Sleep sleeps"):
        click(*power_button(0))
        machine.wait_for_file(f"{DIR}/suspended")
  '';
}
