# ccdash

A live dashboard for all your running Claude Code sessions: which ones are
working, which are waiting on you, and which look stuck. On the desktop as a
[DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) plugin,
on the phone as a web page.

> **Status:** work in progress. The collector daemon, the web UI and the
> DMS plugin work; several hosts are not supported yet.

## How it works

```text
Claude Code hooks ──► ccdash emit ─┐
statusline ─────────► ccdash tee ──┼──► ccdash daemon ──┬──► web UI (LAN / VPN)
claude agents --json ──────────────┤    (per host)      └──► DMS plugin
niri window events ────────────────┘
```

- Every session reports through Claude Code hooks to a small daemon on the
  same machine. The daemon keeps live state in memory and history in SQLite.
- The **web UI** is meant for the phone: it reads, renames sessions, and
  (with the channel enabled) sends them messages and answers their
  permission prompts. "Open in Claude" opens a session in the Claude app
  through Remote Control.
- The **DMS plugin** adds a bar indicator, a quick list, and a full dashboard
  tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`, hooks,
the statusline payload, [channels](https://code.claude.com/docs/en/channels-reference));
no internal state files.

## Usage

With Home Manager (the module also wires the hooks into
`programs.claude-code`):

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    settings.http.listen = [ "[::]:7837" ];
    # optional: filter the UI by workspace (named by the last path component)
    settings.workspaces.dirs = [ "~/code/work" "~/code/personal" ];
    # keep your statusline, ccdash reads the stats on its way through
    claudeCode.statusLine = {
      enable = true;
      command = lib.getExe pkgs.ccstatusline;
    };
  };
}
```

Then:

```sh
ccdash status             # what every session is doing
ccdash open <id>          # focus its window, or attach to a background session
ccdash rename <id> <name> # rename a session (Claude adopts it at the next prompt)
ccdash reply <id> <text>  # send a message to a session (needs the channel)
ccdash url --lan --qr     # web address for the phone as a QR code, token included
```

The web UI lives on the HTTP port:

- **Sessions:** the ones that need you first, then working and idle, then
  background agents (sessions Claude started, or `claude --bg`), which show
  the session that started them and "Finished" instead of "Your turn". Each
  card shows the pending permission or the running tool and what Claude is
  saying, task progress, the current turn (time, tool calls, edited files,
  subagents), context, cost, CPU and memory, and the last hour of activity.
- **Conversation:** your prompts, what Claude writes between tool calls as
  it streams, the tool calls themselves, subagent results, compactions and
  replies. Details has the task list, running subagents, the session's
  processes with CPU and memory, and totals.
- **History:** activity timeline, time per project, most used tools and
  5-hour limit usage.
- **Analytics:** daily activity, when you prompt (weekday × hour), how long
  Claude works per prompt and how long sessions wait for you, cost per day
  and per model, lines changed, memory and CPU per project.

History and Analytics share a time range picker: quick ranges or an exact
from–to, `‹ ›` to step by the range's length, `⊖` to double it, and
drag across any time chart with a mouse to zoom in. With
`workspaces.dirs` set, a workspace switch filters every view.

To connect a phone, scan `ccdash url --lan --qr` (or open "Connect a phone"
in the web UI on the desktop). The address carries the access token; each
browser keeps it, so it is a one-time step per device. The token lives in
`~/.local/state/ccdash/token` and survives reboots; delete it and restart
ccdash to lock every device out. The DMS plugin adds it to the links it
opens by itself.

Sessions can be renamed from the web UI and the DMS plugin. ccdash shows the
new name at once and hands it to Claude Code at the session's next prompt
(or resume), through the `sessionTitle` output of its own hook, so the
terminal title and Remote Control pick it up too.

### Replying from the phone

With the channel enabled, the session sheet gets a message box, and a
permission prompt shows up above it with the tool, its arguments, and
Allow/Deny. Whichever answer comes first, the terminal's or ccdash's, wins.
A message sent while Claude works waits for its next turn. Messages reach
Claude as `<channel source="plugin:ccdash:ccdash">` events (the terminal
shows `← ccdash: …`), and ccdash marks them "sent from ccdash".

