# Data

1. [Where](#1-where)
2. [What the database keeps](#2-what-the-database-keeps)
3. [Backups and imports](#3-backups-and-imports)

## 1. Where

On the hub:

| What       | Where (NixOS module)                                         |
| ---------- | ------------------------------------------------------------ |
| Live state | hub memory, one reducer per host                             |
| History    | `/var/lib/ccdash-hub/ccdash.db` (SQLite, WAL), `ccdash` user |
| Backups    | `/var/lib/ccdash-hub/backups/`, nightly, checked             |
| State sync | `/var/lib/ccdash-hub/claude/`, plain files (not in backups)  |
| Config     | `/etc/ccdash/hub.toml`                                       |

On each workstation the collector keeps only what it cannot deliver yet:

| What      | Where                                                           |
| --------- | --------------------------------------------------------------- |
| Mirror    | collector memory (this machine's sessions, for DMS and the CLI) |
| Outbox    | `~/.local/state/ccdash/outbox.ndjson`, bounded                  |
| Notes     | `~/.local/state/ccdash/notes-cache.json`                        |
| Sync      | `~/.local/state/ccdash/sync.db` (what is in step)               |
| Config    | `~/.config/ccdash/config.toml`                                  |
| Sockets   | `$XDG_RUNTIME_DIR/ccdash/` (`ingest.sock`, `api.sock`)          |
| Telemetry | `127.0.0.1:7839` (OTLP/HTTP JSON), when enabled                 |

## 2. What the database keeps

Every row carries the host it came from. Kept for `store.retention_days`
(30 by default):

| Table              | Holds                                                                                                                                                                                                                                                  |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `events`           | prompts (up to 8 KiB), task notifications and Claude's messages and replies (up to 32 KiB), tool calls (name, path or first command line, duration, output up to 8 KiB), permission requests and answers, tasks, subagents, compactions, state changes |
| `samples`          | once a minute per session from the statusline: context, cost, limits, model, lines                                                                                                                                                                     |
| `proc_samples`     | once a minute per session: CPU, memory, process count                                                                                                                                                                                                  |
| `sessions`         | id, directory, title, kind, start and end, branch                                                                                                                                                                                                      |
| `limit_samples`    | per-model weekly limits from each `/usage` poll                                                                                                                                                                                                        |
| `telemetry`        | with telemetry on: API requests (tokens, cost, latency), errors, approvals, commits, edits, active time                                                                                                                                                |
| `session_messages` | messages between sessions and from you                                                                                                                                                                                                                 |

Claims end with their session or their deadline; revoked device tokens go
after a while. Not pruned: `notes`, `links`, `notes_fts` (knowledge), live
device tokens and their push subscriptions, `sync_files`,
`sync_history`, `sync_conflicts`, `sync_growth` (state sync).

## 3. Backups and imports

```sh
ccdash backup --config /etc/ccdash/hub.toml copy.db   # what the nightly timer runs
ccdash import db old.db --config /etc/ccdash/hub.toml # history of a pre-hub daemon, safe to repeat
```

The nightly copy goes through SQLite's backup API and is checked with
`PRAGMA integrity_check`; `backup.keep` copies are kept. Delete the
database to start over.
