# --- flake-parts/pkgs/ccdash.nix
{
  lib,
  flake-root,
  rustPlatform,
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
      (flake-root + "/web")
      (flake-root + "/dms-plugin")
    ];
  };

  cargoLock.lockFile = flake-root + "/Cargo.lock";

  postInstall = ''
    mkdir -p $out/share/ccdash
    cp -r dms-plugin $out/share/ccdash/dms-plugin
  '';

  meta = {
    inherit (cargoToml.package) description;
    homepage = "https://github.com/tsandrini/ccdash";
    license = lib.licenses.eupl12;
    platforms = lib.platforms.linux;
    mainProgram = "ccdash";
  };
}
