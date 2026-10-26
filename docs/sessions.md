# Sessions from the phone

What can be done to a session sits in its **Actions** menu, on every
session card and on its page. Now pins **Resolved** and **Kill** on the
sessions that need you, and the session page pins **Open on** and
**Resolved**. An action that cannot run shows disabled, with why.

1. [Start](#1-start)
2. [Continue](#2-continue)
3. [Resolve](#3-resolve)
4. [Kill](#4-kill)
5. [Open on the desktop, forget](#5-open-on-the-desktop-forget)
6. [Background sessions](#6-background-sessions)
7. [Configuration](#7-configuration)

## 1. Start

**Start a session** on Now (or **Start** on a project page) asks for a
connected host, a workspace and one of its projects, the model, the
permission mode (the host's default, plan, accept edits) and the prompt.
The panel waits until the session reports and opens its page; its prompts
reach the phone like any other's, and a finished turn notifies "Finished".

The session starts in the **workspace's directory**, whatever the project:
that is where the harness (settings, rules, skills, the root CLAUDE.md)
and direnv live. The project is the session's label (see
[knowledge](knowledge.md#2-tools)): the hub writes it onto the session
when it reports, its notes default to it, and the project's other
sessions are the ones it hears about. The panel never sends a path, and
the hub refuses unknown hosts, projects, models, modes and fields; the
project list is the workspace's projects that have notes and a directory
on the chosen host. The collector then runs, in the workspace's directory:

```sh
<launch.command> --bg [--resume <id>] [--model <m>] \
  [--permission-mode <pm>] --channels plugin:ccdash@inline \
  --settings $XDG_STATE_HOME/ccdash/launch/<launch id>.json -- <prompt>
```

`claude --bg` prints the session's id, and the session's hooks carry the
launch id (`CCDASH_LAUNCH` in the settings file's `env`: background
sessions run in Claude Code's background daemon, which never sees the
collector's environment), so the hub finds the session either way. With
`launch.direnv` set, the collector first runs it (`direnv export json`) in
the directory and puts the variables matching the `launch.env` globs into
the same `env`, so a launched session has what the workspace's `.envrc`
gives a terminal there; a direnv failure fails the launch. The file is
owner-only and pruned after a day. `--settings` replaces an earlier one,
so `launch.command` must not carry its own. Nothing else is passed: never
`--dangerously-skip-permissions`.

## 2. Continue

An ended session's page offers **Continue** (`claude --bg --resume`) on
any connected machine that [syncs](state-sync.md): the collector fetches
the transcript first. Claude Code may continue a stopped background session
as a copy under a new id; the page follows it.

## 3. Resolve

**Resolved** (it asks first) ends a session whose task is done, after one
last turn of cleanup. The hub sends this prompt through the channel:

```text
Task is resolved, user is closing the session.

Wrap up with the ccdash:wrap-up skill: record what this session learned and did
in ccdash (memories, issues, progress) and release its claim. Ask nothing and
start no new work: the session closes when this turn ends.
```

The [`wrap-up` skill](../claude-plugin/skills/wrap-up/SKILL.md) ships with
the ccdash plugin: Claude closes or comments the issues it worked on, logs
progress, records durable memories, releases its claim and ends with a
short summary, which stays on the session's page. It edits, commits and
pushes nothing.

When the turn that prompt starts ends, the hub closes the session as Kill
does; until then the card reads "Resolved". Resolving a session that is
working queues the prompt after its current turn.

| Happens before the close                | Then                                               |
| --------------------------------------- | -------------------------------------------------- |
| **Keep open** (Actions), or you type on | the mark goes; the session stays after its wrap-up |
| the wrap-up turn fails (an API error)   | the mark goes; the session waits on you            |
| closing fails (the process won't exit)  | your turn, with why                                |
| the hub restarts                        | the mark comes back from the stored events         |

Resolve needs the ccdash channel; without it the action shows disabled.

## 4. Kill

**Kill** asks first and answers only once the session is over. Never
SIGKILL.

| Session     | What Kill does                                                                                                                                                   |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Interactive | SIGTERM to its Claude process (only one that is still Claude Code); after 5 s, SIGTERM to its child processes too; if it still runs, Kill says what it waits for |
| Background  | `claude stop <id>`; when that fails and no process runs the session, `claude rm <id>` (keeps the conversation)                                                   |

Why the escalation: Claude Code acts on SIGTERM only once an editor it
opened for the prompt (Ctrl+G), or a running command, has finished.

## 5. Open on the desktop, forget

- **Open on** (the host's name) focuses the session's terminal window, or
  attaches a background session in a new terminal (`attach.command`), as
  `ccdash open` does.
- **Forget**, when the session's host is offline, marks it ended on the
  hub; it comes back if its host reports it running again.

## 6. Background sessions

Claude Code keeps listing a background session whose turn ended and that
waits for your answer (`blocked`), often with no process left, sometimes
for weeks. ccdash shows it as your turn right after its turn ends and as
parked otherwise; finished ones (`done`, `stopped`) end.

## 7. Configuration

The Home Manager module fills these in with the Claude Code binary and the
ccdash plugin, so the channel, the knowledge tools and the wrap-up skill
load as in a terminal.

```toml
# collector config
[launch]
command = ["claude"]                  # + the arguments above; [] turns starting off
stop = ["claude", "stop", "{id}"]
remove = ["claude", "rm", "{id}"]     # when stop fails on a job nothing runs

[attach]
command = ["ghostty", "-e", "claude", "attach", "{id}"]
```
