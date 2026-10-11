# --- flake-parts/pre-commit-hooks.nix
{ inputs, config, ... }:
let
  inherit (config) flake-root;
in
{
  imports = [ inputs.pre-commit-hooks.flakeModule ];

  perSystem =
    { pkgs, ... }:
    {
      pre-commit.settings = {
        excludes = [
          "flake.lock" # NOTE: prettier thinks this is json >.< prettier baka!!!
          "Cargo.lock"
        ];

        settings.rust.check.cargoDeps = pkgs.rustPlatform.importCargoLock {
          lockFile = flake-root + "/Cargo.lock";
        };

        hooks = {
          # --- Nix ---
          deadnix.enable = true; # Find and remove unused code in .nix source files
          nil.enable = true; # Nix Language server, an incremental analysis assistant for writing in Nix.
          nixfmt.enable = true; # An opinionated formatter for Nix
          statix.enable = true; # Lints and suggestions for the nix programming language

          # --- Rust ---
          rustfmt.enable = true; # The Rust code formatter
          clippy.enable = true; # Lints for Rust
          cargo-check.enable = true; # Check the package and its dependencies for errors

          # --- Shell ---
          shellcheck.enable = true; # Shell script analysis tool
          shfmt.enable = true; # Shell parser and formatter

          # --- Misc ---
          markdownlint.enable = true; # Markdown lint tool
          editorconfig-checker.enable = true; # .editorconfig file checker
          typos.enable = true; # Source code spell checker
          prettier = {
            enable = true; # Prettier is an opinionated code formatter
            excludes = [ "dms-plugin/.*\\.js$" ]; # NOTE: QML's `.pragma library` is not JS
          };
          check-toml.enable = true;

          # --- fs utils ---
          check-symlinks.enable = true; # Check for broken symlinks
          check-added-large-files.enable = true;
          check-executables-have-shebangs.enable = true;
          check-shebang-scripts-are-executable.enable = true;
          end-of-file-fixer.enable = true;
          mixed-line-endings.enable = true;
          trim-trailing-whitespace.enable = true;

          # --- VCS ---
          commitizen.enable = true; # Commitizen is release management tool designed for teams.
          ripsecrets.enable = true; # A tool to prevent committing secret keys into your source code
        };
      };
    };
}
