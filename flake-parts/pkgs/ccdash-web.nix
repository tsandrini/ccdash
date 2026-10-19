# The panel: the SvelteKit app built into static files the hub serves
# (`http.web_dir`). `npmDepsHash` follows `web/package-lock.json`: after a
# lock change, `prefetch-npm-deps web/package-lock.json` prints the new one.
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
    fileset = lib.fileset.difference web (
      lib.fileset.unions [
        (lib.fileset.maybeMissing (web + "/node_modules"))
        (lib.fileset.maybeMissing (web + "/build"))
        (lib.fileset.maybeMissing (web + "/.svelte-kit"))
      ]
    );
  };
  common = {
    inherit (packageJson) version;
    inherit src nodejs;
    sourceRoot = "${src.name}/web";
    npmDepsHash = "sha256-EMQNW8dmz2BDZZp5cc+Gk5ow6ljAnmCIE39Kj9xizX8=";
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
