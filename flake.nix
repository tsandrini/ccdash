# --- flake.nix
{
  description = "Live dashboard for running Claude Code sessions";

  inputs = {
    # --- BASE DEPENDENCIES ---
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts-builder.url = "github:tsandrini/flake-parts-builder";

    # --- YOUR DEPENDENCIES ---
    pre-commit-hooks.url = "github:cachix/pre-commit-hooks.nix";
    systems.url = "github:nix-systems/default";
  };

  nixConfig = {
    extra-trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
      "nix-community.cachix.org-1:mB9FSh9qf2dCimDSUo8Zy7bkq5CX+/rkCWyvRCYg3Fs="
      "pre-commit-hooks.cachix.org-1:Pkk3Panw5AW24TOv6kz3PvLhlH8puAsJTBbOPmBo7Rc="
      "tsandrini.cachix.org-1:t0AzIUglIqwiY+vz/WRWXrOkDZN8TwY3gk+n+UDt4gw="
    ];
    extra-substituters = [
      "https://cache.nixos.org"
      "https://nix-community.cachix.org/"
      "https://pre-commit-hooks.cachix.org/"
      "https://tsandrini.cachix.org"
    ];
  };

  outputs =
    inputs@{ flake-parts, flake-parts-builder, ... }:
    let
      inherit (flake-parts-builder.lib) loadParts;
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = loadParts ./flake-parts;
    };
}
