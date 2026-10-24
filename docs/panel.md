# The panel

The web panel (`web/`, SvelteKit) is served by the hub at `/` and is built
phone first. Every view can be narrowed to workspaces and hosts: the scope
is kept per device (browser storage), across pages and reloads, until you
change it. The time range and the analytics section live in the URL, so
links can be shared and the back button works.

1. [Now and Sessions](#1-now-and-sessions)
2. [A session's page](#2-a-sessions-page)
3. [Projects](#3-projects)
4. [History and Analytics](#4-history-and-analytics)
5. [Desktop (DankMaterialShell)](#5-desktop-dankmaterialshell)

## 1. Now and Sessions

- **Now** lists what needs you, with the answer inline (permission prompts,
  questions, plans, sessions on your turn with a quick reply), then what is
  working, the hosts with their link state, the account limits and the last
  progress entries. **Start a session** is here ([sessions](sessions.md)).
- **Sessions** groups every session: those that need you, then working,
  parked and idle, then background ones (which show the session that
  started them and "Finished" instead of "Your turn").
- A session left on your turn for an hour is **parked**: most likely done,
  it no longer counts as needing you.

## 2. A session's page

| Tab     | Shows                                                                                      |
| ------- | ------------------------------------------------------------------------------------------ |
| Chat    | the exchanges with Claude's text, grouped tool calls, subagents, messages between sessions |
| Shell   | its Bash calls as a terminal transcript with their output                                  |
| Details | the turn, files, tasks, processes, the claim, totals                                       |

Open prompts are pinned above a composer that sends a turn through the
channel or leaves a note for the next prompt; it walks the prompt history,
inserts `[[note]]` links and keeps drafts. The title renames in place; the
page also offers **Stop**, **Continue**, **Open on** and **Forget**
([sessions](sessions.md)).

## 3. Projects

Each workspace's projects with their board (open issues), live sessions,
progress, documents, memories and the note graph, and a reader and editor
for every note ([knowledge](knowledge.md)).

## 4. History and Analytics

**History** draws the sessions of a time range as lanes coloured by state,
with the totals, time per project, the most used tools and the 5-hour
limit over time.

**Analytics** sections:

| Section     | Answers                                                                                                            |
| ----------- | ------------------------------------------------------------------------------------------------------------------ |
| Overview    | daily activity, when you prompt (weekday × hour), work per prompt, cost, lines changed, CPU and memory per project |
| Waits       | how long sessions waited on you and how fast you replied, apart from parked time                                   |
| Parallel    | how many sessions worked or waited at once; time everything waited with nothing working                            |
| Limits      | 5-hour, 7-day and per-model weekly limits, their pace, each workspace's share                                      |
| Projects    | time, prompts, cost, lines, tool calls, compactions and CPU per directory                                          |
| Turns       | what fills a turn, the slowest tools and commands, the commands that fail most                                     |
| Context     | compactions and how full sessions got                                                                              |
| Tokens      | tokens by type, cache hits, API latency, commits, edits by language (needs telemetry)                              |
| Permissions | what asks you most, grouped by the rule that would cover it, how you answered                                      |

A **wait** runs from Claude handing the turn back to what came next: a
reply within an hour counts as waiting on you; a later reply, closing the
session or leaving it open counts as parked; Claude carrying on by itself
counts as neither. Permission prompts always count.

The time range in the top bar: quick ranges or an exact span, `‹ ›` to
step, `⊖` to double it, and dragging across a time chart zooms in. Every
chart has a tooltip and a table view.

The Tokens section and the approval sources under Permissions need Claude
Code's OpenTelemetry export: `services.ccdash.claudeCode.telemetry.enable`
sets the `OTEL_*` variables and starts a receiver in the collector on
`127.0.0.1:7839`. It keeps counts and timings only; prompt text stays
redacted, as Claude Code sends it by default.

## 5. Desktop (DankMaterialShell)

`services.ccdash.dms.enable = true` links the [plugin](../dms-plugin/) into
DankMaterialShell: a bar pill with the sessions that need you, a dash tab
and card, notifications when a session starts waiting, and
`dms ipc call ccdash focusWaiting` to jump to the session that has waited
longest. Enable it once under Settings → Plugins and add the widget to a
bar. niri binds:

```kdl
Mod+C { spawn "dms" "ipc" "call" "dash" "toggle" "plugin_ccdash"; }
Mod+Shift+C { spawn "dms" "ipc" "call" "ccdash" "focusWaiting"; }
```

Clicking a session focuses its terminal window. The plugin opens the panel
without a token, so pair the desktop browser once ([security](security.md)).
