{
  lib,
  rustPlatform,
  libxkbcommon,
  linux-pam,
}:

rustPlatform.buildRustPackage {
  pname = "rust-wl-idle-manager";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;

  # xkbcommon decodes the keyboard; PAM is called only by the `--auth` helper.
  buildInputs = [
    libxkbcommon
    linux-pam
  ];

  meta = with lib; {
    description = "Idle daemon for Wayland compositors, built for niri";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "rust-wl-idle-manager";
  };
}
