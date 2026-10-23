# Sessions from the phone

1. [Start](#1-start)
2. [Continue](#2-continue)
3. [Stop](#3-stop)
4. [Open on the desktop, forget](#4-open-on-the-desktop-forget)
5. [Background sessions](#5-background-sessions)
6. [Configuration](#6-configuration)

## 1. Start

**Start a session** on Now (or **Start** on a project page) asks for a
connected host, a workspace and one of its projects, the model, the
permission mode (the host's default, plan, accept edits) and the prompt.
The panel waits until the session reports and opens its page; its prompts
reach the phone like any other's, and a finished turn notifies "Finished".

The hub resolves the directory from the workspace and project: the panel
never sends a path, and the hub refuses unknown hosts, projects, models,
modes and fields. Only projects with a directory on the chosen host are
offered. The collector then runs, in that directory:

```sh
<launch.command> --bg [--resume <id>] [--model <m>] \
  [--permission-mode <pm>] --channels plugin:ccdash@inline \
  --settings '{"env":{"CCDASH_LAUNCH":"<launch id>"}}' -- <prompt>
```

`claude --bg` prints the session's id, and the session's hooks carry the
launch id (a settings `env`: background sessions run in Claude Code's
background daemon, which never sees the collector's environment), so the
hub finds the session either way. `--settings` replaces an earlier one, so
`launch.command` must not carry its own. Nothing else is passed: never
`--dangerously-skip-permissions`.

## 2. Continue

An ended session's page offers **Continue** (`claude --bg --resume`) on
any connected machine that [syncs](state-sync.md): the collector fetches
the transcript first. Claude Code may continue a stopped background session
as a copy under a new id; the page follows it.

## 3. Stop

**Stop** asks first and answers only once the session is over. Never
SIGKILL.

| Session     | What Stop does                                                                                                                                                   |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Interactive | SIGTERM to its Claude process (only one that is still Claude Code); after 5 s, SIGTERM to its child processes too; if it still runs, Stop says what it waits for |
| Background  | `claude stop <id>`; when that fails and no process runs the session, `claude rm <id>` (keeps the conversation)                                                   |

Why the escalation: Claude Code acts on SIGTERM only once an editor it
opened for the prompt (Ctrl+G), or a running command, has finished.

## 4. Open on the desktop, forget

- **Open on** (the host's name) focuses the session's terminal window, or
  attaches a background session in a new terminal (`attach.command`), as
  `ccdash open` does.
- **Forget**, when the session's host is offline, marks it ended on the
  hub; it comes back if its host reports it running again.

## 5. Background sessions

Claude Code keeps listing a background session whose turn ended and that
waits for your answer (`blocked`), often with no process left, sometimes
for weeks. ccdash shows it as your turn right after its turn ends and as
parked otherwise; finished ones (`done`, `stopped`) end.

## 6. Configuration

The Home Manager module fills these in with the Claude Code binary and the
ccdash plugin, so the channel and the knowledge tools load as in a
terminal.

```toml
# collector config
[launch]
command = ["claude"]                  # + the arguments above; [] turns starting off
stop = ["claude", "stop", "{id}"]
remove = ["claude", "rm", "{id}"]     # when stop fails on a job nothing runs

[attach]
command = ["ghostty", "-e", "claude", "attach", "{id}"]
```
