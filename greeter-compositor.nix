{
  lib,
  rustPlatform,
  pkg-config,
  libinput,
  libxkbcommon,
  pixman,
  seatd,
  systemd,
  # Build the VM test's client (examples/test-client.rs) instead of the compositor.
  testClient ? false,
}:

rustPlatform.buildRustPackage {
  pname = if testClient then "rust-wl-greeter-test-client" else "rust-wl-greeter-compositor";
  version = "0.1.0";

  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;
  cargoBuildFlags = [
    "--package"
    "rust-wl-greeter-compositor"
  ]
  ++ lib.optionals testClient [
    "--example"
    "test-client"
  ];
  cargoTestFlags = [
    "--package"
    "rust-wl-greeter-compositor"
  ];
  doCheck = !testClient;

  nativeBuildInputs = [ pkg-config ];
  # libseat (seatd) takes the seat, libinput and libudev (systemd) read input and hotplug,
  # pixman draws, xkbcommon decodes keys. No Mesa, GBM or EGL.
  buildInputs = [
    libinput
    libxkbcommon
    pixman
    seatd
    systemd
  ];

  postInstall = lib.optionalString testClient ''
    install -Dm755 target/*/release/examples/test-client $out/bin/rust-wl-greeter-test-client
  '';

  meta = {
    description =
      if testClient then
        "Test client for rust-wl-greeter-compositor's VM test"
      else
        "Minimal Wayland compositor for a greetd greeter, drawn without a GPU driver stack";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = if testClient then "rust-wl-greeter-test-client" else "rust-wl-greeter-compositor";
  };
}
