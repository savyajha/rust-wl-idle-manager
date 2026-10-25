# NixOS VM test: the greeter, under greetd, on its compositor, on two virtual outputs.
#
# The NixOS module (greeter-module.nix) sets greetd's command: rust-wl-greeter-compositor
# running rust-wl-greeter, whose config names the test's sessions directory, the
# keyboard layout "de" (the compositor's own keymap is the default "us") and a look that
# puts the login widgets at known places. Both programs log to the journal, tagged
# rust-wl-greeter. The outputs are those of the compositor's test: Virtual-1 at 1024x768
# and scale 1.5 (683x512 logical), the primary one, and Virtual-2 at 800x600 and scale 1.
#
# Each session (sessions below) writes a marker file with its arguments and environment,
# owned by the user it runs as, then waits for the test to end it, after which greetd
# starts the greeter again.
#
# The machine has one user who can log in, alice, until the test adds bob (and carol, whose
# shell is nologin, and dave, whose UID, 40010, is past login.defs's UID_MAX, 29999: neither
# is listed).
#
# greetd's PAM stack starts with pam_google_authenticator (google-authenticator-libpam),
# with `echo_verification_code`, so that it asks for its code with a visible prompt, and
# `nullok`, so that it asks nothing of a user who has no ~/.google_authenticator. The
# login stack (pam_unix: the password, a secret prompt) follows. So the user has one
# prompt until the test gives them a TOTP secret, and two after.
#
# Behaviours asserted:
#   1. with one user, the greeter greets them by the full name at once, with no avatar row
#      but the "‹ Users" chip; Hidden and NoDisplay sessions are not offered; the first
#      session by name is chosen; the login screen is on the primary output and the other
#      shows the background alone
#   2. a wrong password: a generic error line under the field, and greetd's description is
#      never logged; then the right password: the session's marker is written with its
#      arguments (Exec split, field codes dropped) and environment; the greeter exited by
#      itself (not by greetd's SIGTERM), and it and the compositor are gone while the
#      session runs
#   3. Escape at the prompt goes back to the row of the one user and "Other…" (what was
#      typed is gone); Enter picks her again; the Log In button submits
#   4. keyboard focus: no ring until a key moves the focus; Tab moves it to the "‹ Users"
#      chip, then the session chip; typing there goes to the field (and the focus with it);
#      Tab cycles to Log In and around, Shift+Tab back, and Enter on Log In submits
#   5. the session popup, opened from the session chip: Escape dismisses it and changes
#      nothing (the chip looks the same, nothing is chosen, a click shows no focus ring);
#      Down and Enter choose the next session, which starts; the state file remembers it
#      and the last user (mode 0600, owned by greeter)
#   6. after greetd restarts, the remembered session is preselected
#   7. two prompts: the visible verification code, then the password; a click in the popup
#      chooses a session
#   8. "Other…" from the one user's login, by a click on its avatar: a password typed as a
#      user name is never logged; Escape at its prompt goes back to the user name; root,
#      typed there, logs in (greetd's PAM stack lets root in with a password)
#   9. five wrong passwords in a row start the lock screen's 30 s cooldown, whose countdown
#      shows in the field's place; after it the right password logs in
#  10. two users: the avatar row shows both (not carol or dave), with no greetd session
#      and no focus ring until a key; Right and Enter pick the second, bob; Space on the
#      session chip opens the popup; bob logs in to the session chosen there
#  11. after greetd restarts, the last user (bob) is preselected, with his own session;
#      Escape at his prompt, and the "‹ Users" chip, go back to the row (what was typed is
#      gone); a click on alice's avatar picks her, with her own session
#  12. "Other…" asks for a user name: dave, who is not listed, logs in, not logged by name
#  13. tuigreet's lastuser seeds the last user when the state has none; not when it has
#      one (one not listed starts the row on "Other…"); not when it names no listed user
#  14. no one listed (login.defs's range matching no one): the user name at once, with no
#      "‹ Users" chip; Escape at a mistyped user's prompt goes back to the user name
#  15. the password typed is the one the greeter's own "de" keymap gives ("z" is typed
#      with the Y key); no password or code reaches the journal, and a password typed
#      as a user name doesn't reach the greeter's log; the greeter is not dumpable;
#      `--check` accepts the config, rejects a bad one, saying where, and rejects one
#      whose sessions directory has no sessions

