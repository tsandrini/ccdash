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
prompts ────────────► ccdash ask ──┤
statusline ─────────► ccdash tee ──┼──► ccdash daemon ──┬──► web UI (LAN / VPN)
claude agents --json ──────────────┤    (per host)      ├──► DMS plugin
claude -p /usage ──────────────────┤                    └──► ntfy (phone, watch)
niri window events ────────────────┘
```

- Every session reports through Claude Code hooks to a small daemon on the
  same machine. The daemon keeps live state in memory and history in SQLite.
- The **web UI** is meant for the phone: it reads, renames sessions,
  answers their permission prompts, questions and plans, and (with the
  channel enabled) sends them messages. "Open in Claude" opens a session in
  the Claude app through Remote Control.
- **Notifications** through [ntfy](https://ntfy.sh) reach the phone (and a
  watch mirroring it) with buttons that answer the prompt.
- The **DMS plugin** adds a bar indicator, a quick list, and a full dashboard
  tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`, hooks,
the statusline payload, [channels](https://code.claude.com/docs/en/channels-reference),
[OpenTelemetry](https://code.claude.com/docs/en/monitoring-usage), the
`/usage` report); no internal state files.

The statusline carries only the 5-hour and 7-day limits. For per-model
weekly limits such as Fable's, the daemon runs `claude -p /usage` every 10
minutes (`usage.interval_secs`) in safe mode: no model request, no hooks,
plugins or memory, nothing saved; about a second of CPU. It reads the
structured report (`SDKUsageReport` in the
[Agent SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript),
experimental). `usage.command = []` turns it off.

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

- **Sessions:** the ones that need you first, then working, parked and idle,
  then background agents (sessions Claude started, or `claude --bg`), which show
  the session that started them and "Finished" instead of "Your turn". Each
  card shows the pending permission or the running tool and what Claude is
  saying, task progress, the current turn (time, tool calls, edited files,
  subagents), context, cost, CPU and memory, and the last hour of activity.
  A session left on your turn for an hour is _parked_: most likely done, it
  no longer counts as needing you.
- **Conversation:** your prompts, what Claude writes between tool calls as
  it streams, the tool calls themselves, subagent results, compactions and
  replies. Details has the task list, running subagents, the session's
  processes with CPU and memory, and totals.
- **History:** activity timeline, time per project, most used tools and
  5-hour limit usage.
- **Analytics**, in sections:
  - _Overview:_ daily activity, when you prompt (weekday × hour), how long
    Claude works per prompt and how long sessions wait for you, cost per day
    and per model, lines changed, memory and CPU per project.
  - _Waits:_ how long sessions waited on you and how fast you replied, apart
    from parked time. A wait runs from Claude handing the turn back to what
    came next: a reply within an hour counts as waiting on you; a later
    reply, closing the session or leaving it open counts as parked; Claude
    carrying on by itself (a background task, a wakeup) counts as neither.
    Permission prompts always count. Lists the sessions you came back to
    late.
  - _Parallel:_ how many sessions worked or waited on you at once, and how
    long everything waited on you with nothing working.
  - _Limits:_ the 5-hour and 7-day limits, and per-model weekly limits
    such as Fable's, with their pace and where they end up at the reset,
    the peak of each 5-hour window, and each workspace's share of the
    week.
  - _Projects:_ a sortable table of time, prompts, cost, lines, tool calls,
    compactions and CPU per directory.
  - _Turns:_ what fills a turn (Claude, tools, permission prompts), tools and
    shell commands by total time, the slowest calls and the commands that
    fail most.
  - _Context:_ compactions and how full sessions got.
  - _Tokens:_ tokens by type, cache hit rate, API latency per model,
    commits, pull requests, edits by language, and your active time against
    Claude's (needs telemetry, below).
  - _Permissions:_ what asks you most, grouped by the allow rule that would
    cover it, how you answered and how fast, and what auto mode blocked.

History and Analytics share a time range picker: quick ranges or an exact
from–to, `‹ ›` to step by the range's length, `⊖` to double it, and
drag across any time chart with a mouse to zoom in. With
`workspaces.dirs` set, a workspace filter narrows every view to one or
several workspaces. It reads "All" (or what you picked) until you open it,
so the other names stay out of sight; each browser remembers its choice.

To connect a phone, scan `ccdash url --lan --qr` (or open "Connect a phone"
in the web UI on the desktop). The address carries the access token; each
browser keeps it, so it is a one-time step per device. The token lives in
`~/.local/state/ccdash/token` and survives reboots; delete it and restart
ccdash to lock every device out. The DMS plugin adds it to the links it
opens by itself.

The Tokens section, and the approval sources under Permissions, come from
Claude Code's OpenTelemetry export. `claudeCode.telemetry.enable = true` sets
the `OTEL_*` variables in `programs.claude-code.settings.env` and starts an
OTLP/HTTP receiver in the daemon on `127.0.0.1:7839`. It takes no token, so
ccdash refuses to bind it anywhere but loopback, and it keeps only counts and
timings: prompt text stays redacted, as Claude Code sends it by default.
Sessions started after the switch report.

Sessions can be renamed from the web UI and the DMS plugin. ccdash shows the
new name at once and hands it to Claude Code at the session's next prompt
(or resume), through the `sessionTitle` output of its own hook, so the
terminal title and Remote Control pick it up too.

### Answering from the phone

Tool approvals, questions (`AskUserQuestion`) and plans (`ExitPlanMode`)
show up at the bottom of the session sheet while the terminal dialog stays
open; whichever answer comes first wins:

- **Tool calls:** Allow, Deny, and Always allow when Claude Code suggests a
  rule (saved where the terminal's "don't ask again" saves it, usually the
  project's `.claude/settings.local.json`). Deny stops Claude like the
  terminal's No; "Deny with a note" lets it carry on with your note.
- **Questions:** one button per option, several for multi-select, or your
  own text.
- **Plans:** Approve, or Keep planning (with a note).

The PermissionRequest hook is `ccdash ask`: it hands the prompt to the
daemon and holds it until ccdash answers or the prompt closes in the
terminal. Claude Code runs it alongside the dialog (verified on 2.1.302;
the [hooks reference](https://code.claude.com/docs/en/hooks) promises this
only for SDK hosts, so recheck after updates).
`claudeCode.asks.enable = false` makes the hook report prompts only.

### Notifications

The daemon can push prompts and finished turns to an
[ntfy](https://docs.ntfy.sh) server; the ntfy app on the phone shows them
with buttons (Allow / Deny / Always, up to three options of a question,
Approve / Keep planning), which a watch mirroring the phone's notifications
can press too. A notification waits 15 s (2 minutes while the session's
window has focus), so a prompt answered at the desk never sends one, and it
is deleted from the phone once the prompt is answered anywhere.

```nix
services.ccdash.settings.notify = {
  url = "https://ntfy.example.org";
  topic = "ccdash";
  token_file = "/run/agenix/ccdash-ntfy-token"; # may publish to the topic
  # how the phone reaches this daemon (buttons, taps); default: LAN address
  web_url = "http://10.0.0.2:7837";
  # turns = false;  questions, plans, permissions: per-kind switches
  # workspaces = [ "work" ];  only these workspaces
};
```

A button calls `POST /act/<key>/<action>` on the daemon with a random key
that belongs to that one prompt and dies with it; the access token never
goes into a notification. The phone needs to reach the daemon (LAN or
VPN) for the buttons to work. Notifications carry session titles, commands
and questions, so use an ntfy server you run, with access control. Keeping
it on the same LAN or VPN costs nothing (the buttons need it anyway), and a
short in-memory cache (`cache-duration`, no `cache-file`) keeps the text off
its disk.

### Replying from the phone

With the channel enabled, the session sheet gets a message box. A message
sent while Claude works waits for its next turn. Messages reach Claude as
`<channel source="plugin:ccdash:ccdash">` events (the terminal shows
`← ccdash: …`), and ccdash marks them "sent from ccdash".

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
Code then asks for confirmation at every launch. Claude Code's startup
notice still lists `plugin:ccdash@inline · plugin not installed`: it only
counts plugins installed from a marketplace, and the channel works anyway.
Sessions started before this (or without the wrapper) show how to restart
them. Team and Enterprise accounts also need an admin to enable channels for
the organization.

Anyone holding the token can answer your sessions' prompts and, with the
channel, prompt them, so treat it like an SSH key: keep the port on your LAN
or VPN, and delete the token file if a device goes missing.

Claude Code reads hooks when a session starts, so sessions that were already
running when ccdash was installed only show busy/idle until you restart
them (`claude --resume` keeps the conversation). The conversation is built
from hook events; Claude's thinking is not part of them.

CPU and memory come from `/proc`: every 5 s ccdash sums the CPU time of each
session's process tree (Claude, its MCP servers and running commands) and
its memory as PSS, so shared pages are not counted twice. Turn it off with
`procs.enable = false`.

The HTTP API needs the bearer token for everything (it can read sessions,
rename them, answer their prompts and, with the channel, prompt them),
except the notification buttons' one-time links. The token is generated in
`~/.local/state/ccdash/token` unless `http.token_file` is set. Keep the
port behind your firewall or VPN.

## Desktop (DankMaterialShell)

`services.ccdash.dms.enable = true;` links the [DMS plugin](dms-plugin/) into
DankMaterialShell: a bar pill with the sessions that need you, a dash tab and
card, notifications when a session starts waiting, and
`dms ipc call ccdash focusWaiting` to jump to the session that has waited
longest (parked sessions are skipped). Enable it once under Settings →
Plugins and add the widget to a bar. Handy niri binds:

```kdl
Mod+C { spawn "dms" "ipc" "call" "dash" "toggle" "plugin_ccdash"; }
Mod+Shift+C { spawn "dms" "ipc" "call" "ccdash" "focusWaiting"; }
```

## Data

Everything is stored on the machine running the daemon:

| What       | Where                                                       |
| ---------- | ----------------------------------------------------------- |
| Live state | daemon memory                                               |
| History    | `~/.local/state/ccdash/ccdash.db` (SQLite, WAL), owner-only |
| Web token  | `~/.local/state/ccdash/token`, owner-only                   |
| Config     | `~/.config/ccdash/config.toml`                              |
| Sockets    | `$XDG_RUNTIME_DIR/ccdash/` (`ingest.sock`, `api.sock`)      |
| Telemetry  | `127.0.0.1:7839` (OTLP/HTTP JSON), when enabled             |

The database keeps 30 days (`store.retention_days`) of:

- `events`: prompts (up to 8 KiB), Claude's messages and replies (up to
  32 KiB), tool calls (name, file path or first command line, duration),
  permission requests and how ccdash answered them, task changes, subagent
  results, compactions and state changes
- `samples`: once a minute per session from the statusline (context,
  cost, rate limits, model, lines changed)
- `proc_samples`: once a minute per session (CPU, memory, process count)
- `sessions`: id, directory, title, kind, start and end
- `limit_samples`: per-model weekly limits from each `/usage` poll
- `telemetry` (with telemetry on): API requests (model, tokens, cost,
  latency), API errors, tool approval decisions, commits, pull requests,
  edit decisions and active time

The HTTP API serves it to your browser with the token. Nothing else leaves
the machine, except notifications when `notify.url` is set. Delete the
database to start over.

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
