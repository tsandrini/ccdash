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

        ccdash = pkgs.callPackage ./ccdash.nix {
          inherit flake-root;
          inherit (config.packages) ccdash-web;
        };
        ccdash-web = pkgs.callPackage ./ccdash-web.nix { inherit flake-root; };
      };

      # NOTE: svelte-check, eslint and prettier over web/ without a local node_modules
      checks.ccdash-web-lint = config.packages.ccdash-web.lint;
    };
}