{ pkgs }:

let
  inherit (pkgs) lib;
  user = "alice";
  # With a "z", typed with the Y key, as the "de" layout has it.
  password = "correct horze";
  bobPassword = "bob builds";
  davePassword = "dave digs";
  rootPassword = "root rules";
  # What someone might type for "Other…"'s user name by mistake.
  notAName = "hunter2 not a name";
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
      # Every user's sessions write their markers here, and remove the test's end file.
      systemd.tmpfiles.rules = [ "d ${dir} 0777 root root -" ];
    };

  testScript = ''
    import re, time
    ${builtins.readFile ./screen.py}

    LOG = "journalctl --no-pager -o cat -t rust-wl-greeter"
    PASSWORD = "${password}"
    BOB_PASSWORD = "${bobPassword}"
    DAVE_PASSWORD = "${davePassword}"
    ROOT_PASSWORD = "${rootPassword}"
    NOT_A_NAME = "${notAName}"
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
    # The "‹ Users" chip's rectangle.
    USERS_CHIP = (253.5 - 12 - 80, 223, 80, 24)
    # Strips of the focus rings (2 wide, 2 to 4 outside a widget) of the field and Log In,
    # above them, and of the session chip, above it (under the message line).
    FIELD_RING = (321.5, 216.5, 40, 1)
    LOG_IN_RING = (461.5, 216.5, 40, 1)
    SESSION_RING = (331.5, 304.5, 20, 1)
    # The avatar row, where the test's look puts the avatar: circles 64 across centred 132
    # from the top, 120 apart, centred on the screen; "Other…" is last of `n`.
    def avatar_box(i, n=3):
        x = 341.5 + (i - (n - 1) / 2) * 120
        return (x - 32, 100, 64, 64)
    def avatar_center(i, n=3):
        x, y, w, h = avatar_box(i, n)
        return (x + w / 2, y + h / 2)
    def avatar_ring(i, n=3):
        """A strip of avatar i's focus ring, at its top. The row is in the top gradient, so
        rings are found by comparing brightness, not by `marked`."""
        x = 341.5 + (i - (n - 1) / 2) * 120
        return (x - 3, 100 - 3.5, 6, 1)

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

    def brightness(rect):
        return sum(sum(p) for p in area(rect))

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
        """The marker session `name` wrote: its owner, a newline, and its text."""
        path = f"{DIR}/session-{name}"
        machine.wait_for_file(path)
        return machine.succeed(f"stat -c %U {path} && cat {path} && rm {path}").strip()

    def end_session(ready):
        """End the running session; wait until the next greeter logs `ready` once more."""
        n = count(ready)
        machine.succeed(f"touch {DIR}/end")
        wait_for_line(ready, n + 1)

    def restart_greetd():
        """Restart greetd; wait until the new greeter's compositor takes input."""
        n = count("input ready")
        machine.succeed("systemctl restart greetd")
        wait_for_line("input ready", n + 1)

    def logged_in(name, want, before, user="alice", ready="prompt: secret"):
        """Session `name` must start as `user` and write `want`, with the greeter having
        exited by itself (not by greetd's SIGTERM) since the log was `before`; then it is
        ended, and the next greeter waited for until it logs `ready`."""
        marker = session_marker(name)
        assert marker == f"{user}\n{want}", marker
        machine.fail(f"pgrep -f {PROGRAMS}")
        # The compositor logs its client's own exit; a signal it would log as "SIGTERM".
        exited = "greeter exited: exit status: 0"
        wait_for_line(exited, before.count(exited) + 1)
        after = read_log()[len(before):]
        assert re.search(r"session \w+ started; exiting$", after, re.M), after
        assert "SIGTERM" not in after, after
        end_session(ready)

    def log_in(name, want, password=PASSWORD, user="alice", ready="prompt: secret"):
        """Type the password and Enter, as `logged_in` checks."""
        before = read_log()
        type_text(password + "\n")
        logged_in(name, want, before, user, ready)

    # The greeter and the compositor, by their program's path (not a shell's command line).
    PROGRAMS = "'^/nix/store/[^ ]*/bin/rust-wl-greeter'"
    ALPHA = "1 alpha wayland Alpha alpha"
    BETA = "2 beta two words wayland Beta:Shared beta"

    machine.start()
    machine.wait_for_unit("greetd.service")

    with subtest("one user, the sessions, and the login screen on the primary output"):
        wait_for_line("prompt: secret")
        wait_for_line("first frame on Virtual-1")
        wait_for_line("first frame on Virtual-2")
        text = read_log()
        # Only alice is listed (not root, the greeter or other system users): straight to
        # her prompt, with no avatar row beside her avatar, but "‹ Users" for one.
        assert re.search(r"users: Alice Liddell$", text, re.M), text
        assert "greeting alice (Alice Liddell)" in text, text
        assert "sessions: Alpha, Beta; Alpha chosen (first)" in text, text
        wait_for_screen(lambda p: p == PLAIN, "the plain background", 5, 384)
        # The avatar, name and hint are drawn on Virtual-1; Virtual-2 has the background alone.
        assert marked((0, 0, 1024 / SCALE, 768 / SCALE)) > 0
        wait_for_screen(lambda p: p == PLAIN, "Virtual-2's background", 400, 300, head=1)
        assert marked((0, 0, 800 / SCALE, 600 / SCALE), head=1) == 0
        assert marked(MESSAGE) == 0
        # Where subtest 10 finds avatars with two users.
        assert marked(avatar_box(0)) == 0 and marked(avatar_box(2)) == 0
        assert marked(USERS_CHIP) > 0

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

    with subtest("Escape goes back to the row of one user; the Log In button submits"):
        n = count("prompt: secret")
        type_text("abc")
        wait_for_dots(3)
        machine.send_key("esc")
        wait_for_line("cancelled; back to the users")
        # alice and "Other…", and no field.
        retry(lambda _: marked(FIELD) == 0, 10)
        assert marked(avatar_box(0, n=2)) > 0 and marked(avatar_box(1, n=2)) > 0
        # On alice: Enter picks her again.
        machine.send_key("ret")
        wait_for_line("prompt: secret", n + 1)
        # Nothing typed before the cancel is left over; the hint ("Password") counts for none.
        assert dots() == 0, dots()
        before = read_log()
        type_text(PASSWORD)
        wait_for_dots(len(PASSWORD))
        click(*LOG_IN)
        logged_in("alpha", ALPHA, before)

    with subtest("keys move the focus ring; typing goes to the field; Enter on Log In submits"):
        # A new greeter, its focus on the field: no ring until a key moves it.
        assert marked(FIELD_RING) == 0 and marked(LOG_IN_RING) == 0
        assert marked(SESSION_RING) == 0
        # The field, "‹ Users", the session chip, Log In.
        machine.send_key("tab")
        machine.send_key("tab")
        retry(lambda _: marked(SESSION_RING) > 0, 10)
        assert marked(FIELD_RING) == 0 and marked(LOG_IN_RING) == 0
        # Typed with the focus on the chip, the password goes to the field, and the focus
        # (its ring still shown) with it.
        type_text(PASSWORD)
        wait_for_dots(len(PASSWORD))
        assert marked(SESSION_RING) == 0 and marked(FIELD_RING) > 0
        for _ in range(3):
            machine.send_key("tab")
        retry(lambda _: marked(LOG_IN_RING) > 0, 10)
        assert marked(SESSION_RING) == 0 and marked(FIELD_RING) == 0
        # Tab goes round to the field, Shift+Tab back to Log In.
        machine.send_key("tab")
        retry(lambda _: marked(FIELD_RING) > 0 and marked(LOG_IN_RING) == 0, 10)
        machine.send_key("shift-tab")
        retry(lambda _: marked(LOG_IN_RING) > 0 and marked(FIELD_RING) == 0, 10)
        # Moving the focus typed nothing; Enter, on Log In, submits.
        assert dots() == len(PASSWORD), dots()
        before = read_log()
        machine.send_key("ret")
        logged_in("alpha", ALPHA, before)

    with subtest("the session popup: Escape dismisses it, Down and Enter choose"):
        click(*SESSION_CHIP)
        wait_for_line("session popup shown")
        # Above the popup, with the pointer (drawn by the compositor) on it already.
        chip = (341.5 - 60, 250 + 58, 120, 24)
        before = area(chip)
        machine.send_key("esc")
        wait_for_line("session popup dismissed")
        # Nothing chosen: the chip still shows Alpha. A click shows no focus ring.
        assert not re.search(r"session \w+ chosen$", read_log(), re.M)
        assert area(chip) == before, "the session chip changed"
        assert marked(SESSION_RING) == 0
        click(*SESSION_CHIP)
        wait_for_line("session popup shown", 2)
        machine.send_key("down")
        machine.send_key("ret")
        wait_for_line("session Beta chosen")
        log_in("beta", BETA)
        state = machine.succeed(f"cat {STATE}")
        assert 'session "alice" "beta.desktop"' in state, state
        assert 'last-user "alice"' in state, state
        assert machine.succeed(f"stat -c '%a %U' {STATE}").strip() == "600 greeter"

    with subtest("after greetd restarts, the remembered session is preselected"):
        n = count("prompt: secret")
        machine.succeed("systemctl restart greetd")
        wait_for_line("prompt: secret", n + 1)
        wait_for_line("sessions: Alpha, Beta; Beta chosen (remembered)")
        log_in("beta", BETA)

    with subtest("two prompts: a visible verification code, then the password"):
        machine.succeed("install -o alice -m 0400 ${otpFile} /home/alice/.google_authenticator")
        # The session greetd has was created before the secret: start over, by the row.
        n = count("prompt: visible")
        machine.send_key("esc")
        machine.send_key("ret")
        wait_for_line("prompt: visible", n + 1)
        # Alpha, by a click on its row. (Going back resets it to her last, Beta.)
        click(*SESSION_CHIP)
        wait_for_line("session popup shown", 3)
        click(*popup_row(0))
        wait_for_line("session Alpha chosen")
        code = machine.succeed("oathtool --totp -b ${otpSecret}").strip()
        prompts = count("prompt: secret")
        type_text(code + "\n")
        wait_for_line("prompt: secret", prompts + 1)
        # Past the code: the next greeter asks for the password alone again.
        machine.succeed("rm /home/alice/.google_authenticator")
        log_in("alpha", ALPHA)

    with subtest("Other… from one user's login: root logs in"):
        # greetd's PAM stack, NixOS's login, has no pam_securetty or pam_access: nothing
        # there keeps root out.
        machine.succeed("test -e /etc/pam.d/greetd -a -e /etc/pam.d/login")
        machine.fail("grep -E 'securetty|pam_access' /etc/pam.d/greetd /etc/pam.d/login")
        print(machine.succeed("grep '^auth' /etc/pam.d/greetd /etc/pam.d/login"))
        machine.succeed(f"echo 'root:{ROOT_PASSWORD}' | chpasswd")
        backs, asked = count("back to the users"), count("asking for a user name")
        prompts, unlisted = count("prompt: secret"), count("greeting a user who is not listed")
        codes = count("prompt: visible")
        click(*USERS)
        wait_for_line("back to the users", backs + 1)
        # The row of alice and "Other…": a click on "Other…".
        retry(lambda _: marked(avatar_box(1, n=2)) > 0, 10)
        click(*avatar_center(1, n=2))
        wait_for_line("asking for a user name", asked + 1)
        type_text(NOT_A_NAME + "\n")
        wait_for_line("greeting a user who is not listed", unlisted + 1)
        # pam_google_authenticator asks a user who does not exist for a code anyway.
        wait_for_line("prompt: visible", codes + 1)
        # Escape: back to the user name, to type it again.
        machine.send_key("esc")
        wait_for_line("cancelled; back to the user name")
        type_text("root\n")
        wait_for_line("greeting a user who is not listed", unlisted + 2)
        wait_for_line("prompt: secret", prompts + 1)
        log_in("alpha", ALPHA, password=ROOT_PASSWORD, user="root")

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

    with subtest("two users: the avatar row; Right and Enter pick the second, who logs in"):
        machine.succeed(
            "useradd -m -c 'Bob Builder' bob",
            "useradd -m -c 'Carol Nologin' -s /run/current-system/sw/bin/nologin carol",
            "useradd -m -u 40010 -c 'Dave Unlisted' dave",
            f"echo 'bob:{BOB_PASSWORD}' | chpasswd",
            f"echo 'dave:{DAVE_PASSWORD}' | chpasswd",
        )
        before = read_log()
        restart_greetd()
        after = read_log()[len(before):]
        # Sorted by full name; carol (nologin) and dave (UID 40010) are not listed.
        assert re.search(r"users: Alice Liddell, Bob Builder$", after, re.M), after
        assert "last user alice (remembered)" in after, after
        # The row: alice, bob and "Other…", each drawn; no field and no chips; and no
        # session asked of greetd until a user is chosen.
        for i in range(3):
            assert marked(avatar_box(i)) > 0, i
        assert marked(FIELD) == 0 and marked(USERS_CHIP) == 0
        assert "greeting" not in after, after
        # No ring yet (the strips, at the same height over the same background, are alike);
        # Right moves the focus from alice to bob, and his ring shows.
        rings = [brightness(avatar_ring(i)) for i in range(3)]
        assert rings[0] == rings[1] == rings[2], rings
        machine.send_key("right")
        retry(lambda _: brightness(avatar_ring(1)) > rings[1] + 500, 10)
        assert brightness(avatar_ring(0)) == rings[0] and brightness(avatar_ring(2)) == rings[2]
        prompts = count("prompt: secret")
        machine.send_key("ret")
        wait_for_line("greeting bob (Bob Builder)")
        wait_for_line("sessions: Alpha, Beta; Alpha chosen (first)", 2)
        wait_for_line("prompt: secret", prompts + 1)
        # His login has the "‹ Users" chip; a key chose him, so the field shows the ring.
        retry(lambda _: marked(USERS_CHIP) > 0 and marked(FIELD_RING) > 0, 10)
        # The field, "‹ Users", the session chip: Space there opens the popup.
        popups, chosen = count("session popup shown"), count("session Beta chosen")
        machine.send_key("tab")
        machine.send_key("tab")
        retry(lambda _: marked(SESSION_RING) > 0, 10)
        machine.send_key("spc")
        wait_for_line("session popup shown", popups + 1)
        machine.send_key("down")
        machine.send_key("ret")
        wait_for_line("session Beta chosen", chosen + 1)
        # Typed on the chip, his password goes to the field.
        before = read_log()
        type_text(BOB_PASSWORD)
        wait_for_dots(len(BOB_PASSWORD))
        assert marked(SESSION_RING) == 0 and marked(FIELD_RING) > 0
        type_text("\n")
        logged_in("beta", BETA, before, user="bob", ready="input ready")
        state = machine.succeed(f"cat {STATE}")
        assert 'last-user "bob"' in state and 'session "bob" "beta.desktop"' in state, state

    with subtest("the last user preselected; Escape and the Users chip go back to the row"):
        before = read_log()
        restart_greetd()
        assert "last user bob (remembered)" in read_log()[len(before):]
        # Enter, with no key before it, picks bob: the row started on him. His session is
        # his own, Beta.
        prompts, greetings = count("prompt: secret"), count("greeting bob (Bob Builder)")
        machine.send_key("ret")
        wait_for_line("greeting bob (Bob Builder)", greetings + 1)
        wait_for_line("prompt: secret", prompts + 1)
        assert "sessions: Alpha, Beta; Beta chosen (remembered)" in read_log()[len(before):]
        type_text("abc")
        wait_for_dots(3)
        machine.send_key("esc")
        wait_for_line("cancelled; back to the users")
        retry(lambda _: marked(FIELD) == 0 and marked(avatar_box(0)) > 0, 10)
        # Back on bob's avatar: Enter picks him again, with nothing typed left over.
        machine.send_key("ret")
        wait_for_line("greeting bob (Bob Builder)", greetings + 2)
        wait_for_line("prompt: secret", prompts + 2)
        assert dots() == 0, dots()
        click(*USERS)
        wait_for_line("cancelled; back to the users", 2)
        retry(lambda _: marked(FIELD) == 0 and marked(avatar_box(0)) > 0, 10)
        # A click on her avatar: alice, with her own session.
        alice = read_log()
        click(*avatar_center(0))
        wait_for_line("prompt: secret", prompts + 3)
        after = read_log()[len(alice):]
        assert "greeting alice (Alice Liddell)" in after, after
        assert "sessions: Alpha, Beta; Alpha chosen (remembered)" in after, after
        log_in("alpha", ALPHA, ready="input ready")

    with subtest("Other…: a typed user name logs in"):
        # The row starts on alice; "Other…" is two to the right.
        machine.send_key("right")
        machine.send_key("right")
        machine.send_key("ret")
        wait_for_line("asking for a user name")
        prompts = count("prompt: secret")
        type_text("dave\n")
        # Not listed: not logged by the name typed (it might be a password).
        wait_for_line("greeting a user who is not listed", count("greeting a user who is not listed") + 1)
        wait_for_line("prompt: secret", prompts + 1)
        log_in("alpha", ALPHA, password=DAVE_PASSWORD, user="dave", ready="input ready")
        assert 'last-user "dave"' in machine.succeed(f"cat {STATE}")

    with subtest("tuigreet's lastuser seeds the last user, once"):
        # greetd's NixOS module creates /var/cache/tuigreet (greeter's, 0755); tuigreet
        # writes the user name there, 0644, without a newline.
        assert machine.succeed("stat -c '%a %U' /var/cache/tuigreet").strip() == "755 greeter"
        def lastuser(name):
            machine.succeed(
                f"printf {name} > /var/cache/tuigreet/lastuser",
                "chown greeter:greeter /var/cache/tuigreet/lastuser",
                "chmod 0644 /var/cache/tuigreet/lastuser",
            )
        def enter_greets(who):
            n = count(f"greeting {who}")
            machine.send_key("ret")
            wait_for_line(f"greeting {who}", n + 1)
        # No state: tuigreet's bob, who is not first, is preselected.
        machine.succeed(f"rm {STATE}")
        lastuser("bob")
        before = read_log()
        restart_greetd()
        assert "last user bob (from tuigreet)" in read_log()[len(before):]
        enter_greets("bob (Bob Builder)")
        # A state with a last user wins, even one not listed: the row starts on "Other…".
        machine.succeed(
            f"printf 'last-user \"dave\"\\n' > {STATE}",
            f"chown greeter:greeter {STATE}",
            f"chmod 0600 {STATE}",
        )
        before = read_log()
        restart_greetd()
        assert "from tuigreet" not in read_log()[len(before):]
        asked = count("asking for a user name")
        machine.send_key("ret")
        wait_for_line("asking for a user name", asked + 1)
        # tuigreet's user must be listed: dave is not, so the row starts on the first.
        machine.succeed(f"rm {STATE}")
        lastuser("dave")
        before = read_log()
        restart_greetd()
        assert "last user" not in read_log()[len(before):]
        enter_greets("alice (Alice Liddell)")

    with subtest("no one listed: the user name at once; a mistyped one typed again"):
        machine.succeed(
            "rm /etc/login.defs",
            "printf 'UID_MIN 50000\\nUID_MAX 50001\\n' > /etc/login.defs",
        )
        before = read_log()
        restart_greetd()
        after = read_log()[len(before):]
        assert re.search(r"users: ?$", after, re.M), after
        assert "asking for a user name" in after, after
        # No row to go back to.
        assert marked(USERS_CHIP) == 0 and marked(avatar_box(0)) == 0
        prompts, unlisted = count("prompt: secret"), count("greeting a user who is not listed")
        codes = count("prompt: visible")
        type_text("bobb\n")
        wait_for_line("greeting a user who is not listed", unlisted + 1)
        wait_for_line("prompt: visible", codes + 1)
        backs = count("cancelled; back to the user name")
        machine.send_key("esc")
        wait_for_line("cancelled; back to the user name", backs + 1)
        type_text("bob\n")
        wait_for_line("greeting a user who is not listed", unlisted + 2)
        wait_for_line("prompt: secret", prompts + 1)
        log_in("alpha", ALPHA, password=BOB_PASSWORD, user="bob", ready="input ready")

    with subtest("no secret in the journal; not dumpable; --check"):
        journal = machine.succeed("journalctl --no-pager -o cat")
        swapped = PASSWORD.translate(str.maketrans("yz", "zy"))
        secrets = [PASSWORD, swapped, BOB_PASSWORD, DAVE_PASSWORD, ROOT_PASSWORD, WRONG, code]
        # The greeter never logs a name typed for "Other…" (PAM modules may: the
        # google_authenticator one logs a user it cannot find).
        assert NOT_A_NAME not in read_log()
        for secret in secrets:
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
        machine.succeed("mkdir /root/no-sessions && echo 'sessions \"/root/no-sessions\"' > /root/empty.kdl")
        out = machine.fail(f"{command[0]} --check /root/empty.kdl 2>&1")
        assert "no sessions in /root/no-sessions" in out, out

    print(read_log())
  '';
}
