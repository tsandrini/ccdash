# A Windows workstation

A Windows machine can be a workstation: the collector, the hooks, the CLI,
the plugin and the channel run there and report to a hub on Linux. The hub
runs on Linux only, as do `ccdash demo` (a hub), the DankMaterialShell
plugin and niri.

Windows is kept working by the people who run it. ccdash is developed and
used on NixOS; a [CI job](development.md#3-checks) builds and tests the
Windows side on every change, and a new feature may reach Windows later
than Linux.

1. [Install](#1-install)
2. [Optional features](#2-optional-features)
3. [The tray](#3-the-tray)
4. [The hub's side](#4-the-hubs-side)
5. [Updating](#5-updating)
6. [Removing](#6-removing)
7. [Where things are](#7-where-things-are)
8. [Differences](#8-differences)

## 1. Install

You need Claude Code 2.1.157 or later and a collector token the hub lists
under this machine's host name (section 4). ccdash for Windows is one file,
`ccdash-x86_64.exe` of the
[latest release](https://github.com/tsandrini/ccdash/releases/latest).
Download it and run its setup, as yourself (no elevation):

```powershell
curl.exe -fLO https://github.com/tsandrini/ccdash/releases/latest/download/ccdash-x86_64.exe
.\ccdash-x86_64.exe win setup --hub wss://ccdash.example.org/collect --token .\token
```

The file is not signed: downloaded with a browser, Windows asks before it
runs it (More info › Run anyway); `curl.exe` leaves no such mark on it.
Setup installs:

- `ccdash.exe` in `~\.local\bin`, which joins your `PATH` (the downloaded
  file may go);
- the ccdash plugin in `~\.local\share\ccdash\claude-plugin`, linked into
  `~\.claude\skills\ccdash`: every session gets the knowledge tools and the
  `ccdash:wrap-up`, `ccdash:report` and `ccdash:project` skills;
- the collector's configuration, `%APPDATA%\ccdash\config.toml`, and its
  token, readable by you alone;
- the hooks in `~\.claude\settings.json`, next to any of your own (the file
  as it was stays as `settings.json.before-ccdash`);
- the tray, started now and at every logon (section 3): it runs the
  collector;
- an entry in Windows' installed apps (Settings › Apps), whose Uninstall
  removes ccdash again (section 6).

The host name is the DNS host name, lowercased; `--host` sets another.

Setup is safe to run again: what is in place is kept. To build ccdash
yourself you need Rust with a C compiler (ring and the bundled SQLite need
one: the MSVC toolchain, or the GNU one with MinGW); then `cargo build
--release` and `target\release\ccdash.exe win setup …`. Such a build does not
update itself (section 5).

## 2. Optional features

As on NixOS, everything beyond reporting and answering prompts is off until
you turn it on. A switch of `ccdash win setup` adds what it needs where it is
missing, at the first install or any later run; what `config.toml` already
says is kept.

| Feature                                      | Switch          | Sets                                                                                                  |
| -------------------------------------------- | --------------- | ----------------------------------------------------------------------------------------------------- |
| Replies from the panel (the channel)         | `--channel`     | `[launch] channel`; the `claude` wrapper in your profile                                              |
| Open a background session                    | `--terminal`    | `[attach] command`: `claude attach {id}`                                                              |
| Edit a prompt in your editor                 | `--editor nvim` | `[edit] command`: `nvim {file}`                                                                       |
| Sessions and prompt history on every machine | `--sync`        | `[sync] enable`                                                                                       |
| Workspaces under a directory of this machine | `--root D:\src` | `[workspace] root`: where the hub's relative workspaces are here ([the hub's side](#4-the-hubs-side)) |
| Model, context, cost and limits per session  | `--status-line` | `ccdash tee` as Claude Code's statusline, in front of yours                                           |
| Tokens, cache hits, commits in Analytics     | `--telemetry`   | `[telemetry] listen`; the `OTEL_*` variables in Claude's `env`                                        |
| Lock and suspend from the panel              | —               | `[power] lock`, `suspend` in `config.toml`                                                            |
| Dictation, the words of attached recordings  | —               | `[transcribe] command` in `config.toml` ([attachments](attachments.md#7-dictation))                   |
| Still frames of attached videos              | —               | `[video] ffmpeg` in `config.toml` ([attachments](attachments.md#8-recordings-and-videos))             |

To turn one off, or to change anything else, edit `config.toml` (the tray's
menu opens it) and restart the collector from the same menu; the channel
also has its line in the profile. Every key: [configuration](configuration.md).

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
wrapper's sessions, with a warning at every start. The wrapper goes into
PowerShell 7's profile; another shell loads
`~\.local\share\ccdash\claude-channel.ps1` from its own.

## 3. The tray

`ccdash win tray` runs the collector under an icon in the taskbar's
notification area; Windows keeps a new icon under `^` until you drag it
out. The icon is ccdash's three bars, and a dot in its corner says what
wants a look:

| Dot    | Means                                                          |
| ------ | -------------------------------------------------------------- |
| none   | connected to the hub, nothing waits                            |
| yellow | a session waits for you: a permission prompt, or your turn     |
| grey   | the hub is out of reach; the collector queues meanwhile        |
| red    | the collector does not run, or the hub speaks another protocol |
| blue   | an update is being put in place                                |

The pointer over the icon reads the same in words. A click opens the panel
in the browser. A right click has the rest: the build and how it is doing,
Open the panel, Open the configuration (`config.toml`, in the program
Windows has for it, else Notepad), Show the log, Restart the collector
(after a change to the configuration), Check for updates and Quit, which
stops the collector too. There is nothing else to set there: the
configuration is the file.

The tray starts the collector with no console window, its output in
`%LOCALAPPDATA%\ccdash\state\collector.log` (the run before:
`collector.log.old`), and starts it again when it ends: after a second,
then ever more slowly, up to a minute. Programs the collector runs share
its hidden console; an editor and an attach open a window of their own. A
collector started some other way (`ccdash collector` in a terminal) is left
to run and shown like its own. What the tray itself did is in `tray.log`,
next to the collector's.

Setup puts `ccdash win tray` into the `Run` key, which needs no elevation (a
scheduled task, `schtasks /SC ONLOGON`, would); Task Manager › Startup apps
turns it off and on. From a terminal, `ccdash win tray` returns at once and
leaves the tray running. A released build opens no console window at logon
on Windows 11 24H2 and later; on an earlier Windows, and with a build of
your own, one opens for a moment.

## 4. The hub's side

A collector token under the machine's host name
([installation](installation.md#41-the-hub)). The workspaces need nothing
written for Windows when they are under the home or the root:

```nix
services.ccdash-hub = {
  collectors.winbox = "/run/agenix/ccdash-collector-winbox";
  workspaces = [ "~/code/work" "clients" ];
};
```

`~/code/work` is `C:\Users\me\code\work` here and `/home/me/code/work` on
a Linux machine; `clients` is under each machine's workspace root, here
what `--root` set (`D:\src\clients`). Both are one workspace on every
machine, and a session started or resumed from the panel goes to the
machine's own directory. A path of its own (`''D:\work''`) names the
workspace `work` on the machines that have that path
([getting started](getting-started.md#2-workspaces-and-projects)).

## 5. Updating

A released build updates itself, and it follows the hub. The hub and its
collectors must speak the same protocol
([installation](installation.md#8-updating)), so the newest build is the
wrong one whenever the hub has not reached it yet. Each protocol number
therefore has a release of its own, `windows-p19` for protocol 19, which
holds the newest build that speaks it. The tray looks at the release of the
hub's protocol:

- a while after it starts, and every four hours: a newer build of the same
  protocol. It is taken once no session works or waits at a permission
  prompt, since the collector starts anew with it and the prompts it holds
  for the panel would go;
- at once when the hub refuses the collector for its protocol, and every
  two minutes until that protocol's release is there: the build the hub
  calls for, newer or older than the running one.

So: update the hub, and the Windows machines follow within minutes; a hub
that stays behind keeps them on its protocol. The tray looks also when its
collector does not run, as a build that cannot start is the one that most
needs its successor. A look that fails (no network yet after a logon) is
repeated ten minutes later and says so in the menu only; a build that came
and could not be used (not the file its manifest describes, or one that
does not run) is not fetched again until the release holds another, or you
ask with Check for updates.

A build other than the running one is downloaded, checked against the
release's manifest (its size and SHA-256, and what the build says it is)
and put in place; then the tray starts again with it, and so does its
collector. Running sessions keep the old binary, which is renamed aside
(`ccdash.exe.old-…`, removed once nothing runs it) rather than overwritten,
and their channel reconnects to the new collector. An update also brings
the build's plugin and `claude` wrapper and the hooks it may have added to
`settings.json`; it changes nothing else.

By hand, `ccdash win update` does the same from a terminal, and `ccdash win
update --check` only says what the releases hold. `ccdash --version` names
the build's commit and protocol. Any release's `ccdash-x86_64.exe win setup`
installs that build over the one in place.

A build of your own names no releases and stays as it is: build again and
run its `win setup`.

## 6. Removing

Uninstall ccdash in Settings › Apps › Installed apps, or run `ccdash win setup
--remove`. It stops the tray and the collector and takes out what setup put
in:

- the start at logon and the entry in the installed apps;
- ccdash's hooks, statusline and telemetry variables in `settings.json`.
  What is yours stays (telemetry that goes to an endpoint of your own
  too), and a statusline ccdash stood in front of is yours again;
- the line in the PowerShell profile that loads the `claude` wrapper;
- the plugin and its link in `~\.claude\skills`;
- the binary. One a process still runs (a session's channel, the
  uninstall itself) is moved into the temporary folder, which Windows
  clears in its own time.

The hooks and the profile's line go first: when they cannot be taken out
(a `settings.json` that does not parse), nothing else is removed, ccdash
stays in the installed apps, and `remove.log` in the state folder says why,
as Settings shows no output.

The configuration with its token and the state stay, for a later install;
`ccdash win setup --remove --purge` takes them too. `~\.local\bin` stays on
your `PATH`, as Claude Code lives there as well. Sessions that are open
call ccdash's hooks until they are started again.

## 7. Where things are

| What           | Where                                                                                                                     |
| -------------- | ------------------------------------------------------------------------------------------------------------------------- |
| config         | `%APPDATA%\ccdash\config.toml`                                                                                            |
| state          | `%LOCALAPPDATA%\ccdash\state` (outbox, sync index, notes cache, fetched attachments, the logs)                            |
| runtime        | `%LOCALAPPDATA%\ccdash\run` (pending titles)                                                                              |
| sockets        | named pipes `\\.\pipe\ccdash-api-<hash>`, `\\.\pipe\ccdash-ingest-<hash>`                                                 |
| binary         | `~\.local\bin\ccdash.exe`; `ccdash --version` names its commit                                                            |
| plugin         | `~\.local\share\ccdash\claude-plugin`, linked as `~\.claude\skills\ccdash`                                                |
| at logon       | `ccdash` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`                                                       |
| installed apps | `ccdash` under `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall`; its icon is `~\.local\share\ccdash\ccdash.ico` |
| Claude Code    | `%USERPROFILE%\.claude`, or `CLAUDE_CONFIG_DIR`                                                                           |

The pipes' names hash the socket path under the runtime directory, so
`CCDASH_RUNTIME_DIR` still gives a second collector a place of its own.
They let in the current user only, and no remote client. The state and
runtime folders get a DACL of their own for the user alone, which what
they hold inherits, as mode 0700 does on Linux. `XDG_*` variables still
win over the folders above, but for `XDG_RUNTIME_DIR`: the pipes are
named after the runtime folder, which the collector and every hook must
agree on, and only `LOCALAPPDATA` is the same for all of them. The
collector's log names its pipes. One tray runs per state folder, and it
ends and restarts only the collector of its own pipes. Such a variable
belongs to your user (`setx`), not to one shell: the tray that starts at
logon has to see it too, and `ccdash win setup` warns when it would not.

## 8. Differences

| Area               | On Windows                                                                                            |
| ------------------ | ----------------------------------------------------------------------------------------------------- |
| the hub, the demo  | refused: they run on Linux only                                                                       |
| stop a session     | Claude Code's tree ends at once (`TerminateProcess`), no SessionEnd hook; its console is then reset   |
| interrupt          | the running Bash call's process tree is ended at once                                                 |
| open               | no window focus; a background session is attached in a console of its own                             |
| editor             | in a console of its own; Windows Terminal keeps the tab open after a non-zero exit (`:cq`)            |
| machine            | no load average (reads 0) or CPU temperature; wake-on-LAN: the card is armed (`powercfg`, 10 min)     |
| lock, suspend      | `LockWorkStation`; `SetSuspendState` from a hidden PowerShell (sleep, not hibernation)                |
| state sync         | `ReadDirectoryChangesW` instead of inotify                                                            |
| workspace sync     | one recursive watch on the root; no open-file check for quiescence; git dirs outside a unit unwatched |
| network shares     | not watched (no extended change records): a root on one is swept every 30 min                         |
| npm's `claude.cmd` | runs, through `cmd.exe`; a batch file takes no multi-line argument, so such prompts need `claude.exe` |

A session's transcripts live under its directory's name
(`C--Users-me-code-x`), which no Linux machine has. Claude Code finds a
session by its id whatever that name is, so a Windows session should
resume on Linux and the other way round as between two Linux machines
whose paths differ ([state sync](state-sync.md#1-turning-it-on)); between
Windows and Linux nobody has tried it yet.
