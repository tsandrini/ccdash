# ccdash

A live dashboard for all your running Claude Code sessions: which ones are
working, which are waiting on you, and which look stuck. On the desktop as a
[DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) plugin,
on the phone as a web page.

> **Status:** work in progress. One hub, collectors on each workstation,
> the web UI, the DMS plugin and the knowledge layer work.

## How it works

```text
on each workstation                      on the hub (always on)
Claude Code hooks ──► ccdash emit ─┐
prompts ────────────► ccdash ask ──┤
statusline ─────────► ccdash tee ──┼──► ccdash collector ══wss══► ccdash hub
claude agents --json ──────────────┤      │ (forwards, acts)      │ (state, SQLite,
claude -p /usage ──────────────────┤      └──► DMS plugin         │  notifications)
niri window events ────────────────┘                              ├──► web UI
                                                                  └──► ntfy
```

- Every session reports through Claude Code hooks to a small **collector**
  on the same machine, which forwards everything to the **hub** over one
  WebSocket (`/collect`, a token per machine) and carries out what comes
  back: replies into sessions, prompt answers, renames. While the hub is
  unreachable, hook events wait in an on-disk queue and Claude Code runs
  as if ccdash were not there.
- The hub keeps every machine's live state in memory and history in
  SQLite, and serves the **web UI**, meant for the phone: it reads,
  renames sessions, answers their permission prompts, questions and plans,
  and (with the channel enabled) sends them messages. "Open in Claude"
  opens a session in the Claude app through Remote Control. Every view can
  be narrowed to one host.
