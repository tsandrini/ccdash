# The panel: the SvelteKit app built into static files the hub serves
# (`http.web_dir`), with the previous panel under `v1/` until it is
# retired. `npmDepsHash` follows `web/package-lock.json`: after a lock
# change, `prefetch-npm-deps web/package-lock.json` prints the new one.
# `passthru.lint` runs `npm run check` and `npm run lint` in the sandbox
# (a flake check), since the pre-commit hooks need a local node_modules.
{
  lib,
  flake-root,
  buildNpmPackage,
  nodejs,
}:
let
  packageJson = lib.importJSON (flake-root + "/web/package.json");
  web = flake-root + "/web";
  src = lib.fileset.toSource {
    root = flake-root;
    fileset = lib.fileset.unions [
      (lib.fileset.difference web (
        lib.fileset.unions [
          (lib.fileset.maybeMissing (web + "/node_modules"))
          (lib.fileset.maybeMissing (web + "/build"))
          (lib.fileset.maybeMissing (web + "/.svelte-kit"))
        ]
      ))
      (flake-root + "/web-v1")
    ];
  };
  common = {
    inherit (packageJson) version;
    inherit src nodejs;
    sourceRoot = "${src.name}/web";
    npmDepsHash = "sha256-QBT4LFmKWmE8A0GWYTjsRbcHPAtwcfHkHG+I0ajBeoM=";
    meta = {
      homepage = "https://github.com/tsandrini/ccdash";
      license = lib.licenses.eupl12;
      platforms = lib.platforms.linux;
    };
  };
in
buildNpmPackage (
  common
  // {
    pname = "ccdash-web";

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      cp -r build/. $out/
      cp -r ../web-v1 $out/v1
      runHook postInstall
    '';

    passthru.lint = buildNpmPackage (
      common
      // {
        pname = "ccdash-web-lint";
        dontNpmBuild = true;
        doCheck = true;
        checkPhase = ''
          runHook preCheck
          npm run check
          npm run lint
          runHook postCheck
        '';
        installPhase = "touch $out";
      }
    );

    meta = common.meta // {
      description = "The ccdash panel, built";
    };
  }
)
