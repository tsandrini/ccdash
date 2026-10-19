# --- flake-parts/shells/dev.nix
{
  lib,
  mkShell,
  nil,
  statix,
  deadnix,
  nix-output-monitor,
  nixfmt,
  commitizen,
  cz-cli,
  gh,
  markdownlint-cli,
  typos,
  cargo,
  rustc,
  clippy,
  rustfmt,
  rust-analyzer,
  nodejs,
  prefetch-npm-deps,
  pre-commit ? null,
}:
mkShell {
  packages = (lib.optional (pre-commit != null) pre-commit.settings.enabledPackages) ++ [
    # -- NIX UTILS --
    nil # Yet another language server for Nix
    statix # Lints and suggestions for the nix programming language
    deadnix # Find and remove unused code in .nix source files
    nix-output-monitor # Processes output of Nix commands to show helpful and pretty information
    nixfmt # An opinionated formatter for Nix

    # -- GIT RELATED UTILS --
    commitizen # Tool to create committing rules for projects, auto bump versions, and generate changelogs
    cz-cli # The commitizen command line utility
    gh # GitHub CLI tool

    # -- BASE LANG UTILS --
    markdownlint-cli # Command line interface for MarkdownLint
    typos # Source code spell checker

    # -- (YOUR) EXTRA PKGS --
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    nodejs # the panel: `npm ci` and `npm run dev` in web/
    prefetch-npm-deps # `npmDepsHash` after a lock file change
  ];

  shellHook = ''
    ${lib.optionalString (pre-commit != null) pre-commit.settings.shellHook}

    # Welcome splash text
    echo ""; echo -e "\e[1;37;42mWelcome to the ccdash devshell!\e[0m"; echo ""
  '';
}
