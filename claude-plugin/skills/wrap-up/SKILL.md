---
name: wrap-up
description: Wrap a finished session up before it closes — record what it learned and did in ccdash (issues, progress, memories), release its claim, end with a short summary. Use when the owner marks the session resolved in ccdash ("Task is resolved, user is closing the session") or asks to wrap up.
allowed-tools:
  - mcp__plugin_ccdash_ccdash__recall
  - mcp__plugin_ccdash_ccdash__remember
  - mcp__plugin_ccdash_ccdash__verify
  - mcp__plugin_ccdash_ccdash__issue_add
  - mcp__plugin_ccdash_ccdash__issue_update
  - mcp__plugin_ccdash_ccdash__progress
  - mcp__plugin_ccdash_ccdash__release
  - Bash(git status *)
  - Bash(git diff *)
  - Bash(git log *)
---

# Wrap up a session

The owner considers the task resolved. When ccdash sent the request, the
session closes as soon as this turn ends, so this turn is the last one:

- Ask nothing (no questions, no plans) and start no new work: no edits,
  builds, deploys, commits or pushes. What would need a permission prompt
  is left to the owner: name it in the summary.
- Work from what this session did, plus read-only checks such as
  `git status --short` and `git diff --stat`.
- Skip what is already recorded. A short session may need one line.
- When the ccdash tools report the hub offline, writes queue and land
  later; carry on.

## Steps, in order

- **Issues.** For each issue the session worked on (its claim, issues it
  filed, updated or logged progress on): `issue_update` to close it with
  a comment naming what landed when it is done, else to comment what is
  left. File real follow-ups with `issue_add`, never what is done.
- **Progress.** One `progress` entry when something landed or was
  decided: what, where (paths, commits), what comes next; link the issue.
- **Memories.** Durable, non-obvious learnings only: quirks, gotchas,
  decisions with their reasons, the owner's stated preferences; not what
  the code or the git history shows. `recall` first; update an existing
  note with `remember` on its slug (the whole body), else write a new one
  with a one-line summary and its project. `verify` the memories this
  session confirmed; correct the ones it disproved.
- **Claim.** `release` the session's claim.
- **Summary.** End with a few lines: what was recorded (slugs), and what
  is left for the owner: uncommitted changes (with the commands to commit
  them when the owner commits by hand), unpushed commits, candidates to
  promote into rules or skills. The reply stays readable on the
  session's page in ccdash.
