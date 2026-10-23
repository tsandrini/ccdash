# Development

1. [The daemons](#1-the-daemons)
2. [The panel](#2-the-panel)
3. [Checks](#3-checks)

## 1. The daemons

```sh
direnv allow                                     # or: nix develop
cargo run -- hub --config hub.toml               # http.listen, collect.tokens, store.path
cargo run -- collector --config collector.toml   # hub.url = "ws://127.0.0.1:7837/collect"
```

- `CCDASH_RUNTIME_DIR` (sockets) and `XDG_STATE_HOME` (outbox, sync index)
  give a second collector, or the hooks of a test session, a place next to
  the real one.
- `CCDASH_LOG=info,ccdash=debug` for more log.
- A dev hub with a dummy bootstrap token (`devtoken-not-secret`) on
  `127.0.0.1` is the usual end-to-end setup; feed it hook payloads with
  `printf '{…}' | CCDASH_RUNTIME_DIR=… ccdash emit`.

## 2. The panel

`web/` is SvelteKit with Svelte 5, Tailwind, shadcn-svelte and LayerChart;
Node and npm are in the devshell.

```sh
cd web && npm ci
npm run dev                   # Vite on :5173; /api and /act go to CCDASH_HUB (127.0.0.1:7837)
npm run check && npm run lint
npm run build                 # → web/build, what a `cargo run` hub serves
```

- The hub serves `http.web_dir` (the Nix build bakes in the `ccdash-web`
  package; a `cargo run` hub falls back to `web/build`; `CCDASH_WEB_DIR`
  overrides both), with `index.html` for every route.
- The API's TypeScript types (`web/src/lib/api/types.ts`) are generated
  from the Rust models: `cargo test api` fails while the file is stale,
  `CCDASH_UPDATE_TYPES=1 cargo test api` rewrites it.
- After a change to `web/package-lock.json`, put the hash that
  `prefetch-npm-deps web/package-lock.json` prints into
  `flake-parts/pkgs/ccdash-web.nix`.
- The service worker only registers in the built app, so push and the
  offline shell are checked against `web/build` served by a hub.

## 3. Checks

```sh
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd web && npm run check && npm run lint
nix flake check     # also runs the pre-commit hooks (editorconfig, markdownlint, typos, …)
nix build .#ccdash .#ccdash-web .#ccdash-aarch64-linux
```

`nix flake check` sees only files git knows; check a fresh clone or a copy
before pushing new files.
