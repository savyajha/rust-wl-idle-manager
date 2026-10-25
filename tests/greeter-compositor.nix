# NixOS VM test: the greeter compositor under greetd, on two virtual outputs.
#
# greetd runs rust-wl-greeter-compositor as its greeter (user `greeter`, VT 1), with the
# test client as the greeter program. Both log to /var/lib/greeter-test/log: the
# compositor's timing lines (--log-timing) and one line per event the client sees. The
# GPU is a virtio-gpu without virgl, with two outputs enabled from the start: Virtual-1
# at 1024x768 and scale 1.5 (683x512 logical), Virtual-2 at 800x600 and scale 1, to its
# right. Keys go in with QMP send-key; the pointer with QMP input-send-event on the USB
# tablet, whose range covers the whole layout. The screen (Virtual-1) is read with
# QEMU's screendump.
#
# Behaviours asserted:
#   1. the client's first frame is presented on both outputs, each at its own scale,
#      and the keyboard starts on the primary output's toplevel
#   2. a key reaches the focused toplevel; a click reaches the surface under the
#      pointer and moves the keyboard there; the cursor shows from the first motion on;
#      an absolute position in a gap of the layout lands on the nearest output
#   3. a grabbing popup is slid back onto its output, takes the keyboard, and is
#      dismissed by Escape and by a click outside it, neither of which (press or
#      release) reaches the client; the keyboard goes back to the toplevel; a click
#      inside the popup reaches it
#   4. switching to VT 2 (chvt) and back redraws both outputs: what the client drew
#      while the VT was away shows on both; Ctrl+Alt+F2 and Ctrl+Alt+F1 switch too
#   5. an output unplugged (its connector forced off through sysfs, and the card's change
#      event sent) while its toplevel has the keyboard: the toplevel is moved to the
#      primary output, beneath the primary output's own, which gets the keyboard back;
#      the pointer is moved onto the primary output; plugged again, the client's
#      set_fullscreen moves the toplevel back. (Forcing the status makes the kernel probe
#      the connector itself, so this does not show that the compositor probes it.)
#   6. greetd starts the compositor again after the client exits
#   7. SIGTERM: the compositor stops the client and exits within 500 ms; the exit time
#      is printed
#   8. the binary links no EGL, GBM, Mesa, LLVM or GL

{
  pkgs,
  compositor,
  testClient,
}:

let
  inherit (pkgs) lib;
  log = "/var/lib/greeter-test/log";

  greeter = pkgs.writeShellScript "greeter" ''
    exec >>${log} 2>&1
    echo "== greeter start"
    exec ${lib.getExe compositor} --log-timing --scale Virtual-1=1.5 \
      -- ${lib.getExe testClient}
  '';

  gpu = builtins.toJSON {
    driver = "virtio-gpu-pci";
    id = "gpu";
    max_outputs = 2;
    outputs = [
      {
        name = "left";
        xres = 1024;
        yres = 768;
      }
      {
        name = "right";
        xres = 800;
        yres = 600;
      }
    ];
  };
