# Configuration

Both daemons read TOML (`--config`, else `$XDG_CONFIG_HOME/ccdash/config.toml`)
and reject unknown keys. The Nix modules write the files: their options
cover the common keys, `settings` takes the rest. In paths, `$VAR` and
`${VAR}` expand.

1. [NixOS module (hub)](#1-nixos-module-hub)
2. [Home Manager module (collector)](#2-home-manager-module-collector)
3. [Hub keys](#3-hub-keys)
4. [Collector keys](#4-collector-keys)

## 1. NixOS module (hub)

`services.ccdash-hub.*`

| Option                                                 | Default                      | Meaning                                                        |
| ------------------------------------------------------ | ---------------------------- | -------------------------------------------------------------- |
| `enable`                                               | `false`                      | run the hub                                                    |
| `tokenFile`                                            | —                            | the bootstrap token of the panel and the API                   |
| `collectors`                                           | `{ }`                        | host name → file with that machine's collector token           |
| `port`                                                 | `7837`                       | loopback port; nginx or another TLS proxy goes in front        |
| `nginx.enable`, `nginx.domain`                         | `false`, —                   | a vhost proxying to the hub (WebSockets, SSE, `/sync/` bodies) |
| `backup.enable`                                        | `false`                      | a nightly checked copy of the database                         |
| `backup.directory`, `backup.keep`, `backup.onCalendar` | `…/backups`, `7`, `03:30`    | where, how many, when                                          |
| `sync.directory`                                       | `/var/lib/ccdash-hub/claude` | the synced `~/.claude` files                                   |
| `workspace.enable`                                     | `false`                      | the workspace sync: bare repositories, `/git/`, git, the gc    |
| `workspace.gcOnCalendar`                               | `03:30`                      | when `git gc` runs over the repositories                       |
| `settings`                                             | `{ }`                        | any key of the hub's TOML                                      |

`programs.ccdash.channel.allow` (the default NixOS module) allowlists the
channel plugin in Claude Code's managed settings.

## 2. Home Manager module (collector)

`services.ccdash.*`

| Option                                     | Default                          | Meaning                                                          |
| ------------------------------------------ | -------------------------------- | ---------------------------------------------------------------- |
| `enable`                                   | `false`                          | run the collector and wire the hooks into `programs.claude-code` |
| `hub.url`, `hub.tokenFile`                 | —                                | the hub's `/collect` endpoint and this machine's token           |
| `sync.enable`                              | `false`                          | keep `~/.claude` in step through the hub                         |
| `workspace.enable`, `workspace.root`       | `false`, `~/ProjectBundle`       | keep the workspace root in step through the hub                  |
| `workspace.include`, `.exclude`            | `[ ]`, `[ ]`                     | globs on unit ids (the starting configuration)                   |
| `workspace.alwaysExclude`                  | `node_modules`, `target`, …      | directory names that never travel                                |
| `workspace.syncIgnored`                    | `.env`, `.env.*`, `.envrc.local` | gitignored paths that travel anyway                              |
| `workspace.maxFileMb`, `.autoIncludeMb`    | `64`, `500`                      | the untracked file cap, the new-directory threshold              |
| `launch.direnv`, `launch.env`              | `programs.direnv.enable`, `[ ]`  | launched sessions get the matching direnv variables              |
| `dms.enable`                               | `false`                          | the DankMaterialShell plugin                                     |
| `claudeCode.hooks.enable`                  | `true`                           | the hooks                                                        |
| `claudeCode.asks.enable`                   | `true`                           | answer prompts from the panel (else only report them)            |
| `claudeCode.channel.enable`                | `false`                          | start sessions with the channel (replies from the panel)         |
| `claudeCode.channel.devFlag`               | `false`                          | load the channel without the allowlist (asks every launch)       |
| `claudeCode.telemetry.enable`, `.port`     | `false`, `7839`                  | OpenTelemetry into the collector                                 |
| `claudeCode.statusLine.enable`, `.command` | `false`, —                       | read the statusline, then run your own                           |
| `systemdTarget`                            | `graphical-session.target`       | what the service starts with                                     |
| `settings`                                 | `{ }`                            | any key of the collector's TOML                                  |

## 3. Hub keys

| Key                                                         | Default                        | Meaning                                                      |
| ----------------------------------------------------------- | ------------------------------ | ------------------------------------------------------------ |
| `host`                                                      | the kernel's host name         | the name the panel shows for the hub                         |
| `http.listen`                                               | `["127.0.0.1:7837"]`           | addresses of the API and the panel                           |
| `http.token_file`                                           | generated in the state dir     | the bootstrap token                                          |
| `http.public_url`                                           | LAN address and first port     | where phones open the panel (links, pairing)                 |
| `http.web_dir`                                              | the built panel                | the panel's files                                            |
| `collect.tokens`                                            | `{}`                           | host name → collector token file                             |
| `store.path`                                                | `<state dir>/ccdash.db`        | the database                                                 |
| `store.retention_days`                                      | `30`                           | history kept                                                 |
| `store.tool_output_bytes`                                   | `8192`                         | Bash output kept per call (0 keeps none)                     |
| `workspaces.dirs`                                           | `[]`                           | top-level directories the panel scopes by                    |
| `notify.delay_secs`, `.focused_delay_secs`                  | `15`, `120`                    | how long a prompt waits before it notifies                   |
| `notify.permissions`, `.questions`, `.plans`, `.turns`      | `true`                         | what notifies                                                |
| `notify.workspaces`                                         | `[]` (all)                     | only these workspaces notify                                 |
| `notify.web_url`                                            | `http.public_url`              | where notification taps lead                                 |
| `push.enabled`                                              | `true`                         | Web Push                                                     |
| `push.subject`, `.ttl_secs`, `.key_file`                    | public URL, `3600`, state dir  | VAPID contact, message lifetime, key pair                    |
| `knowledge.reflect`                                         | `true`                         | the daily staleness pass                                     |
| `knowledge.stale_unverified_days`, `.stale_unrecalled_days` | `90`, `60`                     | when memories turn stale                                     |
| `sync.dir`                                                  | `claude/` next to the database | the synced files                                             |
| `workspace.enable`                                          | `false`                        | the workspace sync (repositories under `sync.dir/workspace`) |

## 4. Collector keys

| Key                                         | Default                                         | Meaning                                                                                  |
| ------------------------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `host`                                      | the kernel's host name                          | must match a key of the hub's `collect.tokens`                                           |
| `hub.url`, `hub.token_file`                 | —                                               | the hub's `/collect` endpoint, this machine's token                                      |
| `agents.command`, `.interval_secs`          | `["claude","agents","--json"]`, `5`             | the liveness poll                                                                        |
| `usage.command`, `.interval_secs`           | `["claude"]`, `600`                             | the `/usage` poll (`[]` is off)                                                          |
| `launch.command`                            | `["claude"]`                                    | starts sessions from the panel (`[]` is off)                                             |
| `launch.stop`, `.remove`                    | `claude stop {id}`, `claude rm {id}`            | stop a background session; remove one nothing runs                                       |
| `launch.direnv`, `.env`                     | `[]`, `[]`                                      | `direnv export json` in the launch dir; the globs of the variables launched sessions get |
| `attach.command`                            | `ghostty -e claude attach {id}`                 | opens a background session on the desktop                                                |
| `niri.enable`, `.terminals`                 | `true`, `["com.mitchellh.ghostty"]`             | window focus and which windows are terminals                                             |
| `procs.enable`, `.interval_secs`            | `true`, `5`                                     | CPU and memory sampling                                                                  |
| `telemetry.listen`                          | `[]`                                            | OTLP receiver (loopback only)                                                            |
| `queue.max_bytes`                           | 32 MiB                                          | the outbox while the hub is unreachable                                                  |
| `inject.session_start`, `.prompt`, `.edits` | `true`                                          | the knowledge injections                                                                 |
| `sync.enable`                               | `false`                                         | the state sync                                                                           |
| `sync.root`                                 | `$CLAUDE_CONFIG_DIR`, else `~/.claude`          | what is kept in step                                                                     |
| `sync.include`, `.exclude`                  | see [state sync](state-sync.md#7-on-the-hub)    | what travels                                                                             |
| `sync.interval_secs`                        | `3`                                             | full scans, only without inotify                                                         |
| `workspace.enable`, `.root`                 | `false`, —                                      | the workspace sync and its root (`~/` and `$VAR` expand)                                 |
| `workspace.depth`                           | `2`                                             | units at `<root>/<bundle>/<repo>`                                                        |
| `workspace.include`, `.exclude`             | `[]`, `[]`                                      | globs on unit ids; the starting configuration a hub without one takes                    |
| `workspace.always_exclude`                  | see [workspace](workspace.md#4-what-stays-home) | directory names that never travel                                                        |
| `workspace.sync_ignored`                    | `[".env", ".env.*", ".envrc.local"]`            | gitignored paths that travel anyway (`.devenv/state/*` quiescent)                        |
| `workspace.max_file_mb`, `.auto_include_mb` | `64`, `500`                                     | the untracked file cap, the new-directory threshold                                      |
