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
                                                                  └──► Web Push
```

- Every session reports through Claude Code hooks to a small **collector**
  on the same machine, which forwards everything to the **hub** over one
  WebSocket (`/collect`, a token per machine) and carries out what comes
  back: replies into sessions, prompt answers, renames, sessions to start,
  stop or open. It also reads each session's git branch from `.git/HEAD`
  (worktrees included), without running git. While the hub is
  unreachable, hook events wait in an on-disk queue and Claude Code runs
  as if ccdash were not there.
- The hub keeps every machine's live state in memory and history in
  SQLite, and serves the **web UI**, meant for the phone: it reads,
  renames sessions, answers their permission prompts, questions and plans,
  (with the channel enabled) sends them messages, and starts, continues
  and stops sessions in known projects. "Open in Claude"
  opens a session in the Claude app through Remote Control. Every view can
  be narrowed to one host.
- **Notifications** reach the phone (and a watch mirroring it) as Web Push
  to the web UI installed as an app, with buttons that answer the prompt.
- The **DMS plugin** on each workstation shows that machine's sessions
  (mirrored from the hub): a bar indicator, a quick list, and a full
  dashboard tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`,
`claude --bg`, `claude stop`, `claude attach`, hooks, the statusline payload, [channels](https://code.claude.com/docs/en/channels-reference),
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
    tokenFile = "/run/agenix/ccdash-ui-token";            # the bootstrap token
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
ccdash url --hub --config /etc/ccdash/hub.toml --qr   # pairing link for one device
ccdash import db old.db --config /etc/ccdash/hub.toml # history of a pre-hub daemon
ccdash backup --config /etc/ccdash/hub.toml copy.db   # what the nightly timer runs
```

The web UI lives on the hub. The panel (`web/`, a SvelteKit app the hub
serves at `/`) is phone first: **Now** lists what needs you with the
answer inline (permission prompts, questions, plans, sessions on your turn
with a quick reply), then what is working, the hosts with their link state
and the account limits, and the last progress entries; **Sessions** groups
every session (the ones that need you first, then working, parked and
idle, then background agents, which show the session that started them
and "Finished" instead of "Your turn"; a session left on your turn for an
hour is _parked_: most likely done, it no longer counts as needing you); a
session's page has **Chat** (the exchanges with Claude's text, grouped
tool calls, subagents and the messages between sessions), **Shell** (its
Bash calls as a terminal transcript with their output) and **Details**
(the turn, files, tasks, processes, the claim, totals), the open prompts
pinned above a composer that sends a turn through the channel or leaves a
note for the next prompt, walks the prompt history, inserts `[[note]]`
links and keeps drafts; **Projects** shows each workspace's board, live
sessions, progress, documents, memories and the note graph, with a reader
and editor for every note; **Settings** holds the pairing code, push, the
theme, quick replies and templates.

**History** draws the sessions of a time range as lanes coloured by state
(working, needs permission, your turn, parked, idle), with the totals,
time per project, the most used tools and the 5-hour limit over time.
**Analytics** comes in sections:

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

History and Analytics share the time range in the topbar: quick ranges or
an exact from–to, `‹ ›` to step by the range's length, `⊖` to double it,
and drag across any time chart with a mouse to zoom in. Every chart has a
tooltip and a table view under it. With `workspaces.dirs` set, a
workspace filter next to it narrows every view to one or several
workspaces; with more than one machine reporting, a host filter joins it,
cards name their machine and History and every analytics section carry a
By host table. The scope, the range and the analytics section live in the
URL, so links are shareable and the back button works.

To connect a phone, scan `ccdash url --hub --qr` on the hub (it reads the
hub's database, so run it as root or the `ccdash` user) or open "Connect a
phone" in the web UI on a paired browser. Every link carries a token minted
for that one device: random, kept hashed by the hub, good for an hour until
the link is opened, then for as long as the device keeps it. Settings ›
Devices lists them with their browser and last use; rename one in place,
or revoke it to sign the device out at its next request and end its push
notifications (revoking the device you are on asks first). The token in
`tokenFile` is the bootstrap one: the hub accepts it alongside the devices'
own, which is how a browser paired before this and the panel on the hub
keep working. A device on it sees "shared token" in Settings and can take
a token of its own there. Rotating `tokenFile` (and restarting the hub)
signs out only the devices still on it, nothing else. The DMS plugin opens
the hub's address without a token, so pair the desktop browser once.

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

Served over HTTPS, the web UI installs as an app (Settings has the install
button, else the browser's "Add to Home screen"), works offline for what it
cached, and can take the same notifications as Web Push: Settings → "Turn
on push notifications" asks the browser for permission and hands the
subscription to the hub; "Send a test" checks the way back. The
notification shows the prompt with as many buttons as
the platform allows (two on Android: Allow / Deny, or the first option and
"Answer", which opens the panel); tapping it opens the session, and a watch
mirroring the phone's notifications can press the buttons too. A
notification waits 15 s (2 minutes while the session's window has focus),
so a prompt answered at the desk never sends one, and it closes once the
prompt is answered anywhere. Push is on by default; the hub generates its
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
services.ccdash-hub.settings.notify = {
  # turns = false;  questions, plans, permissions: per-kind switches
  # workspaces = [ "work" ];  only these workspaces
  # web_url = "https://ccdash.example.org";  where taps point; default: public URL
};
```

A button calls `POST /act/<key>/<action>` on the hub with a random key
that belongs to that one prompt and dies with it; the access token never
goes into a notification. The phone needs to reach the hub (LAN or VPN)
for the buttons to work.

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

### Starting sessions from the phone

**Start a session** on Now (and **Start** on a project page, with the
project filled in) opens a sheet: a connected host, a workspace and one of
its projects (the ones Projects lists that have a directory on that host,
as its collector reports them, or the workspace's own directory), the
model, the permission mode (the host's default, plan, accept edits)
and the prompt, with templates, history and `[[note]]` links. The hub
resolves the directory from the workspace and project; the panel never
sends a path, and the hub refuses unknown hosts, projects, models, modes
and fields. The sheet waits until the session reports and opens its page.
Its questions, approvals and plans reach the phone like any other's; when
it finishes a turn, a "Finished" notification follows (`notify.turns`).

The collector runs, in the project's directory:

```sh
<launch.command> --bg [--resume <id>] [--model <m>] \
  [--permission-mode <pm>] --channels plugin:ccdash@inline \
  --settings '{"env":{"CCDASH_LAUNCH":"<launch id>"}}' -- <prompt>
```

`claude --bg` prints the session's id, and the session's hooks carry the
launch id (a settings `env`: background sessions run in Claude Code's
background daemon, so the collector's own environment never reaches
them), so the hub finds the session either way. `--settings` replaces an
earlier one, so `launch.command` must not carry its own. Nothing else is
passed: no `--dangerously-skip-permissions`, ever. The Home Manager module
sets `launch.command` to the Claude Code binary with the ccdash plugin
(`--plugin-dir`), so the channel and the knowledge tools load as in a
terminal; `launch.command = []` turns starting sessions off on that
machine.

An ended session's page offers **Continue** (`claude --bg --resume`; Claude
Code may continue a stopped background session as a copy under a new id,
and the page follows it). **Stop**, on the session page and the cards'
menu, asks first, then runs `launch.stop` (`claude stop <id>`) for a
background session or sends SIGTERM to an interactive session's Claude
process (never SIGKILL, and only to a process that is still Claude Code).
**Open on** (the host's name) focuses the session's window or attaches a background
session there (`attach.command`, as `ccdash open` does). **Forget**, when
the session's host is offline, marks it ended on the hub; it comes back if
its host reports it running again.

```toml
# collector config (the Home Manager module fills these in)
[launch]
command = ["claude"]                # + the arguments above; [] is off
stop = ["claude", "stop", "{id}"]
```

Anyone holding a token can answer your sessions' prompts, start sessions
in your projects and, with the channel, prompt them, so treat it like an
SSH key: keep the port on your LAN
or VPN, and revoke the device in Settings if it goes missing (rotate
`tokenFile` if it was on the bootstrap token).

Claude Code reads hooks when a session starts, so sessions that were already
running when ccdash was installed only show busy/idle until you restart
them (`claude --resume` keeps the conversation). The conversation is built
from hook events; Claude's thinking is not part of them.

CPU and memory come from `/proc`: every 5 s the collector sums the CPU
time of each session's process tree (Claude, its MCP servers and running
commands) and its memory as PSS, so shared pages are not counted twice.
Turn it off with `procs.enable = false`.

The HTTP API needs a bearer token for everything (it can read sessions,
rename them, answer their prompts, start and stop sessions and, with the
channel, prompt them): the
bootstrap one from `tokenFile`, compared in constant time, or a device's
own, looked up by its SHA-256. Only the notification buttons' one-time
links and `/collect`, which takes a collector's token, differ. Keep the hub
behind your firewall or VPN.

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
- `ui_tokens`, `push_subscriptions`: the devices' tokens (SHA-256, label,
  browser, last use) and their push subscriptions (not pruned)
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

The panel is `web/` (SvelteKit, Svelte 5, Tailwind, shadcn-svelte,
LayerChart; Node and npm are in the devshell):

```sh
cd web && npm ci
npm run dev    # Vite on :5173; /api and /act go to CCDASH_HUB (127.0.0.1:7837)
npm run check && npm run lint
npm run build                # → web/build, what the hub serves
```

The hub serves `http.web_dir` (the Nix build bakes in the `ccdash-web`
package; a `cargo run` hub falls back to `web/build`; `CCDASH_WEB_DIR`
overrides both) with `index.html` for every route. The API's TypeScript
types (`web/src/lib/api/types.ts`) are
generated from the Rust models: `cargo test api` fails while the file is
stale, `CCDASH_UPDATE_TYPES=1 cargo test api` rewrites it. After a change to
`web/package-lock.json`, put the hash that
`prefetch-npm-deps web/package-lock.json` prints into
`flake-parts/pkgs/ccdash-web.nix`. The service worker only
registers in the built app, so push and the offline shell are checked
against `web/build` served by a hub.

## License

[EUPL-1.2](LICENSE)
