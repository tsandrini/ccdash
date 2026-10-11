# --- flake-parts/modules/home-manager/default.nix
{
  lib,
  config,
  inputs,
  self,
  ...
}:
let
  inherit (inputs.flake-parts.lib) importApply;
in
{
  options.flake.homeModules = lib.mkOption {
    type = with lib.types; lazyAttrsOf unspecified;
    default = { };
  };

  config.flake.homeModules = {
    ccdash = importApply ./ccdash.nix { localFlake = self; };
    default = config.flake.homeModules.ccdash;
  };
}
