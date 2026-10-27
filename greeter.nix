{
  lib,
  rustPlatform,
  pkg-config,
  adwaita-fonts,
  cairo,
  libxkbcommon,
  makeFontsConf,
  pango,
}:

rustPlatform.buildRustPackage {
  pname = "rust-wl-greeter";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;
  cargoBuildFlags = [
    "--package"
    "rust-wl-greeter"
  ];
  cargoTestFlags = [
    "--package"
    "rust-wl-greeter"
  ];

  nativeBuildInputs = [ pkg-config ];
  # Pango and cairo draw the login screen; xkbcommon decodes the keyboard. No PAM: greetd
  # runs it.
  buildInputs = [
    cairo
    libxkbcommon
    pango
  ];

  # A font for the unit tests that draw text.
  FONTCONFIG_FILE = makeFontsConf { fontDirectories = [ adwaita-fonts ]; };
  preCheck = "export XDG_CACHE_HOME=$TMPDIR";

  meta = {
    description = "Graphical greetd greeter that looks like rust-wl-idle-manager's lock screen";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "rust-wl-greeter";
  };
}
