# --- flake-parts/modules/nixos/default.nix
{ config, ... }:
{
  config.flake.nixosModules = {
    ccdash = ./ccdash.nix;
    default = config.flake.nixosModules.ccdash;
  };
}
