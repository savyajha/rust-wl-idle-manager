# NixOS VM test: the greeter shows its last user's theme, as their lock screen copies it
# to /var/cache/rust-wl-greeter/<user> (see ui::export), on one output (1024x768, scale
# 1), with the default look; and falls back, file by file, on anything wrong.
#
# The fixture theme (below) is a solid background of #3060c0, 64x36, and an error colour
# of magenta; the look's own background is the plain #202428. The greeter reads the theme
# once, at start, for the last user in its state: the test writes that state, and restarts
# greetd for each case.
#
# Behaviours asserted:
#   1. with no last user, the look's own background
#   2. alice's theme: her background on the screen, her font (DejaVu Serif, as Pango
#      finds it), her error colour on the field's outline when a password is wrong; and
#      the time it took to read
#   3. a background that is a link, someone else's (root's), or too big, is ignored, and
#      the look's background is shown; the rest of the theme still applies
#   4. an empty theme directory: the look's own

{ pkgs }:

let
  inherit (pkgs) lib;
  dir = "/var/lib/greeter-test";
  themes = "/var/cache/rust-wl-greeter";

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

  # cairo's Rgb24 in little-endian bytes: blue, green, red, unused.
  theme = pkgs.runCommand "theme" { nativeBuildInputs = [ pkgs.python3 ]; } ''
    mkdir $out
    python3 -c '
    import struct, sys
    width, height = 64, 36
    header = b"RWGB" + struct.pack("<III", 1, width, height)
    open(sys.argv[1], "wb").write(header + bytes([0xc0, 0x60, 0x30, 0]) * (width * height))
    ' $out/background
    echo "@define-color error_color #ff00ffff;" > $out/gtk.css
    printf "[Settings]\ngtk-font-name=DejaVu Serif 11\n" > $out/settings.ini
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
  name = "greeter-theme";

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
      password = "right one";
    };

    programs.rust-wl-greeter = {
      enable = true;
      sessions = "${sessions}/share/wayland-sessions";
    };

    # The theme's font, besides the default.
    fonts.packages = [
      pkgs.adwaita-fonts
      pkgs.dejavu_fonts
    ];
    systemd.tmpfiles.rules = [ "d ${dir} 0777 root root -" ];
  };

  testScript = ''
    import re, time
    ${builtins.readFile ./screen.py}

    LOG = "journalctl --no-pager -o cat -t rust-wl-greeter"
    THEME = "${themes}/alice"
    LAYOUT = (1024, 768)
    PLAIN = [0x20, 0x24, 0x28]
    BLUE = [0x30, 0x60, 0xc0]
    # Where the background shows plain, between its gradients.
    PROBE = (5, 384)
    # The field's top edge (176 wide, centred, 558 from the top): its outline.
    EDGE = (464, 556, 48, 5)

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

    def restart_greetd():
        """Restart greetd; wait until the new greeter takes input, and return what it
        logged."""
        before = read_log()
        machine.succeed("systemctl restart greetd")
        wait_for_line("input ready", before.count("input ready") + 1)
        return read_log()[len(before):]

    def area_pixels(rect):
        width, pixels = screen()
        x0, y0, w, h = rect
        rows = range(y0, y0 + h)
        return [list(pixels[(y * width + x) * 3:][:3]) for y in rows for x in range(x0, x0 + w)]

    def install(name, owner="alice"):
        machine.succeed(f"install -o {owner} -g greeter -m 0640 ${theme}/{name} {THEME}/{name}")

    machine.start()
    machine.wait_for_unit("greetd.service")

    with subtest("no last user: the look's own background"):
        wait_for_line("input ready")
        text = read_log()
        assert "no one logged in before: the look's own theme" in text, text
        assert "font: DejaVu Serif" not in text, text
        wait_for_screen(lambda p: p == PLAIN, "the look's plain background", *PROBE)

    with subtest("alice's theme: her background, her error colour"):
        # The module made her directory: hers, the greeter's group, setgid.
        mode = machine.succeed(f"stat -c '%a %U %G' {THEME}").strip()
        assert mode == "2750 alice greeter", mode
        for name in ["background", "gtk.css", "settings.ini"]:
            install(name)
        machine.succeed(
            "printf 'last-user \"alice\"\\n' > /var/lib/rust-wl-greeter/state",
            "chown greeter:greeter /var/lib/rust-wl-greeter/state",
            "chmod 0600 /var/lib/rust-wl-greeter/state",
        )
        logged = restart_greetd()
        read = re.search(r"theme of alice read in ([0-9.]+) ms: (.*)$", logged, re.M)
        assert read, logged
        assert read.group(2) == "gtk.css, settings.ini, background 64×36", read.group(2)
        machine.log(f"the theme was read in {read.group(1)} ms")
        # Her font, as Pango finds it (not a fallback).
        assert "font: DejaVu Serif\n" in logged, logged
        wait_for_screen(lambda p: p == BLUE, "her background", *PROBE)
        # A wrong password: the field's outline in her error colour, for 1.5 s.
        wait_for_line("prompt: secret")
        machine.send_chars("wrong\n")
        # Looked for often from now on: it shows for 1.5 s, once PAM has failed it.
        def magenta():
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                pixels = area_pixels(EDGE)
                if any(p[0] > 160 and p[1] < 90 and p[2] > 160 for p in pixels):
                    return True
            return False
        assert magenta(), "no outline in the error colour"
        wait_for_line("wrong password")

    with subtest("a background that is a link, root's, or too big: the look's own"):
        for case, setup in [
            ("a link", f"ln -sf ${theme}/background {THEME}/background"),
            ("root's", f"install -o root -g greeter -m 0640 ${theme}/background {THEME}/"),
            ("too big", f"truncate -s 20M {THEME}/background && chown alice {THEME}/background"),
        ]:
            with machine.nested(case):
                machine.succeed(f"rm -f {THEME}/background", setup)
                logged = restart_greetd()
                assert f"ignoring {THEME}/background" in logged, logged
                # The rest still applies.
                rest = r"theme of alice read in [0-9.]+ ms: gtk.css, settings.ini$"
                assert re.search(rest, logged, re.M), logged
                wait_for_screen(lambda p: p == PLAIN, "the look's plain background", *PROBE)

    with subtest("an empty theme directory: the look's own"):
        machine.succeed(f"rm -f {THEME}/background {THEME}/gtk.css {THEME}/settings.ini")
        logged = restart_greetd()
        assert re.search(r"theme of alice read in [0-9.]+ ms: nothing$", logged, re.M), logged
        wait_for_screen(lambda p: p == PLAIN, "the look's plain background", *PROBE)

    print(read_log())
  '';
}
