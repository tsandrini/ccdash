# The workspace on every machine (workspace sync)

With the workspace sync on, the workspace root (`~/ProjectBundle`: every git
repository and plain directory under it) behaves like one directory that
happens to be visible on every machine. A file saved on one machine is on
the others within a second; a branch switch, a staged file and a stash
follow; two sessions on two machines editing one repository collide only
where two sessions on one machine would (the same hunk), and that collision
is an ordinary git conflict marker plus an entry in the panel. Nothing to
run by hand.

1. [Turning it on](#1-turning-it-on)
2. [Units](#2-units)
3. [How a change travels](#3-how-a-change-travels)
4. [What stays home](#4-what-stays-home)
5. [Conflicts, held snapshots, diverged branches](#5-conflicts-held-snapshots-diverged-branches)
6. [A new machine, an existing copy](#6-a-new-machine-an-existing-copy)
7. [The Workspace page](#7-the-workspace-page)
8. [On the hub](#8-on-the-hub)
9. [Costs](#9-costs)

## 1. Turning it on

```nix
services.ccdash-hub.workspace.enable = true;   # NixOS, the hub
services.ccdash.workspace = {                  # Home Manager, every workstation
  enable = true;
  root = "~/ProjectBundle";
  include = [ "work/*" "me/*" ];               # globs on unit ids, empty = everything
  exclude = [ "me/nixpkgs" "*_bkp" ];
};
```

The Home Manager options are the starting configuration: the first
collector to reach a hub without one hands it over, and from then on the
hub owns the configuration and the panel edits it (Workspace ›
Configuration). Every machine needs the same user and the same root path.

## 2. Units

A unit is a directory under the root that travels as one git-shaped thing:

| Unit                                           | Example                                       | How                                                                                              |
| ---------------------------------------------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| a repository at unit depth (`<bundle>/<repo>`) | `tsandrini/ccdash`                            | its own `.git`                                                                                   |
| a bundle root that is a repository             | `tsandrini` (with `.claude/`, `meta/`)        | the same; the directories that are units of their own are left out of its tree by pathspec       |
| a repository nested deeper                     | `meteopress/livance/nested`                   | its own unit; the parent leaves the directory alone (an existing gitlink stays as it is)         |
| a plain directory at unit depth                | `meteopress/tmp` (in a bundle without `.git`) | a hidden repository under `$XDG_STATE_HOME/ccdash/workspace/<unit>.git` with `core.worktree` set |

A plain directory inside a bundle-root repository is part of that
repository's unit, not a unit of its own. Dot-directories count. Unit ids
are paths relative to the root, `/`-separated; `include` and `exclude` are
globs on them (`*` spans `/`), `exclude` wins, a per-unit override in the
panel wins over both.

A directory that appears at unit depth and is not configured is **new**:
the collector reports it with its size (working tree plus `.git`, the
never-travelling directories left out); the hub includes it by itself when
it is under the threshold (500 MB, per bundle in the configuration) and
otherwise lists it on the Workspace page for a tap (Include or Ignore). A
unit whose directory disappears on one host shows as missing there; the
directory is never deleted elsewhere.

## 3. How a change travels

One inotify instance watches every directory of every unit that is not
ignored (79,592 on a 1.3 M-file workspace, registered in 0.11 s), each
unit's `.git` and its refs, and the root and bundle directories for units
appearing. A new directory is checked with `git check-ignore` before it is
watched, so an `npm install` costs two events. A burst settles for 250 ms
(2 s at most under a steady stream); then the unit's pass runs on one of
four worker threads (a unit is in one pass at a time):

- **Snapshot.** The real index is copied to a temporary one; `git add -A`
  into it (plus `git add -f` for the opted-in ignored paths, with the
  directories of nested units and untracked files over `max_file_mb`
  excluded by pathspec) gives the **work tree**; `write-tree` on another
  copy of the real index gives the **index tree** (the staged state). A
  commit whose tree holds both (`work/`, `index/`) is made on this host's
  chain (`refs/sync/<host>`, the first one off HEAD) with a message
  carrying HEAD, the branch, every branch and tag, the stashes and the
  empty directories. Nothing changed → no commit. The real index is never
  written by a snapshot.
- **Push.** `refs/sync/<host>` goes to the hub's bare repository of the
  unit over git smart HTTP with the collector token, together with the
  branch tips, tags and stashes under `refs/sync-tips/<host>/…` so their
  objects travel. The hub tells the other collectors (`WorkspaceChanged`).
- **Apply** on the other machines, after their own fresh snapshot `L`:
  fetch `refs/sync/<other>` as `S`, `merge-base L S`. `S` already in → nothing.
  `L` is the base → **fast-forward**: `read-tree -u -m` moves the working
  tree to `S`'s, the index becomes `S`'s staged state, HEAD and the
  branches follow, `refs/sync/<me>` becomes `S`. Otherwise **merge**:
  `merge-tree --write-tree` of the two snapshot trees on the base (the
  staged states merge in the same call; a path staged differently on both
  sides ends unstaged), the result is applied the same way and a two-parent
  commit becomes `refs/sync/<me>`, pushed so the other side fast-forwards.
  `read-tree -u -m` refuses when a file changed underneath; the pass ends
  and the next event retries. Files are written through git's atomic
  rename; the executable bit and symlinks travel, mtimes become "now",
  empty directories are recreated.
- **HEAD, branches, stashes.** HEAD and the checked-out branch follow the
  snapshot (rewinds included); every other branch and tag is set to the
  snapshot's value; a branch moved on both sides since the base stays as
  it is here and the other tip is kept under
  `refs/sync/diverged/<host>/<branch>`. Stashes from the snapshot are
  recreated when missing (`git stash store`).

Every git call the collector makes runs with `core.excludesFile` set to
the never-travelling list, `core.hooksPath` pointing at an empty directory
(a `pre-push` hook would otherwise run on every sync push), `gc.auto` off,
no credential helpers, no prompts, stdin closed, a timeout, and the hub
token in `GIT_CONFIG_*` environment variables, never on the command line.
Nothing is written into a repository's configuration.

Left alone until done: units with `index.lock`, a merge, rebase, cherry-pick
or bisect in progress; applies while a live Claude Code session's tool call
is in flight on this host (PreToolUse without PostToolUse, stale after 10
minutes). Safety nets: a sweep of every unit at start, after an inotify
overflow and every 30 minutes; every unit's remote refs fetched every 5
minutes. Hub down: nothing moves, work continues, the next connection
merges.

## 4. What stays home

- **Never, whatever the configuration** (`always_exclude`, by directory
  name anywhere): `node_modules`, `.venv`, `venv`, `.direnv`, `.devenv`,
  `target`, `result`, `result-*`, `__pycache__`, `.cache`. Store symlinks
  point into another machine's `/nix/store`; build trees are rewritten by
  every build.
- **Opted in** (`sync_ignored`, per configuration, bundle or unit; the
  Workspace page's unit view lists the ignored directories present with
  their size and a switch each): gitignored files and directories that
  travel anyway, by default `.env`, `.env.*`, `.envrc.local`. A
  `node_modules` can be carried this way (measured: 38k files hashed in
  2 s the first time, 81 MB on the hub, 0.7 s per quiet pass afterwards).
- **Quiescent** (the default for anything under `.devenv/state`, a flag on
  any `sync_ignored` entry): snapshotted only while no process on this host
  holds a file under it open (`/proc/*/fd`), and written only under the
  same condition; otherwise the unit shows "waiting: postgres holds … open"
  and the directory keeps its content while everything else in the unit
  moves on. The snapshot still carries the other machine's version, and
  the directory catches up within 15 seconds of the process letting go.
  A dev database travels when its server is down, never under a live one.
- Untracked files above `max_file_mb` (64) are skipped and listed.
- `refs/sync/*` and `refs/sync-tips/*` are visible in `git log --all` and
  `for-each-ref`, like remote-tracking refs.

## 5. Conflicts, held snapshots, diverged branches

- **Text conflict**: the file gets standard markers (`<<<<<<<
refs/sync/<me>` … `>>>>>>> refs/sync/<other>`), every other file merges
  clean, and the hub records the conflict with both blobs. It is open until
  the markers are gone (the next snapshot notices) or the panel's **Keep
  mine** / **Take theirs** writes one side. **Dismiss** hides the entry.
  A unit (or the whole configuration) can choose `set_aside` instead:
  the local version stays and the other side is only on the hub.
- **Binary conflict**: the local version stays; the other blob is on the
  hub, shown and chosen in the panel.
- **Held snapshot**: a snapshot that would add or delete more than 2,000
  files, or a fifth of a unit with at least 100 tracked files, in one
  settle does not travel. The unit shows held with the numbers, a push
  notification asks, and the panel's **Apply** pushes it as it is while
  **Drop** puts the working tree back to the previous snapshot. The guard
  against a generator writing into the tree and against an accidental
  `rm -rf`.
- **Diverged branch**: `main` committed on two hosts within one window. The
  local branch stays, the other tip is `refs/sync/diverged/<host>/main`,
  the unit view shows both for you to merge or rebase by hand; the working
  trees still merge.

## 6. A new machine, an existing copy

- **Init** (a machine without the workspace): once the collector connects,
  every unit the hub has and this machine lacks is created (an empty
  repository fast-forwarded to the newest snapshot, branches, HEAD and
  staged state included), in parallel, under the same resource limits.
  `ccdash workspace init` does the same now and `ccdash workspace status`
  follows it. An 800 MB history takes 20 s on the LAN and 80 s over the
  tunnel.
- **Adopt** (a machine with its own copy of the workspace): a unit that
  exists here and already has snapshots on the hub from another machine is
  not touched until you say so — it shows as "to adopt" with the hint; so
  does a copy whose first snapshot never reached the hub.
  `ccdash workspace adopt [unit]` (or **Adopt here** on the unit view)
  takes the first snapshot and merges it with the hub's latest: a
  repository merges on the commit both copies share (edits since then from
  both sides, a deletion stays deleted, the same lines changed on both
  sides end as markers), a plain directory has no common base and merges
  as a union. When one copy is simply older than the other, merge nothing:
  `--prefer hub` makes this copy exactly the hub's content (its own branch
  tips are kept under `refs/sync/diverged/<this machine>/…` when they are
  not behind), `--prefer mine` makes the hub's content exactly this copy's
  and the other machines fast-forward to it. Measured on a 219-unit copy:
  2.5–4 minutes, 33–42 real merges, one genuine conflict.
- A unit that exists only here with nothing on the hub is seeded without a
  question.

```sh
ccdash workspace status [unit] [--json]   # every unit's state on this machine
ccdash workspace resync [unit]            # fetch and snapshot again
ccdash workspace init                     # create what the hub has and this machine lacks
ccdash workspace adopt [unit] [--prefer mine|hub]   # copies that exist on both sides: merge, or one side wins
```

## 7. The Workspace page

- **Machines**: each collector (units, in step, moving, a seed or adoption
  as progress, errors) with **Resync**.
- **Units**: the grid of units × machines, grouped by bundle, a chip per
  cell (in step, sending, receiving, conflict, held, waiting, failing, to
  adopt, creating, missing, paused), filters (changed today, needs you,
  conflicts, held, paused) and a search. A unit opens its page: the state
  on every machine with the snapshot id and the last pass, diverged
  branches, its conflicts, the ignored directories present with a switch
  each, overrides (size limit, conflict shape, pause, exclude) and the
  snapshot history (host, time, files changed, branch, merges).
- **New directories** with Include and Ignore; **Held snapshots** with
  Apply and Drop; **Conflicts** with the two versions side by side and
  Keep mine / Take theirs / Dismiss; **Configuration** (globs, the lists,
  limits, conflict shape, per-bundle thresholds).
- Push notifications: a held snapshot, a conflict, a new directory over the
  threshold, a unit failing for more than 10 minutes.

## 8. On the hub

One bare repository per unit under `sync.dir/workspace/<unit>.git`
(`/var/lib/ccdash-hub/claude/workspace/` with the NixOS module, which puts
restic over it), created on the first push with `http.receivepack`,
`receive.shallowUpdate` (69 of 219 real units are shallow clones), `gc.auto`
off and a shared `pre-receive` guard. The smart HTTP routes
`/git/<unit>.git/{info/refs,git-upload-pack,git-receive-pack}` take the
collector tokens and spawn `git http-backend` as CGI with the body streamed
both ways; the NixOS module's nginx location has `client_max_body_size 0`
and `proxy_request_buffering off`.

The guard makes merges compare-and-swap: a push to `refs/sync/<host>` of a
commit with more than one parent is accepted only if every extra parent is
the current tip of another host's ref; otherwise it is refused with the
tips, the collector fetches and merges again, and since identical inputs
give identical trees the second round converges. Without it three hosts
merging the same pair at once re-triggered one conflict eight times. Plain
snapshots are never refused.

Tables: `workspace_units` (unit, bundle, kind, state, size, version),
`workspace_refs` (host → snapshot per unit), `workspace_conflicts`,
`workspace_events` (pushes with their bytes, the growth), `workspace_config`
(the one configuration, versioned), `workspace_seen` (what each host sees at
unit depth). `ccdash gc` runs `git gc --quiet` over every repository; the
module's timer runs it nightly at 03:30. Snapshot history is kept whole for
now; retention is a later knob.

```toml
# hub config
[workspace]
enable = true

# collector config
[workspace]
enable = true
root = "~/ProjectBundle"
# depth = 2
# include = ["tsandrini/*"]
# exclude = ["tsandrini/nixpkgs"]
# always_exclude = ["node_modules", ".venv", "venv", ".direnv", ".devenv", "target", "result", "result-*", "__pycache__", ".cache"]
# sync_ignored = [".env", ".env.*", ".envrc.local"]
# max_file_mb = 64
# auto_include_mb = 500
```

## 9. Costs

Measured on the real fleet (`meta/plans/workspace-sync/research.md` in the
owner's workspace) with the collector's unit at `CPUWeight=20`,
`IOWeight=20`, `Nice=10`, which the Home Manager module sets: idle 0.04 %
of one core on the busiest workstation, under 1 % during real work (120
edits, a vite dev server, an `npm install` of 38k files, 9k files written
into an ignored `target/`), 26 MiB resident; the Pi hub at 2–4 % of one
core while three machines work, 1.1–1.25 cores only during a seed. End to
end: a one-line edit lands on the other machine in ~50 ms over the LAN and
~170 ms over WireGuard. A seed of 219 units (13 GB) takes about 4 minutes;
an adoption reads the copy once (12–17 GB) and takes 2.5–4 minutes.
