{
  description = "Idle daemon for Wayland compositors (tested with sway, intended for niri)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        # Runs `command` on the source offline, with the package's vendored dependencies.
        cargoCheck =
          name: tools: command:
          pkgs.stdenv.mkDerivation {
            name = "rust-wl-idle-manager-${name}";
            inherit (self.packages.${system}.rust-wl-idle-manager) src cargoDeps;
            nativeBuildInputs = [
              pkgs.rustPlatform.cargoSetupHook
              pkgs.cargo
            ]
            ++ tools;
            buildPhase = command;
            installPhase = "touch $out";
            dontFixup = true;
          };
      in
      {
        packages = rec {
          rust-wl-idle-manager = pkgs.callPackage ./package.nix { };
          default = rust-wl-idle-manager;
        };

        checks = {
          # Boots a VM with a headless sway and runs the actual binary against it.
          # Linux + KVM only. Run with: nix build .#checks.<system>.idle-lifecycle
          idle-lifecycle = import ./tests/idle-lifecycle.nix {
            inherit pkgs;
            idleManager = self.packages.${system}.rust-wl-idle-manager;
          };

          fmt = cargoCheck "fmt" [ pkgs.rustfmt ] "cargo fmt --check";
          clippy = cargoCheck "clippy" [
            pkgs.rustc
            pkgs.clippy
          ] "cargo clippy --all-targets -- -D warnings";
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.rust-wl-idle-manager ];
          packages = with pkgs; [
            rust-analyzer
            clippy
            rustfmt
          ];
        };
      }
    );
}
