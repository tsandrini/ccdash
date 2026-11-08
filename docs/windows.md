# A Windows workstation

A Windows machine can be a workstation: the collector, the hooks, the CLI,
the plugin and the channel run there and report to a hub on Linux. The hub
runs on Linux only, as do `ccdash demo` (a hub), the DankMaterialShell
plugin and niri.

1. [Install](#1-install)
2. [Optional features](#2-optional-features)
3. [Start at logon](#3-start-at-logon)
4. [The hub's side](#4-the-hubs-side)
5. [Updating](#5-updating)
6. [Where things are](#6-where-things-are)
7. [Differences](#7-differences)

## 1. Install

You need Rust with a C compiler (ring and the bundled SQLite need one: the
MSVC toolchain, or the GNU one with MinGW), PowerShell 7, Claude Code
2.1.157 or later, and a collector token the hub lists under this machine's
host name (section 4). From the repository, as yourself (no elevation):

```powershell
pwsh -File windows\install.ps1 -Build -Hub wss://ccdash.example.org/collect -Token .\token
```

It builds ccdash and installs:

- `ccdash.exe` in `~\.local\bin`, which joins your `PATH`;
- the ccdash plugin in `~\.local\share\ccdash\claude-plugin`, linked into
  `~\.claude\skills\ccdash`: every session gets the knowledge tools and the
  `ccdash:wrap-up`, `ccdash:report` and `ccdash:project` skills;
- the collector's configuration, `%APPDATA%\ccdash\config.toml`, and its
  token, readable by you alone;
- the hooks in `~\.claude\settings.json`, next to any of your own (the file
  as it was stays as `settings.json.before-ccdash`);
- the collector, started now and at every logon (section 3).

The host name is the DNS host name, lowercased; `-HostName` sets another.

A machine without a toolchain installs from a bundle: `-Pack D:\bundle` on
one that has it writes the binary, the plugin, the wrapper and the script,
but no token. Copy it over and run `pwsh -File .\install.ps1 -Hub … -Token
…` there.

## 2. Optional features

As on NixOS, everything beyond reporting and answering prompts is off until
you turn it on. A switch adds what it needs where it is missing, at the
first install or any later run; what `config.toml` already says is kept.

| Feature                                      | Switch         | Sets                                                           |
| -------------------------------------------- | -------------- | -------------------------------------------------------------- |
| Replies from the panel (the channel)         | `-Channel`     | `[launch] channel`; the `claude` wrapper in your profile       |
| Open a background session                    | `-Terminal`    | `[attach] command`: `claude attach {id}`                       |
| Edit a prompt in your editor                 | `-Editor nvim` | `[edit] command`: `nvim {file}`                                |
| Sessions and prompt history on every machine | `-Sync`        | `[sync] enable`                                                |
| Model, context, cost and limits per session  | `-StatusLine`  | `ccdash tee` as Claude Code's statusline, in front of yours    |
| Tokens, cache hits, commits in Analytics     | `-Telemetry`   | `[telemetry] listen`; the `OTEL_*` variables in Claude's `env` |
| Lock and suspend from the panel              | —              | `[power] lock`, `suspend` in `config.toml`                     |

To turn one off, edit `config.toml` (the channel: also the profile's line)
and run the script again. Every key: [configuration](configuration.md).

**The terminal.** Windows has no terminal command to set: ccdash starts
`claude attach` and the editor in a console of their own, and Windows shows
it in its default terminal application (Settings › System › For developers
› Terminal), Windows Terminal or the console host. The editor must be a
console program that returns when the file is closed: exit status 0 takes
the text, anything else (`:cq`) cancels.

**The channel.** The wrapper starts interactive sessions with `--channels
plugin:ccdash@inline`, and sessions started from the panel get it through
`[launch] channel`. Claude Code loads it only from its allowlist,
`allowedChannelPlugins` in its managed settings (an organization's
server-managed settings outrank the machine's), as the NixOS module's
`programs.ccdash.channel.allow` does. Without it, `$env:CCDASH_CHANNEL_FLAG
= '--dangerously-load-development-channels'` loads the channel in the
wrapper's sessions, with a warning at every start.

## 3. Start at logon

The script adds `ccdash collector --background` to the `Run` key, which
needs no elevation (a scheduled task, `schtasks /SC ONLOGON`, would).
`--background` starts the collector again with no console window and
returns; a window opened for the start alone, the `Run` key's, closes at
once. Two starts at once leave one collector.

Its log, rewritten at each start, is
`%LOCALAPPDATA%\ccdash\state\collector.log`. Programs the collector runs
share its hidden console; an editor and an attach open a window of their
own.

From a script, start it with `Start-Process`, not by capturing its output:
a pipe it inherited stays open while the collector runs.

## 4. The hub's side

A collector token under the machine's host name
([installation](installation.md#41-the-hub)), and its workspace directories
as Windows writes them, next to the Linux ones:

```nix
services.ccdash-hub = {
  collectors.winbox = "/run/agenix/ccdash-collector-winbox";
  workspaces = [ "/home/me/code/work" ''C:\Users\me\code\work'' ];
};
```

A directory's last component names its workspace, so both are the
workspace `work`, and a session started from the panel goes to the
machine's own directory.

## 5. Updating

```powershell
pwsh -File windows\install.ps1 -Build                  # binary, plugin, wrapper
pwsh -File windows\install.ps1 -Build -Pack D:\bundle  # for a machine with no toolchain
```

Running sessions keep the old binary, which is renamed aside rather than
overwritten, and their channel reconnects to the new collector. The hub
and its collectors must speak the same protocol: update the hub with them
([installation](installation.md#8-updating)).

## 6. Where things are

| What        | Where                                                                      |
| ----------- | -------------------------------------------------------------------------- |
| config      | `%APPDATA%\ccdash\config.toml`                                             |
| state       | `%LOCALAPPDATA%\ccdash\state` (outbox, sync index, notes cache, the log)   |
| runtime     | `%LOCALAPPDATA%\ccdash\run` (pending titles)                               |
| sockets     | named pipes `\\.\pipe\ccdash-api-<hash>`, `\\.\pipe\ccdash-ingest-<hash>`  |
| binary      | `~\.local\bin\ccdash.exe`; `~\.local\share\ccdash\REV` names its commit    |
| plugin      | `~\.local\share\ccdash\claude-plugin`, linked as `~\.claude\skills\ccdash` |
| Claude Code | `%USERPROFILE%\.claude`, or `CLAUDE_CONFIG_DIR`                            |

The pipes' names hash the socket path under the runtime directory, so
`CCDASH_RUNTIME_DIR` still gives a second collector a place of its own.
They let in the current user only, and no remote client. The state and
runtime folders get a DACL of their own for the user alone, which what
they hold inherits, as mode 0700 does on Linux. `XDG_*` variables still
win over the folders above, but for `XDG_RUNTIME_DIR`: the pipes are
named after the runtime folder, which the collector and every hook must
agree on, and only `LOCALAPPDATA` is the same for all of them. The
collector's log names its pipes.

## 7. Differences

| Area               | On Windows                                                                                            |
| ------------------ | ----------------------------------------------------------------------------------------------------- |
| the hub, the demo  | refused: they run on Linux only                                                                       |
| stop a session     | Claude Code's tree ends at once (`TerminateProcess`), no SessionEnd hook; its console is then reset   |
| interrupt          | the running Bash call's process tree is ended at once                                                 |
| open               | no window focus; a background session is attached in a console of its own                             |
| editor             | in a console of its own; Windows Terminal keeps the tab open after a non-zero exit (`:cq`)            |
| machine            | no load average (reads 0) or CPU temperature; wake-on-LAN: the card is armed (`powercfg`, 10 min)     |
| lock, suspend      | `LockWorkStation`; `SetSuspendState` from a hidden PowerShell (sleep, not hibernation)                |
| state sync         | `ReadDirectoryChangesW` instead of inotify; sessions do not resume across Linux and Windows           |
| workspace sync     | one recursive watch on the root; no open-file check for quiescence; git dirs outside a unit unwatched |
| network shares     | not watched (no extended change records): a root on one is swept every 30 min                         |
| npm's `claude.cmd` | runs, through `cmd.exe`; a batch file takes no multi-line argument, so such prompts need `claude.exe` |
| claims             | compared as absolute paths: they only meet on machines with the same paths                            |

A session's transcripts live under its directory's name
(`C--Users-me-code-x`), which no Linux machine has, so the hub's search
finds Windows sessions but they resume only on Windows.
