{ config, lib, ... }:
let
  inherit (config) flake-root;
in
{
  perSystem =
    {
      pkgs,
      config,
      system,
      ...
    }:
    {
      packages = {
        default = config.packages.ccdash;

        ccdash = pkgs.callPackage ./ccdash.nix {
          inherit flake-root;
          inherit (config.packages) ccdash-web;
        };
        ccdash-web = pkgs.callPackage ./ccdash-web.nix { inherit flake-root; };
      }
      // lib.optionalAttrs (system == "x86_64-linux") {
        # The hub for the Pi, cross-compiled here at native speed: building it
        # on the Pi or under QEMU takes a quarter of an hour, this takes a
        # minute. The panel is static files, so the x86_64 build serves it.
        ccdash-aarch64-linux = pkgs.pkgsCross.aarch64-multiplatform.callPackage ./ccdash.nix {
          inherit flake-root;
          inherit (config.packages) ccdash-web;
        };
      };

      # NOTE: svelte-check, eslint and prettier over web/ without a local node_modules
      checks.ccdash-web-lint = config.packages.ccdash-web.lint;
    };
}