- **Notifications** reach the phone (and a watch mirroring it) with
  buttons that answer the prompt: through [ntfy](https://ntfy.sh), or as
  Web Push to the web UI installed as an app, or both.
- The **DMS plugin** on each workstation shows that machine's sessions
  (mirrored from the hub): a bar indicator, a quick list, and a full
  dashboard tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`, hooks,
the statusline payload, [channels](https://code.claude.com/docs/en/channels-reference),
[OpenTelemetry](https://code.claude.com/docs/en/monitoring-usage), the
`/usage` report); no internal state files.

The statusline carries only the 5-hour and 7-day limits. For per-model
weekly limits such as Fable's, the collector runs `claude -p /usage` every
10 minutes (`usage.interval_secs`) in safe mode: no model request, no
hooks, plugins or memory, nothing saved; about a second of CPU. It reads
the structured report (`SDKUsageReport` in the
[Agent SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript),
experimental). `usage.command = []` turns it off.

## Usage

The hub is a NixOS service. It listens on loopback; put nginx (or any TLS
proxy that passes WebSockets through) in front of it:

```nix
{
  imports = [ inputs.ccdash.nixosModules.hub ];

  services.ccdash-hub = {
    enable = true;
    tokenFile = "/run/agenix/ccdash-ui-token";            # web UI bearer token
    collectors = {
      desk = "/run/agenix/ccdash-collector-desk";          # one token per machine
      laptop = "/run/agenix/ccdash-collector-laptop";
    };
    nginx = { enable = true; domain = "ccdash.example.org"; };
    backup.enable = true;                                  # nightly copy, checked
    settings = {
      # optional: filter the UI by workspace (named by the last path component)
      workspaces.dirs = [ "/home/me/code/work" "/home/me/code/personal" ];
      store.retention_days = 90;
    };
  };
  # TLS, listen addresses and access control for the vhost are yours:
  services.nginx.virtualHosts."ccdash.example.org" = { ... };
}
```

Each workstation runs the collector through Home Manager (the module also
wires the hooks into `programs.claude-code`):

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    hub.url = "wss://ccdash.example.org/collect";
    hub.tokenFile = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-desk";
    # keep your statusline, ccdash reads the stats on its way through
    claudeCode.statusLine = {
      enable = true;
      command = lib.getExe pkgs.ccstatusline;
    };
  };
}
```

The collector announces its host name (`host` in its config, the kernel's
by default); the hub must list a token under that name.

Then, on a workstation:

```sh
ccdash status             # this machine's sessions as the hub sees them
ccdash open <id>          # focus its window, or attach to a background session
ccdash rename <id> <name> # rename a session (Claude adopts it at the next prompt)
ccdash reply <id> <text>  # send a message to a session (needs the channel)
ccdash url                # the hub's web address
```

And on the hub:

```sh
ccdash url --hub --config /etc/ccdash/hub.toml --qr   # pairing link with the token
ccdash import db old.db --config /etc/ccdash/hub.toml # history of a pre-hub daemon
ccdash backup --config /etc/ccdash/hub.toml copy.db   # what the nightly timer runs
```

The web UI lives on the hub:

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
With more than one machine reporting, a host filter sits next to it, and
cards name their machine.

To connect a phone, scan `ccdash url --hub --qr` on the hub (or open
"Connect a phone" in the web UI on a paired browser). The address carries
the access token; each browser keeps it, so it is a one-time step per
device. The token comes from `tokenFile`; rotate it and restart the hub to
lock every device out. The DMS plugin opens the hub's address without a
token, so pair the desktop browser once.

The Tokens section, and the approval sources under Permissions, come from
Claude Code's OpenTelemetry export. `claudeCode.telemetry.enable = true` sets
the `OTEL_*` variables in `programs.claude-code.settings.env` and starts an
OTLP/HTTP receiver in the collector on `127.0.0.1:7839`, which forwards
the kept records to the hub. It takes no token, so ccdash refuses to bind
it anywhere but loopback, and it keeps only counts and timings: prompt
text stays redacted, as Claude Code sends it by default. Sessions started
after the switch report.

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
collector, which holds it until the hub answers or the prompt closes in
the terminal (or the hub link drops: then the hook lets go and the
terminal alone decides). Claude Code runs it alongside the dialog
(verified on 2.1.302; the
[hooks reference](https://code.claude.com/docs/en/hooks) promises this
only for SDK hosts, so recheck after updates).
`claudeCode.asks.enable = false` makes the hook report prompts only.

### Notifications

The hub can push prompts and finished turns to an
[ntfy](https://docs.ntfy.sh) server; the ntfy app on the phone shows them
with buttons (Allow / Deny / Always, up to three options of a question,
Approve / Keep planning), which a watch mirroring the phone's notifications
can press too. A notification waits 15 s (2 minutes while the session's
window has focus), so a prompt answered at the desk never sends one, and it
is deleted from the phone once the prompt is answered anywhere.

```nix
services.ccdash-hub.settings.notify = {
  url = "https://ntfy.example.org";
  topic = "ccdash";
  token_file = "/run/agenix/ccdash-ntfy-token"; # may publish to the topic
  # where buttons and taps point; default: the hub's public URL
  # web_url = "https://ccdash.example.org";
  # turns = false;  questions, plans, permissions: per-kind switches
  # workspaces = [ "work" ];  only these workspaces
};
```

A button calls `POST /act/<key>/<action>` on the hub with a random key
that belongs to that one prompt and dies with it; the access token never
goes into a notification. The phone needs to reach the hub (LAN or VPN)
for the buttons to work. Notifications carry session titles, commands
and questions, so use an ntfy server you run, with access control. Keeping
it on the same LAN or VPN costs nothing (the buttons need it anyway), and a
short in-memory cache (`cache-duration`, no `cache-file`) keeps the text off
its disk.

### Web Push

Served over HTTPS, the web UI installs as an app (the pairing dialog has
the install button, else the browser's "Add to Home screen"), works
offline for what it cached, and can take the same notifications as Web
Push: "Connect a phone" → "Turn on push notifications" asks the browser
for permission and hands the subscription to the hub; "Send a test" checks
the way back. The notification shows the prompt with as many buttons as
the platform allows (two on Android: Allow / Deny, or the first option and
"Answer", which opens the panel); tapping it opens the session. It closes
once the prompt is answered. Push is on by default; the hub generates its
VAPID key pair at `push.key_file` on first start (the NixOS module keeps
it in `/var/lib/ccdash-hub`), messages are encrypted for the device
(RFC 8291) and go through the browser vendor's push service, which sees
only ciphertext. Devices the service reports gone are forgotten.

```nix
services.ccdash-hub.settings.push = {
  # enabled = false;
  subject = "mailto:you@example.org"; # VAPID contact; default: public URL
  # ttl_secs = 3600;  how long the service holds an undelivered message
};
```

### Replying from the phone

With the channel enabled, the session sheet gets a message box. A message
sent while Claude works waits for its next turn. Messages reach Claude as
`<channel source="plugin:ccdash:ccdash">` events (the terminal shows
`← ccdash: …`), and ccdash marks them "sent from ccdash".

This uses Claude Code
[channels](https://code.claude.com/docs/en/channels-reference), a research
preview: `ccdash channel` is an MCP server that every session started with
the channel flags runs, connected to the collector. Two pieces turn it on:

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

CPU and memory come from `/proc`: every 5 s the collector sums the CPU
time of each session's process tree (Claude, its MCP servers and running
commands) and its memory as PSS, so shared pages are not counted twice.
Turn it off with `procs.enable = false`.

The HTTP API needs the bearer token for everything (it can read sessions,
rename them, answer their prompts and, with the channel, prompt them),
except the notification buttons' one-time links and `/collect`, which
takes a collector's token. Keep the hub behind your firewall or VPN.

## Knowledge: memories, issues, progress, documents

The hub also holds what Claude should know across sessions and machines:
memories (facts worth keeping), issues (open work), progress (dated
entries) and documents (plans, specs), as notes with `[[slug]]` links,
backlinks and full-text search, each in one workspace. Sessions reach
them through tools the channel server exposes (`recall`, `remember`,
`verify`, `issue_add`, `issue_update`, `progress`, `doc_write`,
`tell_session`), and the hooks inject what matters: open issues and
relevant memories at session start, other sessions active in the same
project at every prompt, and a warning before editing a file another
session edited recently. The **Projects** page of the web UI shows every
project's notes, searches them and edits them.

Every read is scoped to the calling session's workspace in the hub; a
session never sees another workspace's notes. A tool call is one round
trip session → collector → hub with a 5 s budget.

While the hub is unreachable, writes are queued in the collector's outbox
(the tool says so) and replayed in order when the link returns; reads come
from a cache of the live notes the hub pushes to every collector on
connect and on every change, kept in `~/.local/state/ccdash/notes-cache.json`.
`doc_write` carries the version the edit was based on and the hub refuses
a stale one, so shared documents never lose an edit silently. A session
that started during an outage gets a one-off catch-up at its first prompt
after the link returns.

A daily pass marks memories stale when they were not verified for 90 days
or not recalled for 60 (`knowledge.stale_unverified_days`,
`knowledge.stale_unrecalled_days`; `knowledge.reflect = false` turns it
off). Stale notes stay searchable but are no longer injected; the Projects
page lists them so you can verify, rewrite or archive them. A note that
`supersedes` another retires it.

```sh
# one note per memory file (safe to rerun)
ccdash import memory ~/code/work/.claude/memory
# the doc, its checkboxes as issues, its log as progress
ccdash import plan ~/code/work/meta/plans/x.md --project x
# every note as markdown with frontmatter
ccdash export --config /etc/ccdash/hub.toml ./notes
```

`import` runs on a workstation and sends through its collector; `export`
reads the database (or a backup copy with `--db`) directly. Collector
settings `inject.session_start`, `inject.prompt` and `inject.edits` turn
the three injections off individually.

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

Everything is stored on the hub:

| What       | Where                                                        |
| ---------- | ------------------------------------------------------------ |
| Live state | hub memory, one reducer per host                             |
| History    | `/var/lib/ccdash-hub/ccdash.db` (SQLite, WAL), `ccdash` user |
| Backups    | `/var/lib/ccdash-hub/backups/`, nightly, checked             |
| Config     | `/etc/ccdash/hub.toml`                                       |

On each workstation the collector keeps only what it cannot deliver:

| What      | Where                                                       |
| --------- | ----------------------------------------------------------- |
| Mirror    | collector memory (this host's sessions for DMS and the CLI) |
| Outbox    | `~/.local/state/ccdash/outbox.ndjson`, bounded              |
| Config    | `~/.config/ccdash/config.toml`                              |
| Sockets   | `$XDG_RUNTIME_DIR/ccdash/` (`ingest.sock`, `api.sock`)      |
| Telemetry | `127.0.0.1:7839` (OTLP/HTTP JSON), when enabled             |

Every row carries the host it came from. The database keeps 30 days
(`store.retention_days`) of:

- `events`: prompts (up to 8 KiB), Claude's messages and replies (up to
  32 KiB), tool calls (name, file path or first command line, duration),
  permission requests and how ccdash answered them, task changes, subagent
  results, compactions and state changes
- `samples`: once a minute per session from the statusline (context,
  cost, rate limits, model, lines changed)
- `proc_samples`: once a minute per session (CPU, memory, process count)
- `sessions`: id, directory, title, kind, start and end
- `notes`, `links`, `notes_fts`: the knowledge layer (not pruned)
- `limit_samples`: per-model weekly limits from each `/usage` poll
- `telemetry` (with telemetry on): API requests (model, tokens, cost,
  latency), API errors, tool approval decisions, commits, pull requests,
  edit decisions and active time

The HTTP API serves it to your browser with the token. Nothing else leaves
the hub, except notifications when `notify.url` is set or a device
subscribed to push (encrypted; the push service sees only ciphertext). Delete the
database to start over. `ccdash import db` brings the history of a
pre-hub daemon (`~/.local/state/ccdash/ccdash.db` on the workstation)
into the hub; it skips rows the hub already has, so it is safe to repeat.

The collector ↔ hub protocol (`src/protocol.rs`) is versioned: a hub and a
collector of different versions refuse each other and log why.

## Development

```sh
direnv allow   # or: nix develop
cargo run -- hub --config hub.toml             # http.listen, collect.tokens, store.path
cargo run -- collector --config collector.toml # hub.url = "ws://127.0.0.1:7837/collect"
nix build
```

`CCDASH_RUNTIME_DIR` moves a collector's sockets, so a second collector
(or the hooks of a test session) can run next to the real one.

`CCDASH_WEB_DIR=web` serves the UI from disk instead of the embedded copy,
so edits show up on reload.

## License

[EUPL-1.2](LICENSE)
