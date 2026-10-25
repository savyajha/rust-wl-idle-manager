{
  description = "Idle daemon for Wayland compositors, built for niri";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (self.packages.${system})
          rust-wl-idle-manager
          rust-wl-greeter-compositor
          rust-wl-greeter
          ;
        greeterTestClient = pkgs.callPackage ./greeter-compositor.nix { testClient = true; };
        # Runs `command` on the source offline, with the package's vendored dependencies.
        cargoCheck =
          name: tools: command:
          pkgs.stdenv.mkDerivation {
            name = "rust-wl-idle-manager-${name}";
            inherit (rust-wl-idle-manager) src cargoDeps;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              pkgs.cargo
              pkgs.pkg-config
            ]
            ++ tools;
            buildInputs =
              rust-wl-idle-manager.buildInputs
              ++ rust-wl-greeter-compositor.buildInputs
              ++ rust-wl-greeter.buildInputs;
            buildPhase = command;
            installPhase = "touch $out";
            dontFixup = true;
          };
      in
      {
        packages = rec {
          rust-wl-idle-manager = pkgs.callPackage ./package.nix { };
          rust-wl-greeter-compositor = pkgs.callPackage ./greeter-compositor.nix { };
          rust-wl-greeter = pkgs.callPackage ./greeter.nix { };
          default = rust-wl-idle-manager;
        };

        checks = {
          # Boots a VM with a headless sway and runs the actual binary against it.
          # Linux + KVM only. Run with: nix build .#checks.<system>.idle-lifecycle
          idle-lifecycle = import ./tests/idle-lifecycle.nix {
            inherit pkgs;
            idleManager = self.packages.${system}.rust-wl-idle-manager;
          };
          # The built-in lock screen, checked by its pixels.
          lock-screen = import ./tests/lock-screen.nix {
            inherit pkgs;
            idleManager = self.packages.${system}.rust-wl-idle-manager;
          };
          # The greeter compositor under greetd, on two virtual outputs.
          greeter-compositor = import ./tests/greeter-compositor.nix {
            inherit pkgs;
            compositor = rust-wl-greeter-compositor;
            testClient = greeterTestClient;
          };
          # The greeter under greetd, through its NixOS module: logins, prompts, sessions.
          greeter = import ./tests/greeter.nix { inherit pkgs; };

          fmt = cargoCheck "fmt" [ pkgs.rustfmt ] "cargo fmt --all --check";
          clippy = cargoCheck "clippy" [
            pkgs.rustc
            pkgs.clippy
          ] "cargo clippy --workspace --all-targets -- -D warnings";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [
            rust-wl-idle-manager
            rust-wl-greeter-compositor
            rust-wl-greeter
          ];
          packages = with pkgs; [
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      }
    )
    // {
      # greetd's greeter: the compositor running the greeter (see greeter-module.nix).
      nixosModules.greeter = ./greeter-module.nix;
    };
}
