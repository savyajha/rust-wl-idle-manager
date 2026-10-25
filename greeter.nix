{
  lib,
  rustPlatform,
  pkg-config,
  cairo,
  libxkbcommon,
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

  meta = {
    description = "Graphical greetd greeter that looks like rust-wl-idle-manager's lock screen";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "rust-wl-greeter";
  };
}