in
pkgs.testers.runNixOSTest {
  name = "greeter-compositor";

  nodes.machine = {
    virtualisation.memorySize = 1024;
    virtualisation.qemu.options = [
      "-vga none"
      "-device ${lib.escapeShellArg gpu}"
    ];
    services.greetd = {
      enable = true;
      settings.default_session.command = "${greeter}";
    };
    # logind lets an active session switch VT (libseat's SwitchTo) through polkit only.
    security.polkit.enable = true;
    systemd.tmpfiles.rules = [ "d /var/lib/greeter-test 0755 greeter greeter -" ];
    environment.systemPackages = [ pkgs.kbd ];
  };

  testScript = ''
    import re, time
    ${builtins.readFile ./screen.py}

    LOG = "${log}"
    CLIENT = "'^/nix/store/[^ ]*/bin/rust-wl-greeter-test-client'"
    COLOUR_1 = [0x33, 0x66, 0xcc]
    # After SIGUSR1, the client's colours inverted.
    INVERTED_1 = [0xcc, 0x99, 0x33]
    INVERTED_2 = [0x33, 0x99, 0xcc]
    WHITE, BLACK = [0xff, 0xff, 0xff], [0, 0, 0]
    # The layout, in logical pixels: Virtual-1 (683x512) then Virtual-2 (800x600).
    LAYOUT = (683 + 800, 600)

    def read_log():
        return machine.succeed(f"cat {LOG}")

    def count(text):
        return read_log().count(text)

    def wait_for_line(text, count=1, timeout=30):
        """Wait until `text` is in the log `count` times; print the log if it never is."""
        try:
            machine.wait_until_succeeds(f"test $(grep -cF -- '{text}' {LOG}) -ge {count}", timeout=timeout)
        except Exception:
            print(read_log())
            raise

    def lines_after(marker):
        """The log after the last line containing `marker`."""
        text = read_log()
        return text[text.rindex(marker):]

    def settle():
        """Long enough for anything still on its way (QEMU holds a key 100 ms) to arrive."""
        time.sleep(0.5)

    machine.start()
    machine.wait_for_unit("greetd.service")

    with subtest("first frame on both outputs, each at its own scale"):
        wait_for_line("presented Virtual-1")
        wait_for_line("presented Virtual-2")
        wait_for_line("input ready")
        text = read_log()
        assert "configure Virtual-1 683x512 scale 180" in text, text
        assert "configure Virtual-2 800x600 scale 120" in text, text
        assert "keyboard on Virtual-1" in text, text
        print(machine.succeed(f"grep -E 'ms: |output ' {LOG}"))
        wait_for_screen(lambda p: p == COLOUR_1, "the client's colour")
        # No cursor before the pointer moves: where its arrow would be, the client's colour.
        assert screen_pixel(6, 16) == COLOUR_1

    with subtest("a key and a click reach the client"):
        machine.send_key("a")
        wait_for_line("key 30 pressed on Virtual-1")
        wait_for_line("key 30 released on Virtual-1")
        click(683 + 400, 300)
        wait_for_line("button 272 pressed on Virtual-2 at 400,300")
        wait_for_line("button 272 released on Virtual-2")
        wait_for_line("keyboard on Virtual-2")
        click(300, 300)
        wait_for_line("button 272 pressed on Virtual-1 at 300,300")
        # The arrow at the pointer, at scale 1.5 drawn twice its size: its white fill, 16 px
        # down. Drawn into the frame here: virtio-gpu offers no cursor plane (see drm.rs).
        assert "scale 1.5, software cursor" in read_log(), read_log()
        wait_for_screen(lambda p: p == WHITE, "the cursor", 300 * 3 // 2 + 6, 300 * 3 // 2 + 16)
        wait_for_line("keyboard on Virtual-1", count=2)
        # Below Virtual-1, which is shorter than Virtual-2: a gap in the tablet's range. The
        # pointer goes to Virtual-1's bottom row; the arrow's tip in the screen's last row.
        click(300, 580)
        wait_for_line("button 272 pressed on Virtual-1 at 300,511")
        wait_for_screen(lambda p: p == BLACK, "the cursor in the last row", 450, 767)

    with subtest("a popup: placed on the output, dismissed by Escape"):
        machine.send_key("p")
        wait_for_line("presented popup")
        # Anchored 50 px from the right edge, 200 px wide: slid left to fit.
        assert "popup at 483,100 200x100" in read_log(), read_log()
        wait_for_line("keyboard on popup")
        machine.send_key("a")
        wait_for_line("key 30 pressed on popup")
        machine.send_key("esc")
        wait_for_line("popup_done")
        wait_for_line("keyboard on Virtual-1", count=3)
        settle()
        assert "key 1 " not in lines_after("popup at"), "Escape reached the client"

    with subtest("a popup: a click inside reaches it, a click outside dismisses it"):
        machine.send_key("p")
        wait_for_line("presented popup", count=2)
        click(583, 150)
        wait_for_line("button 272 pressed on popup at 100,50")
        click(100, 400)
        wait_for_line("popup_done", count=2)
        wait_for_line("keyboard on Virtual-1", count=4)
        settle()
        assert "at 100,400" not in read_log(), "the outside click reached the client"

    with subtest("VT switch away and back redraws both outputs"):
        machine.succeed("chvt 2")
        wait_for_line("session paused")
        wait_for_screen(lambda p: p != COLOUR_1, "the text console")
        # The client draws while the VT is away; nothing of it may show until it is back.
        machine.succeed(f"pkill -USR1 -f {CLIENT}")
        wait_for_line("recoloured")
        settle()
        assert screen_pixel(512, 384) != INVERTED_1, "drawn while the VT was away"
        machine.succeed("chvt 1")
        wait_for_line("session active: redrawing 2 outputs in full")
        wait_for_screen(lambda p: p == INVERTED_1, "Virtual-1 redrawn")
        wait_for_screen(lambda p: p == INVERTED_2, "Virtual-2 redrawn", 400, 300, head=1)

    with subtest("Ctrl+Alt+F2 and Ctrl+Alt+F1 switch VT"):
        machine.send_key("ctrl-alt-f2")
        wait_for_line("session paused", count=2)
        wait_for_screen(lambda p: p != INVERTED_1, "the text console")
        machine.send_key("ctrl-alt-f1")
        wait_for_line("session active", count=2)
        wait_for_screen(lambda p: p == INVERTED_1, "the client")

    with subtest("an output unplugged and plugged again"):
        # Forcing a connector's status sends no uevent; the change event a real hotplug
        # sends on the card is sent through its uevent file.
        def plug(status):
            machine.succeed(f"echo {status} > /sys/class/drm/card0-Virtual-2/status")
            machine.succeed("echo change > /sys/class/drm/card0/uevent")
        # The keyboard on Virtual-2's toplevel, by a click, and the pointer there.
        click(683 + 400, 300)
        wait_for_line("keyboard on Virtual-2", count=2)
        on_1 = count("keyboard on Virtual-1")
        plug("off")
        wait_for_line("output Virtual-2 disconnected")
        wait_for_line("output removed Virtual-2")
        # Its toplevel, kept by the client, moves to Virtual-1 at its size and scale...
        wait_for_line("configure Virtual-2 683x512 scale 180")
        # ...beneath Virtual-1's own toplevel, which gets the keyboard back.
        wait_for_screen(lambda p: p == INVERTED_1, "Virtual-1's own toplevel on top")
        wait_for_line("keyboard on Virtual-1", count=on_1 + 1)
        # The pointer is at Virtual-1's right edge: the arrow's outline in the last column.
        wait_for_screen(lambda p: p == BLACK, "the cursor in the last column", 1023, 460)
        # The pointer, which was on Virtual-2, is at Virtual-1's right edge: a click there
        # without moving reaches Virtual-1's toplevel.
        for down in [True, False]:
            input_events({"type": "btn", "data": {"down": down, "button": "left"}})
        wait_for_line("button 272 pressed on Virtual-1 at 682,300")
        plug("detect")
        wait_for_line("output added Virtual-2", count=2)
        # The client asks for the kept toplevel on the new output: found and moved there.
        wait_for_line("configure Virtual-2 800x600 scale 120", count=2)
        wait_for_screen(lambda p: p == INVERTED_2, "Virtual-2 back", 400, 300, head=1)

    with subtest("greetd starts the compositor again after the client exits"):
        machine.send_key("q")
        wait_for_line("greeter exited: exit status: 0")
        wait_for_line("== greeter start", count=2)
        retry(lambda _: "presented Virtual-1" in lines_after("== greeter start"), 30)
        retry(lambda _: "input ready" in lines_after("== greeter start"), 30)

    with subtest("SIGTERM: the client is stopped, and the compositor's exit time"):
        us, client = machine.succeed("""
          pid=$(pgrep -f '^/nix/store/[^ ]*/bin/rust-wl-greeter-compositor ')
          client=$(pgrep -f """ + CLIENT + """)
          # Builtins only from here on, so the shell measures without forking.
          start=''${EPOCHREALTIME/./}
          kill -TERM "$pid"
          # Until it is gone or a zombie (exited, not yet reaped).
          while read -r stat < /proc/$pid/stat && [[ $stat != *") Z "* ]]; do :; done 2>/dev/null
          end=''${EPOCHREALTIME/./}
          # The compositor reaps the client before it exits.
          echo $(( end - start )) $([ -e /proc/$client ] && echo alive || echo gone)
        """).split()
        print(f"exit time after SIGTERM: {int(us) / 1000:.1f} ms")
        wait_for_line("SIGTERM: exiting")
        exit_lines = machine.succeed(f"grep -E 'after the signal|did not exit' {LOG}")
        print(exit_lines)
        assert "did not exit" not in exit_lines, "the client had to be killed"
        assert int(us) < 500_000, f"exit took {us} us"
        assert client == "gone", "the client outlived the compositor"
        wait_for_line("== greeter start", count=3)
        retry(lambda _: "presented Virtual-1" in lines_after("== greeter start"), 30)

    with subtest("no EGL, GBM, Mesa, LLVM or GL"):
        libs = machine.succeed("${pkgs.glibc.bin}/bin/ldd ${lib.getExe compositor}")
        print(libs)
        assert not re.search(r"libEGL|libgbm|mesa|LLVM|libGL[.X]|libglapi|libGLdispatch", libs), libs

    print(read_log())
  '';
}
