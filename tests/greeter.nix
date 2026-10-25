# NixOS VM test: the greeter, under greetd, on its compositor, on two virtual outputs.
#
# The NixOS module (greeter-module.nix) sets greetd's command: rust-wl-greeter-compositor
# running rust-wl-greeter, whose config names the test's user, its sessions directory, the
# keyboard layout "de" (the compositor's own keymap is the default "us") and a look that
# puts the login widgets at known places. Both programs log to the journal, tagged
# rust-wl-greeter. The outputs are those of the compositor's test: Virtual-1 at 1024x768
# and scale 1.5 (683x512 logical), the primary one, and Virtual-2 at 800x600 and scale 1.
#
# Each session (sessions below) writes a marker file with its arguments and environment,
# then waits for the test to end it, after which greetd starts the greeter again.
#
# greetd's PAM stack starts with pam_google_authenticator (google-authenticator-libpam),
# with `echo_verification_code`, so that it asks for its code with a visible prompt, and
# `nullok`, so that it asks nothing of a user who has no ~/.google_authenticator. The
# login stack (pam_unix: the password, a secret prompt) follows. So the user has one
# prompt until the test gives them a TOTP secret, and two after.
#
# Behaviours asserted:
#   1. the greeter greets the configured user, by the full name; Hidden and NoDisplay
#      sessions are not offered; the first session by name is chosen; the login screen is
#      on the primary output and the other shows the background alone
#   2. a wrong password: a generic error line under the field, and greetd's description is
#      never logged; then the right password: the session's marker is written with its
#      arguments (Exec split, field codes dropped) and environment; the greeter exited by
#      itself (not by greetd's SIGTERM), and it and the compositor are gone while the
#      session runs
#   3. Escape at the prompt, and the "‹ Users" chip, cancel and start over (what was typed
#      is gone); the Log In button submits
#   4. the session popup, opened from the session chip: Escape dismisses it and changes
#      nothing (the chip looks the same, nothing is chosen); Down and Enter choose the next session, which starts; the state file
#      remembers it (mode 0600, owned by greeter)
#   5. after greetd restarts, the remembered session is preselected
#   6. two prompts: the visible verification code, then the password; a click in the popup
#      chooses a session
#   7. five wrong passwords in a row start the lock screen's 30 s cooldown, whose countdown
#      shows in the field's place; after it the right password logs in
#   8. the password typed is the one the greeter's own "de" keymap gives ("z" is typed
#      with the Y key); no password or code reaches the journal; the greeter is not
#      dumpable; `--check` accepts the config, rejects a bad one, saying where, and one
#      whose sessions directory has no sessions

{ pkgs }:

