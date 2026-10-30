# --- flake-parts/pkgs/ccdash.nix
# The hub, collector and hooks. The hub serves the panel from
# `ccdash-web` unless `http.web_dir` or `CCDASH_WEB_DIR` says otherwise.
{
  lib,
  flake-root,
  rustPlatform,
  ccdash-web,
  git,
}:
let
  cargoToml = lib.importTOML (flake-root + "/Cargo.toml");
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = flake-root;
    fileset = lib.fileset.unions [
      (flake-root + "/Cargo.toml")
      (flake-root + "/Cargo.lock")
      (flake-root + "/src")
      # NOTE: `cargo test api` checks the exported types against this file
      (flake-root + "/web/src/lib/api/types.ts")
      (flake-root + "/dms-plugin")
      (flake-root + "/claude-plugin")
    ];
  };

  cargoLock.lockFile = flake-root + "/Cargo.lock";

  # NOTE: the workspace sync's tests drive real repositories
  nativeCheckInputs = [ git ];

  # NOTE: read by `option_env!` in src/config.rs
  env.CCDASH_WEB_DIR_DEFAULT = "${ccdash-web}";

  postInstall = ''
    mkdir -p $out/share/ccdash
    cp -r dms-plugin $out/share/ccdash/dms-plugin
    cp -r claude-plugin $out/share/ccdash/claude-plugin
    substituteInPlace $out/share/ccdash/claude-plugin/.mcp.json \
      --replace-fail '"command": "ccdash"' "\"command\": \"$out/bin/ccdash\""
  '';

  meta = {
    inherit (cargoToml.package) description;
    homepage = "https://github.com/tsandrini/ccdash";
    license = lib.licenses.eupl12;
    platforms = lib.platforms.linux;
    mainProgram = "ccdash";
  };
}
