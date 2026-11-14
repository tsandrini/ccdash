# Sessions from the phone

What can be done to a session sits in its **Actions** menu, on every
session card and on its page. Now pins **Resolved** and **Kill** on the
sessions that need you, and the session page pins **Open on** and
**Resolved**. An action that cannot run shows disabled, with why.

1. [Start](#1-start)
2. [Resume](#2-resume)
3. [Resolve](#3-resolve)
4. [Interrupt](#4-interrupt)
5. [Kill](#5-kill)
6. [Open on the desktop, forget](#6-open-on-the-desktop-forget)
7. [Background sessions](#7-background-sessions)
8. [Configuration](#8-configuration)

## 1. Start

**Start a session** on Now (or **Start** on a project page) asks for a
connected host, a workspace and one of its projects, the model, the
effort (the default, or low to max), the permission mode (the host's
default, plan, accept edits) and the prompt. The lists narrow as you type:
the model's has the aliases (`opus` follows the newest release) and the
versions by name, and takes any other name `claude --model` knows.
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
  [--permission-mode <pm>] [--effort <level>] --channels plugin:ccdash@inline \
  --settings $XDG_STATE_HOME/ccdash/launch/<launch id>.json -- <prompt>
```

`claude --bg` prints the session's id, and the session's hooks carry the
launch id (`CCDASH_LAUNCH` in the settings file's `env`: background
sessions run under Claude Code's supervisor, which never sees the
collector's environment), so the hub finds the session either way. With
`launch.direnv` set, the collector first runs it (`direnv export json`) in
the directory and puts the variables matching the `launch.env` globs into
the same `env`, so a launched session has what the workspace's `.envrc`
gives a terminal there; a direnv failure fails the launch. The file is
owner-only and kept as long as Claude Code lists the job, whose saved
options name it, and 30 days otherwise. `--settings` replaces an earlier
one, so `launch.command` must not carry its own. Nothing else is
passed: never `--dangerously-skip-permissions`.

A host has to trust the workspace first: `--bg` refuses a directory nobody
accepted the trust prompt for, and the failure shows in the start sheet.
Run `claude` once in the workspace's directory on a new machine.

## 2. Resume

Any past session resumes on any connected machine that
[syncs](state-sync.md): the collector fetches the transcript first. An
ended session's page has **Resume…** at its foot (and in its Actions menu);
every other one is found under **Sessions › Past**.

Resumed as it was, a background session of the same machine goes on under
its own id. Pick another model, effort or permission mode and Claude Code
continues it as a copy under a new id, on those; the old one stays.

A copy is the same conversation under another id, and the panel shows it
as one. The copy takes the title over, and its chat starts with the
earlier session's part: a line marks where it went on, with the model and
effort on either side when they changed (`Opus 5.5 → Fable 5.2`) and a
link to the earlier session. Its header and card say what it continues;
the earlier session's say what it was continued as, and its page offers
**Open the copy**. An ended session leaves the **Ended** list to its copy,
and Past marks it **continued**. Cost, context, the process and Kill stay
each session's own, and a session that was copied while it ran goes on by
itself: the copy's chat shows it up to the moment of the copy.

A running session takes no other model or effort from outside, only
`/model` and `/effort` typed into it. For a background session the panel
does the next best thing in one step: **Switch model & effort…** in its
Actions menu asks for the two and the next prompt, says what it is about
to do, and on **Stop and switch** the hub stops the session
(`claude stop`) and continues its conversation as a copy on what you
picked. A turn in progress is cut off. If the stop fails nothing changes;
if the copy fails to start, the session stays stopped and the same sheet
(or **Resume…** on its page) starts it. A session in a terminal is not
offered this: change it there.

### Finding it: Sessions › Past

**Past** (`/sessions/past`, also **Resume…** on Now and "Search past
sessions for …" in `Ctrl K`) lists every conversation the hub holds: the
transcripts of every machine that syncs, the ones from before ccdash too,
newest first and grouped by day.

- **Search** looks through what was said: your prompts, Claude's replies
  and the titles, by word prefix, diacritics ignored; `"…"` for a phrase.
  Sessions with every word come first; when none has them all, the ones
  with some (the line above the list says so). Each card shows where it
  matched, who said it and when.
- **Machines** (any number; all by default, the bar's host when one is
  picked): sessions any of them wrote to. **Project**, **When** (24 hours
  to a year) and **Sort** (best match, last active, started, most prompts,
  largest, most expensive) sit beside it, each with its counts. Filters and
  the open session are in the address, so back and links work.
- A **card** has the machine it last ran on (and how many others), the
  workspace and project, the branch, when it was last active, the title,
  what you asked first and where it ended (Claude Code's recap when it wrote
  one, else Claude's last words), the prompts, the time Claude worked, the
  transcript's size, the cost and the model; marks for **running**,
  **continued** (resumed later as a copy), **headless** (`claude -p`, the
  SDK) and **outside workspaces**.
- A session **opens** beside the list (its own view on a phone): every
  machine it ran on (online or not), the directory and branch, **Resume…**,
  its page when the hub has its chat, and the terminal command
  (`cd <dir> && claude --resume <id>`) to copy. **Overview** has the recap,
  what you asked first, where it ended (your last prompt and Claude's last
  reply), the matches of the search, every prompt (a click reads from
  there), the files it edited, dates, size, cost, models and the Claude
  Code version. **Conversation** has your prompts and Claude's text in
  order, tool calls left out, paged.
- Keys: `/` search, `↑ ↓` (or `j k`) move, `Enter` resume, `Esc` close.

The hub reads the transcripts into its catalog as the sync brings them; on
the first start that takes a minute or so, and the page says how many are
left. A session started from a machine without the state sync is not
there: its page (from Sessions) still resumes it on its own machine.

### What runs

**Resume…** opens the start sheet: pick the **machine** (the connected ones
with the session's workspace; the one it last ran on is marked, an offline
one shows why it cannot be picked), the model, the mode and the prompt.

| Where, how                                                           | What runs                               | Session                    |
| -------------------------------------------------------------------- | --------------------------------------- | -------------------------- |
| its job is on that machine, nothing changed                          | `claude --bg --resume <id> -- <prompt>` | the same one, same options |
| another machine, another model or mode, or its settings file is gone | the command of [Start](#1-start)        | a copy under a new id      |

Claude Code keeps a job's options itself: with any flag it starts a copy
instead, so a job of that machine runs with none, `launch.command`'s
included. Those options name the launch's settings file, and without it the
job fails to restart, so ccdash notes which job a file belongs to and takes
the copy when it cannot tell. The page follows a copy. A session that ran
outside every workspace, or runs now, cannot be resumed from the panel: the
first resumes in a terminal (the copied command), the second is open
already.

## 3. Resolve

**Resolved** (it asks first) ends a session whose task is done, after one
last turn of cleanup. The hub sends this prompt through the channel:

```text
Task is resolved, user is closing the session.

Wrap up with the ccdash:wrap-up skill: record what this session learned and did
in ccdash (memories, issues, progress) and release its claim. Ask nothing and
start no new work: the session closes when this turn ends.
```

The [`wrap-up` skill][wrap-up] ships with
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

Resolve needs the ccdash channel; without it the action shows disabled. A
[sleeping](#7-background-sessions) session has none: send it a message
first, or Kill it.

## 4. Interrupt

**Stop** in a working session's composer, or **Interrupt** in its Actions
menu, ends the current turn the way Esc does in the terminal: Claude stops,
the session is on your turn, and what you send next is what it reads. No
confirmation: nothing is lost but the rest of the turn.

How it lands, through the hooks only (Claude Code's documented way; a
SIGINT would exit the process):

| The session is                   | What happens                                                              |
| -------------------------------- | ------------------------------------------------------------------------- |
| waiting on a permission prompt   | the prompt is denied with Claude Code's stop; the turn ends at once       |
| about to call a tool             | the PreToolUse hook denies the call and ends the turn (`continue: false`) |
| running a Bash command           | the command is ended, as Esc does; the turn ends with it                  |
| running another tool             | it runs to its end; the PostToolUse hook then ends the turn               |
| writing text, no tool call ahead | the turn ends by itself; the interrupt is dropped with it                 |
| run by hooks older than ccdash   | the action shows disabled: interrupt it in its terminal                   |

A running command gets SIGTERM with its whole process group, and SIGKILL 3 s
later if it ignores that. A shell left running in the background by an earlier
call stays, as do MCP servers.

Until it lands the live block reads "Interrupting: Claude stops at its next
step"; then the chat shows **Interrupted from ccdash** where the turn ended.
The terminal shows "hook stopped continuation: Interrupted from ccdash: the
user stopped this turn from their dashboard", and Claude reads the same
line at its next turn, so it knows why it stopped. Claude Code fires no
Stop hook for an interrupted turn; the hub closes the turn on the hook's
report. A subagent's tool calls do not take the interrupt: the main thread's
next step does.

## 5. Kill

**Kill** asks first and answers only once the session is over. Never
SIGKILL.

| Session     | What Kill does                                                                                                                                                   |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Interactive | SIGTERM to its Claude process (only one that is still Claude Code); after 5 s, SIGTERM to its child processes too; if it still runs, Kill says what it waits for |
| Background  | `claude stop <id>`; when that fails and no process runs the session, `claude rm <id>` (keeps the conversation)                                                   |

Why the escalation: Claude Code acts on SIGTERM only once an editor it
opened for the prompt (Ctrl+G), or a running command, has finished.

A killed session stays ended: Claude Code goes on listing its job as
`done`, and a listed job only keeps an open session open. **Resume**
brings it back.

## 6. Open on the desktop, forget

- **Open on** (the host's name) focuses the session's terminal window, or
  attaches a background session in a new terminal (`attach.command`), as
  `ccdash open` does.
- **Forget**, when the session's host is offline, marks it ended on the
  hub; it comes back if its host reports it running again.

## 7. Background sessions

A session you started, from the panel or with `claude --bg` in a terminal,
is yours like any other: it shows in Working, Needs you and Parked, and a
finished turn notifies. **Background agents** are only the sessions Claude
started; they report to the session that started them, and end with their
job.

At each turn's end Claude Code labels the job from Claude's closing words
([`claude agents --json`](https://code.claude.com/docs/en/agent-view#read-session-state-from-a-script)).
Both labels mean a turn ended and the session waits for you, so ccdash
treats them alike:

| `state`             | Claude Code means                                  | ccdash shows           |
| ------------------- | -------------------------------------------------- | ---------------------- |
| `working`           | a turn is running                                  | Working                |
| `blocked`           | it needs something from you                        | your turn, then parked |
| `done`              | finished what you asked, ready for the next prompt | your turn, then parked |
| `failed`, `stopped` | it ended                                           | ended, with Resume     |

**Asleep.** After about an hour unattached (8 h with Remote Control, a
minute when memory runs low) Claude Code stops an idle session's process
and keeps its job. The session stays open, marked asleep, and the composer
stays: a message runs `claude --bg --resume <id>`, the session comes back on
its saved options, channel included, and reads the message as its prompt
(or as a copy, as with [Resume](#2-resume); the old row then ends). If
that fails, the session says why and your text stays on the page. An
asleep `done` session stops showing as open after 7 days; Resume still
brings it back. A job Claude Code no longer lists ends its session.

The supervisor that runs these sessions is its own user service (see
[Configuration](#8-configuration)), so a collector restart leaves them
alone, and so does restarting the service: the next supervisor adopts them.
A machine shutdown still stops them; they show as ended, and Resume
brings the conversation back.

## 8. Configuration

The Home Manager module fills these in with the Claude Code binary and the
ccdash plugin, so the channel, the knowledge tools and the skills load as
in a terminal. It also runs `claude daemon run` as the user service
`claude-daemon` (`services.ccdash.claudeCode.daemon.enable`): started on
demand from the panel, the supervisor would live in the collector's cgroup
and die with it. The service starts with `default.target`, so a machine
nobody logs in to needs `loginctl enable-linger`.

A session runs with the `PATH` of whatever dispatched it, so sessions from
the panel get the collector's, which keeps the user manager's.

Open (attach to a background session) and Edit in the editor need a
terminal; without one the panel leaves them out. The Home Manager module
writes both commands from `services.ccdash.terminal` (and `editor`).

```toml
# collector config
[launch]
command = ["claude"]                  # + the arguments above; [] turns starting off; a job
                                      # continued on this machine runs without its arguments
channel = false                       # true adds --channels (Home Manager: claudeCode.channel.enable)
stop = ["claude", "stop", "{id}"]
remove = ["claude", "rm", "{id}"]     # when stop fails on a job nothing runs

[attach]
command = []    # off; e.g. ["ghostty", "-e", "claude", "attach", "{id}"]

[edit]
command = []    # off; e.g. ["ghostty", "-e", "nvim", "{file}"], the panel's Edit in the editor
                # ([panel](panel.md#2-a-sessions-page))
```

[wrap-up]: https://github.com/tsandrini/ccdash/blob/main/claude-plugin/skills/wrap-up/SKILL.md
