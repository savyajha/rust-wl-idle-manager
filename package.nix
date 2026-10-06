{
  lib,
  rustPlatform,
  pkg-config,
  adwaita-fonts,
  cairo,
  libxkbcommon,
  linux-pam,
  makeFontsConf,
  pango,
}:

rustPlatform.buildRustPackage {
  pname = "rust-wl-idle-manager";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  # xkbcommon decodes the keyboard; PAM is called only by the `--auth` helper; Pango and
  # cairo draw the lock screen.
  buildInputs = [
    cairo
    libxkbcommon
    linux-pam
    pango
  ];

  # A font for the unit tests that draw text.
  FONTCONFIG_FILE = makeFontsConf { fontDirectories = [ adwaita-fonts ]; };
  preCheck = "export XDG_CACHE_HOME=$TMPDIR";

  meta = with lib; {
    description = "Idle daemon for Wayland compositors, built for niri";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "rust-wl-idle-manager";
  };
}
