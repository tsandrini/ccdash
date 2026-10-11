# ccdash

A live dashboard for all your running Claude Code sessions: which ones are
working, which are waiting on you, and which look stuck. On the desktop as a
[DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) plugin,
on the phone as a web page.

> **Status:** work in progress. The collector daemon and the web UI work;
> the DMS plugin is not built yet.

## How it works

```text
Claude Code hooks ──► ccdash emit ─┐
statusline ─────────► ccdash tee ──┼──► ccdash daemon ──┬──► web UI (LAN / VPN)
claude agents --json ──────────────┤    (per host)      └──► DMS plugin
niri window events ────────────────┘
```

- Every session reports through Claude Code hooks to a small daemon on the
  same machine. The daemon keeps live state in memory and history in SQLite.
- The **web UI** is read-only and meant for the phone. Tapping a session opens
  it in the Claude app through Remote Control.
- The **DMS plugin** adds a bar indicator, a quick list, and a full dashboard
  tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`, hooks,
the statusline payload); no internal state files.

## Usage

With Home Manager (the module also wires the hooks into
`programs.claude-code`):

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    settings.http.listen = [ "[::]:7837" ];
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
ccdash status          # what every session is doing
ccdash open <id>       # focus its window, or attach to a background session
ccdash url --host <ip> # web address for the phone, token included
```

The web UI lives on the HTTP port:

- **Sessions:** the ones that need you first, then working and idle. Each
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

Open the address from `ccdash url` once; the token is kept in the browser
after that.

Claude Code reads hooks when a session starts, so sessions that were already
running when ccdash was installed only show busy/idle until you restart
them (`claude --resume` keeps the conversation). The conversation is built
from hook events; Claude's thinking is not part of them.

CPU and memory come from `/proc`: every 5 s ccdash sums the CPU time of each
session's process tree (Claude, its MCP servers and running commands) and
its memory as PSS, so shared pages are not counted twice. Turn it off with
`procs.enable = false`.

The HTTP API is read-only and needs the bearer token (generated in
`~/.local/state/ccdash/token` unless `http.token_file` is set). Keep the
port behind your firewall or VPN.

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