This uses Claude Code
[channels](https://code.claude.com/docs/en/channels-reference), a research
preview: `ccdash channel` is an MCP server that every session started with
the channel flags runs, connected to the daemon. Two pieces turn it on:

```nix
# Home Manager: a `claude` wrapper that starts interactive sessions with
# `--channels plugin:ccdash@inline --plugin-dir …`
services.ccdash.claudeCode.channel.enable = true;

# NixOS: allowlist the plugin in Claude Code's managed settings, which
# replaces Anthropic's default channel allowlist on this machine
imports = [ inputs.ccdash.nixosModules.default ];
programs.ccdash.channel.allow = true;
```

Without the allowlist entry, `claudeCode.channel.devFlag = true` loads the
channel with `--dangerously-load-development-channels` instead, and Claude
Code then asks for confirmation at every launch. Sessions started before
this (or without the wrapper) show how to restart them. Team and Enterprise
accounts also need an admin to enable channels for the organization.

Anyone holding the token can now prompt your sessions and approve their
tool calls, so treat it like an SSH key: keep the port on your LAN or VPN,
and delete the token file if a device goes missing.

Claude Code reads hooks when a session starts, so sessions that were already
running when ccdash was installed only show busy/idle until you restart
them (`claude --resume` keeps the conversation). The conversation is built
from hook events; Claude's thinking is not part of them.

CPU and memory come from `/proc`: every 5 s ccdash sums the CPU time of each
session's process tree (Claude, its MCP servers and running commands) and
its memory as PSS, so shared pages are not counted twice. Turn it off with
`procs.enable = false`.

The HTTP API needs the bearer token for everything (it can read sessions,
rename them and, with the channel, prompt them and answer their permission
prompts) (generated in
`~/.local/state/ccdash/token` unless `http.token_file` is set). Keep the
port behind your firewall or VPN.

## Desktop (DankMaterialShell)

`services.ccdash.dms.enable = true;` links the [DMS plugin](dms-plugin/) into
DankMaterialShell: a bar pill with the sessions that need you, a dash tab and
card, notifications when a session starts waiting, and
`dms ipc call ccdash focusWaiting` to jump to the session that has waited
longest. Enable it once under Settings → Plugins and add the widget to a bar.
Handy niri binds:

```kdl
Mod+C { spawn "dms" "ipc" "call" "dash" "toggle" "plugin_ccdash"; }
Mod+Shift+C { spawn "dms" "ipc" "call" "ccdash" "focusWaiting"; }
```

## Data

Everything stays on the machine running the daemon:

| What       | Where                                                       |
| ---------- | ----------------------------------------------------------- |
| Live state | daemon memory                                               |
| History    | `~/.local/state/ccdash/ccdash.db` (SQLite, WAL), owner-only |
| Web token  | `~/.local/state/ccdash/token`, owner-only                   |
| Config     | `~/.config/ccdash/config.toml`                              |
| Sockets    | `$XDG_RUNTIME_DIR/ccdash/` (`ingest.sock`, `api.sock`)      |

The database keeps 30 days (`store.retention_days`) of:

- `events`: prompts (up to 8 KiB), Claude's messages and replies (up to
  32 KiB), tool calls (name, file path or first command line, duration),
  permission requests, task changes, subagent results, compactions and
  state changes
- `samples`: once a minute per session from the statusline (context,
  cost, rate limits, model, lines changed)
- `proc_samples`: once a minute per session (CPU, memory, process count)
- `sessions`: id, directory, title, kind, start and end

Nothing is sent anywhere; the HTTP API serves it to your browser with the
token. Delete the database to start over.

## Development

```sh
direnv allow   # or: nix develop
cargo run -- daemon
nix build
```

`CCDASH_WEB_DIR=web` serves the UI from disk instead of the embedded copy,
so edits show up on reload.

## License

[EUPL-1.2](LICENSE)
