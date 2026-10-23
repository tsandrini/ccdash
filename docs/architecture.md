# Architecture

1. [Components](#1-components)
2. [What Claude Code interfaces are used](#2-what-claude-code-interfaces-are-used)
3. [The collector](#3-the-collector)
4. [The hub](#4-the-hub)
5. [Protocol](#5-protocol)

## 1. Components

```text
on each workstation                      on the hub (always on)
Claude Code hooks ──► ccdash emit ─┐
prompts ────────────► ccdash ask ──┤
statusline ─────────► ccdash tee ──┼──► ccdash collector ══wss══► ccdash hub
claude agents --json ──────────────┤      │ (forwards, acts,      │ (state, SQLite,
claude -p /usage ──────────────────┤      │  syncs ~/.claude)     │  notifications)
niri window events ────────────────┘      └──► DMS plugin         ├──► web panel
                                                                  └──► Web Push
```

| Part                        | Runs                                    | Does                                                                       |
| --------------------------- | --------------------------------------- | -------------------------------------------------------------------------- |
| `ccdash emit`, `ask`, `tee` | as Claude Code hooks and the statusline | hand each payload to the local collector; never fail Claude Code           |
| `ccdash collector`          | a user service per machine              | forwards to the hub, carries out what comes back, syncs `~/.claude`        |
| `ccdash hub`                | a system service, one per setup         | keeps live state and history, serves the panel and the API, sends Web Push |
| `ccdash channel`            | an MCP server per session               | delivers replies from the panel; exposes the knowledge tools               |
| DMS plugin                  | in DankMaterialShell                    | this machine's sessions in the bar and the dash                            |

## 2. What Claude Code interfaces are used

Public ones only: `claude agents --json`, `claude --bg`, `claude stop`,
`claude rm`, `claude attach`, hooks, the statusline payload,
[channels](https://code.claude.com/docs/en/channels-reference),
[OpenTelemetry](https://code.claude.com/docs/en/monitoring-usage) and the
`/usage` report.

The one exception is the [state sync](state-sync.md): it copies the files
Claude Code keeps under `~/.claude` byte for byte and reads only the
running-session registry (`sessions/<pid>.json`) and the prompt history's
lines; the transcript format is never parsed.

## 3. The collector

- Takes hook events over a Unix datagram socket and forwards them to the
  hub over one WebSocket (`/collect`, a token per machine). While the hub
  is unreachable, events wait in an on-disk queue and Claude Code runs as
  if ccdash were not there.
- Polls `claude agents --json` every 5 s for liveness and background
  sessions, and `claude -p /usage` every 10 minutes for the per-model
  weekly limits the statusline lacks (safe mode: no model request, no
  hooks, nothing saved; about a second of CPU).
- Reads each session's git branch from `.git/HEAD` (worktrees included)
  without running git, and its process tree's CPU and memory (PSS) from
  `/proc` every 5 s.
- Carries out what the hub sends: replies, prompt answers, renames,
  sessions to start, stop or open, sync work.
- Serves a local API (`$XDG_RUNTIME_DIR/ccdash/api.sock`) for the hooks, the
  CLI and the DMS plugin, with a mirror of this machine's sessions.

Claude Code reads hooks when a session starts: sessions that were running
before ccdash was installed only show busy and idle until they are resumed.

## 4. The hub

- One reducer per host turns events and polls into live session state; the
  history goes to SQLite ([data](data.md)).
- Serves the panel (`web/`, built into the `ccdash-web` package) and the
  HTTP API under `/api/v1`, with server-sent events for live updates.
- Sends Web Push for prompts and finished turns, with answer buttons.
- Keeps the synced `~/.claude` files and the knowledge notes.

A conversation in the panel is rebuilt from hook events, so Claude's
thinking is not part of it.

## 5. Protocol

Collector and hub speak JSON messages over the WebSocket
(`src/protocol.rs`). The protocol is versioned: a hub and a collector of
different versions refuse each other and log why, so a hub and its
collectors are upgraded together. The state sync uses plain HTTP routes
under `/sync/` with the collector's token.
