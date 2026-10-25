# The greeter as greetd's greeter: rust-wl-greeter-compositor running rust-wl-greeter, with a
# config generated from these options and checked (`--check`) when the system is built.
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.programs.rust-wl-greeter;
  quote = builtins.toJSON;
  configFile =
    pkgs.runCommand "rust-wl-greeter.kdl"
      {
        text = ''
          sessions ${quote cfg.sessions}
          ${lib.optionalString (cfg.keyboardLayout != null) "keyboard-layout ${quote cfg.keyboardLayout}"}
          ${cfg.extraConfig}
        '';
        passAsFile = [ "text" ];
      }
      ''
        cp "$textPath" $out
        ${lib.getExe cfg.package} --check $out
      '';
  scales = lib.concatMapAttrsStringSep " " (
    connector: scale: "--scale ${lib.escapeShellArg "${connector}=${toString scale}"}"
  ) cfg.scales;
in
{
  options.programs.rust-wl-greeter = {
    enable = lib.mkEnableOption "rust-wl-greeter as greetd's greeter";
    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./greeter.nix { };
      description = "The greeter.";
    };
    compositorPackage = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./greeter-compositor.nix { };
      description = "The greeter's compositor.";
    };
    sessions = lib.mkOption {
      type = lib.types.str;
      default = "${config.services.displayManager.sessionData.desktops}/share/wayland-sessions";
      defaultText = lib.literalExpression ''"''${config.services.displayManager.sessionData.desktops}/share/wayland-sessions"'';
      description = "The directory of the sessions' `.desktop` files.";
    };
    keyboardLayout = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "de";
      description = "The xkb layout the greeter types with; by default the compositor's.";
    };
    scales = lib.mkOption {
      type = lib.types.attrsOf lib.types.float;
      default = { };
      example = {
        eDP-1 = 1.5;
      };
      description = "Each connector's scale; 1 for the others.";
    };
    extraConfig = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = "More of the greeter's KDL config, such as its `look`.";
    };
  };

  config = lib.mkIf cfg.enable {
    services.greetd = {
      enable = true;
      # Both log to stderr, which greetd connects to the VT; systemd-cat sends it to the
      # journal instead.
      settings.default_session.command = lib.concatStringsSep " " [
        "${pkgs.systemd}/bin/systemd-cat -t rust-wl-greeter"
        "${lib.getExe cfg.compositorPackage} --log-timing ${scales}"
        "-- ${lib.getExe cfg.package} --config ${configFile}"
      ];
    };
    # The compositor's Ctrl+Alt+Fn goes through logind, which asks polkit.
    security.polkit.enable = true;
    # The greeter also reads tuigreet's last user, once, from /var/cache/tuigreet/lastuser:
    # greetd's module creates that directory (the greeter user's, 0755) and tuigreet, run as
    # that user too, writes the file 0644, so nothing more is needed.
    systemd.tmpfiles.rules = [ "d /var/lib/rust-wl-greeter 0700 greeter greeter -" ];
  };
}
