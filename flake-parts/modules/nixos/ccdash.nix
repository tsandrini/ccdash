# --- flake-parts/modules/nixos/ccdash.nix
{ config, lib, ... }:
let
  cfg = config.programs.ccdash;
in
{
  options.programs.ccdash.channel.allow = lib.mkEnableOption ''
    the ccdash channel in Claude Code's managed settings
    ({file}`/etc/claude-code/managed-settings.d/ccdash.json`), so sessions
    started by the home-manager module's `claude` wrapper load it without
    the development-channel warning. This replaces Anthropic's default
    channel allowlist on this machine; list any other channel plugins you
    use in `allowedChannelPlugins` yourself
  '';

  config = lib.mkIf cfg.channel.allow {
    environment.etc."claude-code/managed-settings.d/ccdash.json".text = builtins.toJSON {
      allowedChannelPlugins = [
        {
          marketplace = "inline";
          plugin = "ccdash";
        }
      ];
    };
  };
}
