# Sessions on every machine (state sync)

With sync on, the hub keeps Claude Code's state for every machine and each
collector keeps its machine's copy in step: a session ended on one machine
resumes on another with a plain `claude --resume` (the picker lists it), and
the up arrow and Ctrl+R know the prompts of every machine.

1. [Turning it on](#1-turning-it-on)
2. [What travels](#2-what-travels)
3. [How files travel](#3-how-files-travel)
4. [One session, one machine](#4-one-session-one-machine)
5. [Conflicts](#5-conflicts)
6. [The State page](#6-the-state-page)
7. [On the hub](#7-on-the-hub)

## 1. Turning it on

```nix
services.ccdash.sync.enable = true;   # Home Manager, on every workstation
```

Claude Code files sessions by directory, so every machine needs the same
user and the same paths.

## 2. What travels

| Travels (relative to `~/.claude`)                               | Stays on each machine                                          |
| --------------------------------------------------------------- | -------------------------------------------------------------- |
| `projects/` (transcripts, subagents, tool results, auto memory) | credentials, `~/.claude.json`                                  |
| `file-history/` (rewind checkpoints)                            | settings, plugins, skills (Nix and your repos carry those)     |
| `plans/`, `tasks/`, `paste-cache/`                              | the running-session registry, background jobs, shell snapshots |
| `history.jsonl` (prompt history)                                | `projects/-tmp-*` (scratch sessions)                           |

## 3. How files travel

- **Changes** come from inotify (a watch per directory that can hold synced
  files); a burst of writes settles for a quarter second (two at most), then
  just those files are handled. Everything is scanned at start-up, after an
  inotify overflow, for Resync and Stop, and every half hour as a safety
  net. Without inotify the collector scans every `interval_secs`.
- **Byte for byte:** a file that only grew sends its new tail; a transcript
  line still being written waits. The hub refuses a write made against a
  version it no longer has, so nothing is overwritten unseen.
- **Deletions never travel:** Claude Code's clean-up (`cleanupPeriodDays`)
  on one machine removes nothing elsewhere, and the hub keeps everything.
- **Prompt history** merges line by line: lines from other machines are
  appended, the file is never rewritten.
- **Running sessions:** the files of a session running on a machine are
  never written there; they catch up when it ends.
- **Name lookups** of the hub are reused for 5 minutes, so a backfill does
  not flood the DNS server.

Measured on 6,430 files (2.4 GB) with a hub on the same machine: a
backfill takes about 15 s, a second machine compares everything in under
3 s, and an idle collector uses no measurable CPU.

## 4. One session, one machine

A resumed session goes on in the same transcript, so two machines with it
open would split it. The hub knows where each session is open:

- In a session another machine opened first and still has open, prompts
  are refused: the hook blocks them and says where, with a link. A resume
  shows a warning first.
- The hub refuses that machine's writes to the session's files (a blocked
  prompt still writes bookkeeping lines).
- Stop the session there (its page's **Stop**, or **Forget** when that
  machine is offline), then send the prompt again. If the other machine
  added to the session after it was opened here, the hook asks you to quit
  and resume it, to load the whole conversation.

## 5. Conflicts

A file changed on both sides between two syncs is a conflict. The hub's
version stays in place everywhere; the machine keeps its own as a copy
under `~/.claude/ccdash-conflicts/<time>/…`. A transcript copy still opens
with `claude --resume <path>`.

| Action                   | Effect                                                                                                         |
| ------------------------ | -------------------------------------------------------------------------------------------------------------- |
| Keep the current version | the machine deletes its copy                                                                                   |
| Use the copy             | the copy replaces the version on every machine; the replaced one becomes the copy, so the choice can be undone |
| Hide                     | the conflict leaves the list; the copy stays                                                                   |

Each conflict shows how the two versions compare: the bytes they share, the
lines each has after that, and when each was written. The collector reports
every copy it holds after each full scan, so a copy deleted by hand
resolves its conflict.

## 6. The State page

- **Machines:** each machine's copy (files, what waits, the first run's
  progress, whether it watches files) with **Resync**, which compares every
  file with the hub again, sends what the hub lacks and fetches what the
  machine lacks, deleted files included.
- **Conflicts** with the actions above.
- **Sizes:** by kind of file, by project and the largest sessions.
- **Growth:** per day and project, counted from the first write the hub
  records (a backfilled file counts on the day it was last written), and
  what grows fastest.

## 7. On the hub

The files are plain files under `sync.dir` (`/var/lib/ccdash-hub/claude`
with the NixOS module), outside the database and its nightly copies: back
the directory up with a file-level tool (restic encrypts). The `/sync/`
routes take the collectors' tokens and pieces of up to 8 MiB; the NixOS
module raises nginx's body limit there.

```toml
# collector config
[sync]
enable = true
# root = "~/.claude"              # default: $CLAUDE_CONFIG_DIR, else ~/.claude
# include = ["projects/*", "file-history/*", "plans/*", "tasks/*",
#            "paste-cache/*", "history.jsonl"]
# exclude = ["projects/-tmp-*"]
# interval_secs = 3               # full scans, only without inotify

# hub config
[sync]
# dir = "/var/lib/ccdash-hub/claude"  # default: claude/ next to the database
```
