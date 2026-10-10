# --- flake-parts/pkgs/default.nix
{ config, ... }:
let
  inherit (config) flake-root;
in
{
  perSystem =
    { pkgs, config, ... }:
    {
      packages = {
        default = config.packages.ccdash;

        ccdash = pkgs.callPackage ./ccdash.nix { inherit flake-root; };
      };
    };
}