let
  inherit (pkgs) lib;
  user = "alice";
  # With a "z", typed with the Y key, as the "de" layout has it.
  password = "correct horze";
  wrongPassword = "wrong guess";
  otpSecret = "JBSWY3DPEHPK3PXP";
  dir = "/var/lib/greeter-test";

  # Writes its arguments and environment, then waits until the test ends it.
  session = pkgs.writeShellScript "session" ''
    echo "$# $* $XDG_SESSION_TYPE $XDG_CURRENT_DESKTOP $XDG_SESSION_DESKTOP" > ${dir}/session-$1
    while [ ! -e ${dir}/end ]; do ${pkgs.coreutils}/bin/sleep 0.1; done
    ${pkgs.coreutils}/bin/rm -f ${dir}/end
  '';

  # Aardvark and Aaa would come first by name, if they were offered.
  sessions = pkgs.runCommand "sessions" { } ''
    mkdir -p $out/share/wayland-sessions
    cd $out/share/wayland-sessions
    cat > beta.desktop <<'EOF'
    [Desktop Entry]
    Name=Beta
    Name[de]=Zweite
    Comment=The second session
    Exec=${session} beta "two words" %U
    DesktopNames=Beta;Shared;
    EOF
    cat > alpha.desktop <<'EOF'
    [Desktop Entry]
    Name=Alpha
    Exec=${session} alpha
    DesktopNames=Alpha
    EOF
    cat > aardvark.desktop <<'EOF'
    [Desktop Entry]
    Name=Aardvark
    Exec=${session} aardvark
    Hidden=true
    EOF
    cat > aaa.desktop <<'EOF'
    [Desktop Entry]
    Name=Aaa
    Exec=${session} aaa
    NoDisplay=true
    EOF
  '';

  otpFile = pkgs.writeText "google_authenticator" ''
    ${otpSecret}
    " TOTP_AUTH
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
  name = "greeter";

  nodes.machine =
    { config, ... }:
    {
      imports = [ ../greeter-module.nix ];

      virtualisation.memorySize = 1024;
      virtualisation.qemu.options = [
        "-vga none"
        "-device ${lib.escapeShellArg gpu}"
      ];

      users.users.${user} = {
        isNormalUser = true;
        description = "Alice Liddell";
        inherit password;
      };

      programs.rust-wl-greeter = {
        enable = true;
        inherit user;
        sessions = "${sessions}/share/wayland-sessions";
        keyboardLayout = "de";
        scales.Virtual-1 = 1.5;
        # On Virtual-1, in logical pixels: the field (176x30) is centred at the top, 220
        # from it, between the background's gradients.
        extraConfig = ''
          look {
              background
              avatar { anchor "top"; offset 0 100; }
              name { anchor "top"; offset 0 172; weight "semibold"; }
              password { anchor "top"; offset 0 220; }
          }
        '';
      };

      # The verification code first, as a visible prompt, for users who have a secret.
      security.pam.services.greetd.rules.auth.google_authenticator = {
        order = config.security.pam.services.greetd.rules.auth.login.order - 10;
        control = "required";
        modulePath = "${pkgs.google-authenticator}/lib/security/pam_google_authenticator.so";
        settings = {
          nullok = true;
          echo_verification_code = true;
        };
      };

      fonts.packages = [ pkgs.adwaita-fonts ];
      environment.systemPackages = [ pkgs.oath-toolkit ];
      systemd.tmpfiles.rules = [ "d ${dir} 0755 ${user} users -" ];
    };

  testScript = ''
    import re, time
    ${builtins.readFile ./screen.py}

    LOG = "journalctl --no-pager -o cat -t rust-wl-greeter"
    PASSWORD = "${password}"
    WRONG = "${wrongPassword}"
    DIR = "${dir}"
    STATE = "/var/lib/rust-wl-greeter/state"
    PLAIN = [0x20, 0x24, 0x28]
    # The layout, in logical pixels: Virtual-1 (683x512) then Virtual-2 (800x600).
    LAYOUT = (683 + 800, 600)
    SCALE = 1.5
    # Where the test's look and the greeter's fixed sizes put things on Virtual-1, in
    # logical pixels: the field, 176x30 at (253.5, 220); the Log In button, 80 wide, 12
    # right of it; the "‹ Users" chip, 80x24, 12 left of it; the error line, 20 high, 32
    # under it; the session chip, 24 high, 58 under it; the popup's rows, 28 high, from
    # 8 under the chip.
    FIELD = (253.5, 220, 176, 30)
    LOG_IN = (253.5 + 176 + 12 + 40, 235)
    USERS = (253.5 - 12 - 40, 235)
    MESSAGE = (253.5, 250 + 32, 176, 20)
    SESSION_CHIP = (341.5, 250 + 58 + 12)
    def popup_row(i):
        return (341.5, 250 + 58 + 24 + 8 + 28 * i + 14)

    def read_log():
        return machine.succeed(LOG)

    def count(text):
        return read_log().count(text)

    def wait_for_line(text, count=1, timeout=60):
        """Wait until `text` is in the greeter's log `count` times; print the log if it never is."""
        try:
            machine.wait_until_succeeds(f"test $({LOG} | grep -cF -- '{text}') -ge {count}", timeout=timeout)
        except Exception:
            print(read_log())
            raise

    def type_text(text):
        """Type `text` as the "de" layout reads the keys: Y and Z are swapped."""
        machine.send_chars(text.translate(str.maketrans("yzYZ", "zyZY")))

    def area(rect, head=0):
        """The pixels of a rectangle (logical x, y, w, h at SCALE), as RGB lists."""
        width, pixels = screen(head)
        x0, y0, w, h = [round(v * SCALE) for v in rect]
        rows = range(y0, y0 + h)
        return [list(pixels[(y * width + x) * 3:][:3]) for y in rows for x in range(x0, x0 + w)]

    def marked(rect, head=0):
        """How many pixels of a rectangle differ clearly from the plain background: text or
        shapes, not the gradients."""
        return sum(any(abs(p[c] - PLAIN[c]) > 40 for c in range(3)) for p in area(rect, head))

    def dots():
        """How many dots the field shows: runs of opaque white along its middle row, left of
        its button. The hint's text is at 0.75 opacity, under 200 over the background."""
        row = area((FIELD[0], FIELD[1] + FIELD[3] / 2, FIELD[2] - 30, 1 / SCALE))
        whites = [min(p) > 240 for p in row]
        return sum(white and not before for before, white in zip([False] + whites, whites))

    def wait_for_dots(n):
        """Typed keys arrive late (QEMU holds each 100 ms) and a click goes in at once: wait
        until the field shows all `n` characters."""
        with machine.nested(f"waiting for {n} dots"):
            retry(lambda _: dots() == n, 20)

    def session_marker(name):
        path = f"{DIR}/session-{name}"
        machine.wait_for_file(path)
        return machine.succeed(f"cat {path} && rm {path}").strip()

    def end_session():
        """End the running session; wait for the greeter's next password prompt."""
        n = count("prompt: secret")
        machine.succeed(f"touch {DIR}/end")
        wait_for_line("prompt: secret", n + 1)

    def logged_in(name, want, before):
        """Session `name` must start and write `want`, with the greeter having exited by
        itself (not by greetd's SIGTERM) since the log was `before`; then it is ended."""
        assert session_marker(name) == want, want
        machine.fail(f"pgrep -f {PROGRAMS}")
        # The compositor logs its client's own exit; a signal it would log as "SIGTERM".
        exited = "greeter exited: exit status: 0"
        wait_for_line(exited, before.count(exited) + 1)
        after = read_log()[len(before):]
        assert re.search(r"session \w+ started; exiting$", after, re.M), after
        assert "SIGTERM" not in after, after
        end_session()

    def log_in(name, want):
        """Type the password and Enter, as `logged_in` checks."""
        before = read_log()
        type_text(PASSWORD + "\n")
        logged_in(name, want, before)

    # The greeter and the compositor, by their program's path (not a shell's command line).
    PROGRAMS = "'^/nix/store/[^ ]*/bin/rust-wl-greeter'"
    ALPHA = "1 alpha wayland Alpha alpha"
    BETA = "2 beta two words wayland Beta:Shared beta"

    machine.start()
    machine.wait_for_unit("greetd.service")

    with subtest("the configured user, the sessions, and the login screen on the primary output"):
        wait_for_line("prompt: secret")
        wait_for_line("first frame on Virtual-1")
        wait_for_line("first frame on Virtual-2")
        text = read_log()
        assert "greeting alice (Alice Liddell)" in text, text
        assert "sessions: Alpha, Beta; Alpha chosen (first)" in text, text
        wait_for_screen(lambda p: p == PLAIN, "the plain background", 5, 384)
        # The avatar, name and hint are drawn on Virtual-1; Virtual-2 has the background alone.
        assert marked((0, 0, 1024 / SCALE, 768 / SCALE)) > 0
        wait_for_screen(lambda p: p == PLAIN, "Virtual-2's background", 400, 300, head=1)
        assert marked((0, 0, 800 / SCALE, 600 / SCALE), head=1) == 0
        assert marked(MESSAGE) == 0

    with subtest("a wrong password, then the right one"):
        n = count("prompt: secret")
        type_text(WRONG + "\n")
        wait_for_line("wrong password")
        # greetd's session is cancelled and created again: a new prompt.
        wait_for_line("prompt: secret", n + 1)
        retry(lambda _: marked(MESSAGE) > 0, 10)
        assert "AUTH_ERR" not in read_log(), "greetd's description was logged"
        log_in("alpha", ALPHA)
        assert "starting session Alpha (alpha.desktop)" in read_log()

    with subtest("Escape and the Users chip cancel; the Log In button submits"):
        n = count("prompt: secret")
        type_text("abc")
        machine.send_key("esc")
        wait_for_line("cancelled")
        wait_for_line("prompt: secret", n + 1)
        # The hint ("Password") counts for none.
        assert dots() == 0, dots()
        type_text("abc")
        wait_for_dots(3)
        click(*USERS)
        wait_for_line("cancelled", 2)
        wait_for_line("prompt: secret", n + 2)
        # Nothing typed before the cancel is left over.
        before = read_log()
        assert dots() == 0, dots()
        type_text(PASSWORD)
        wait_for_dots(len(PASSWORD))
        click(*LOG_IN)
        logged_in("alpha", ALPHA, before)

    with subtest("the session popup: Escape dismisses it, Down and Enter choose"):
        click(*SESSION_CHIP)
        wait_for_line("session popup shown")
        # Above the popup, with the pointer (drawn by the compositor) on it already.
        chip = (341.5 - 60, 250 + 58, 120, 24)
        before = area(chip)
        machine.send_key("esc")
        wait_for_line("session popup dismissed")
        # Nothing chosen: the chip still shows Alpha.
        assert not re.search(r"session \w+ chosen$", read_log(), re.M)
        assert area(chip) == before, "the session chip changed"
        click(*SESSION_CHIP)
        wait_for_line("session popup shown", 2)
        machine.send_key("down")
        machine.send_key("ret")
        wait_for_line("session Beta chosen")
        log_in("beta", BETA)
        state = machine.succeed(f"cat {STATE}")
        assert 'session "alice" "beta.desktop"' in state, state
        assert machine.succeed(f"stat -c '%a %U' {STATE}").strip() == "600 greeter"

    with subtest("after greetd restarts, the remembered session is preselected"):
        n = count("prompt: secret")
        machine.succeed("systemctl restart greetd")
        wait_for_line("prompt: secret", n + 1)
        wait_for_line("sessions: Alpha, Beta; Beta chosen (remembered)")
        log_in("beta", BETA)

    with subtest("two prompts: a visible verification code, then the password"):
        machine.succeed("install -o alice -m 0400 ${otpFile} /home/alice/.google_authenticator")
        # Alpha, by a click on its row.
        click(*SESSION_CHIP)
        wait_for_line("session popup shown", 3)
        click(*popup_row(0))
        wait_for_line("session Alpha chosen")
        # The session greetd has was created before the secret: start over.
        n = count("prompt: visible")
        machine.send_key("esc")
        wait_for_line("prompt: visible", n + 1)
        code = machine.succeed("oathtool --totp -b ${otpSecret}").strip()
        prompts = count("prompt: secret")
        type_text(code + "\n")
        wait_for_line("prompt: secret", prompts + 1)
        # Past the code: the next greeter asks for the password alone again.
        machine.succeed("rm /home/alice/.google_authenticator")
        log_in("alpha", ALPHA)

    with subtest("five wrong passwords: a countdown in the field, then the right one"):
        for _ in range(5):
            prompts, wrong = count("prompt: secret"), count("wrong password")
            type_text(WRONG + "\n")
            wait_for_line("wrong password", wrong + 1)
            wait_for_line("prompt: secret", prompts + 1)
        wait_for_line("5 wrong passwords; waiting 30 s")
        # "Try again in N s" where the field was: in a cooldown, nothing else is drawn there.
        retry(lambda _: marked(FIELD) > 0, 10)
        # The lock screen's cooldown is fixed at 30 s.
        time.sleep(31)
        log_in("alpha", ALPHA)

    with subtest("no secret in the journal; not dumpable; --check"):
        journal = machine.succeed("journalctl --no-pager -o cat")
        swapped = PASSWORD.translate(str.maketrans("yz", "zy"))
        for secret in [PASSWORD, swapped, WRONG, code]:
            assert secret not in journal, f"{secret!r} in the journal"
        pid = machine.succeed("pgrep -f '^/nix/store/[^ ]*/bin/rust-wl-greeter --config'").strip()
        machine.fail(f"sudo -u greeter ls /proc/{pid}/fd")
        command = machine.succeed(f"tr '\\0' ' ' < /proc/{pid}/cmdline").split()
        config = command[command.index("--config") + 1]
        machine.succeed(f"{command[0]} --check {config}")
        machine.succeed("echo 'user 1' > /root/bad.kdl")
        out = machine.fail(f"{command[0]} --check /root/bad.kdl 2>&1")
        assert "at line 1" in out, out
        # A config whose sessions directory has none fails too.
        machine.succeed("mkdir /root/no-sessions && echo 'user \"alice\"; sessions \"/root/no-sessions\"' > /root/empty.kdl")
        out = machine.fail(f"{command[0]} --check /root/empty.kdl 2>&1")
        assert "no sessions in /root/no-sessions" in out, out

    print(read_log())
  '';
}
