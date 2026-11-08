# Configuration

Both daemons read TOML (`--config`, else `$XDG_CONFIG_HOME/ccdash/config.toml`;
on Windows `%APPDATA%\ccdash\config.toml`) and reject unknown keys. The Nix
modules write the files, and on Windows [the script](windows.md) the
collector's: their options cover the common keys, `settings` takes the rest.
In paths, `$VAR` and `${VAR}` expand.

1. [NixOS module (hub)](#1-nixos-module-hub)
2. [Home Manager module (collector)](#2-home-manager-module-collector)
3. [Hub keys](#3-hub-keys)
4. [Collector keys](#4-collector-keys)

## 1. NixOS module (hub)

`services.ccdash-hub.*` runs the hub; `programs.ccdash.channel.allow` (the
default NixOS module) allowlists the channel plugin in Claude Code's managed
settings. Every option with its type, default and example, generated from
the module: [hub module
options](https://tsandrini.github.io/ccdash/options/hub.html).

## 2. Home Manager module (collector)

`services.ccdash.*` runs the collector and wires Claude Code to it (hooks,
the plugin, the channel wrapper, the statusline). Every option, generated
from the module: [Home Manager module
options](https://tsandrini.github.io/ccdash/options/home-manager.html).

## 3. Hub keys

| Key                                                         | Default                         | Meaning                                                                                  |
| ----------------------------------------------------------- | ------------------------------- | ---------------------------------------------------------------------------------------- |
| `host`                                                      | the kernel's host name          | the name the panel shows for the hub                                                     |
| `http.listen`                                               | `["127.0.0.1:7837"]`            | addresses of the API and the panel                                                       |
| `http.token_file`                                           | generated in the state dir      | the bootstrap token                                                                      |
| `http.public_url`                                           | LAN address and first port      | where phones open the panel (links, pairing)                                             |
| `http.web_dir`                                              | the built panel                 | the panel's files                                                                        |
| `collect.tokens`                                            | `{}`                            | host name → collector token file                                                         |
| `store.path`                                                | `<state dir>/ccdash.db`         | the database                                                                             |
| `store.retention_days`                                      | `30`                            | history kept                                                                             |
| `store.tool_output_bytes`                                   | `8192`                          | Bash output kept per call (0 keeps none)                                                 |
| `workspaces.dirs`                                           | `[]`                            | top-level directories the panel scopes by                                                |
| `notify.delay_secs`, `.focused_delay_secs`                  | `15`, `120`                     | how long a prompt waits before it notifies                                               |
| `notify.permissions`, `.questions`, `.plans`, `.turns`      | `true`                          | what notifies                                                                            |
| `notify.workspaces`                                         | `[]` (all)                      | only these workspaces notify                                                             |
| `notify.web_url`                                            | `http.public_url`               | where notification taps lead                                                             |
| `push.enabled`                                              | `true`                          | Web Push                                                                                 |
| `push.subject`, `.ttl_secs`, `.key_file`                    | public URL, `3600`, state dir   | VAPID contact, message lifetime, key pair                                                |
| `knowledge.reflect`                                         | `true`                          | the daily staleness pass                                                                 |
| `knowledge.stale_unverified_days`, `.stale_unrecalled_days` | `90`, `60`                      | when memories turn stale                                                                 |
| `sync.dir`                                                  | `claude/` next to the database  | the synced files                                                                         |
| `workspace.enable`                                          | `false`                         | the workspace sync (repositories under `sync.dir/workspace`)                             |
| `reports.dir`                                               | `reports/` next to the database | the published reports' HTML files                                                        |
| `reports.max_mb`                                            | `32`                            | the largest report taken                                                                 |
| `wake.via.<host>`                                           | —                               | where magic packets go for a machine on another network ([machines](machines.md#4-wake)) |

## 4. Collector keys

| Key                                         | Default                                         | Meaning                                                                                  |
| ------------------------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------------------------- |
| `host`                                      | the kernel's host name                          | must match a key of the hub's `collect.tokens`                                           |
| `hub.url`, `hub.token_file`                 | —                                               | the hub's `/collect` endpoint, this machine's token                                      |
| `agents.command`, `.interval_secs`          | `["claude","agents","--json","--all"]`, `5`     | the liveness poll                                                                        |
| `usage.command`, `.interval_secs`           | `["claude"]`, `600`                             | the `/usage` poll (`[]` is off)                                                          |
| `skills.plugin_dirs`                        | `[]` (the ccdash plugin when the channel is on) | plugin directories sessions start with; their skills join the composer's `/` menu        |
| `launch.command`                            | `["claude"]`                                    | starts sessions from the panel (`[]` is off)                                             |
| `launch.channel`                            | `false`                                         | launched sessions load the ccdash channel (`--channels`)                                 |
| `launch.stop`, `.remove`                    | `claude stop {id}`, `claude rm {id}`            | stop a background session; remove one nothing runs                                       |
| `launch.direnv`, `.env`                     | `[]`, `[]`                                      | `direnv export json` in the launch dir; the globs of the variables launched sessions get |
| `attach.command`                            | `[]` (off)                                      | opens a background session in a terminal (`{id}`: its short id)                          |
| `edit.command`                              | `[]` (off)                                      | opens a draft from the panel in an editor; `{file}` is its path                          |
| `niri.enable`, `.terminals`                 | `true`, common terminals' app ids               | window focus and which windows are terminals                                             |
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
| `power.suspend`                             | `false`                                         | carry out the panel's Suspend                                                            |
| `power.suspend_command`                     | `["systemctl", "suspend"]`                      | suspends; returns once the sleep is on its way                                           |
| `power.lock`                                | `false`                                         | carry out the panel's Lock                                                               |
| `power.lock_command`                        | `["loginctl", "lock-session", "{session}"]`     | locks the user's display session (`{session}`: its id)                                   |

On Windows `host` defaults to the DNS host name, lowercased; `niri.enable`
to `false`; `power.lock_command` to `["rundll32.exe",
"user32.dll,LockWorkStation"]`; and `power.suspend_command` to a hidden
PowerShell calling `SetSuspendState` (sleep, not hibernation).
