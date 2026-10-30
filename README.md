# ccdash

A live dashboard for all your Claude Code sessions, on every machine: which
are working, which wait for you, and what they did. A web panel for the
phone, a [DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell)
plugin for the desktop, and a hub that keeps your sessions, prompt history
and notes in step across machines.

> **Status:** work in progress, in daily use.

## Contents

- [1. What it does](#1-what-it-does)
- [2. How it fits together](#2-how-it-fits-together)
- [3. Install](#3-install)
  - [3.1 The hub](#31-the-hub)
  - [3.2 Each workstation](#32-each-workstation)
  - [3.3 Your phone](#33-your-phone)
- [4. Everyday use](#4-everyday-use)
- [5. Configuration](#5-configuration)
- [6. Development](#6-development)
- [7. Documentation](#7-documentation)
- [8. License](#8-license)

## 1. What it does

- **See** every session on every machine: working, waiting on a
  permission prompt, your turn, parked; with its conversation, shell
  output, files and cost. ([panel](docs/panel.md))
- **Answer** permission prompts, questions and plans from the phone, also
  from the notification's buttons; notifications wait 15 s, so a prompt
  answered at the desk never sends one.
  ([prompts and notifications](docs/prompts-and-notifications.md))
- **Talk to** a running session, **start** one in a known project,
  **continue** an ended one on any machine, mark one **resolved** (Claude
  wraps up, records what it learned, then it closes) or **kill** one, even
  a stuck one.
  ([sessions](docs/sessions.md))
- **Move between machines:** a session ended on the desk resumes on the
  laptop with a plain `claude --resume`; a session is never open on two
  machines at once, and conflicts are kept, never lost.
  ([state sync](docs/state-sync.md))
- **Remember:** memories, issues, progress and documents shared by every
  session of a workspace, through tools like `recall` and `remember`.
  ([knowledge](docs/knowledge.md))
- **Look back:** history lanes and analytics (waits, limits, turns,
  tokens, permissions). ([panel](docs/panel.md#4-history-and-analytics))
- **On the desktop:** a DankMaterialShell bar pill and dash tab, and a key
  to focus the session that waited longest.
  ([panel](docs/panel.md#5-desktop-dankmaterialshell))

## 2. How it fits together

```text
each workstation                                  the hub (always on)
Claude Code hooks, statusline ──► ccdash collector ══wss══► ccdash hub ──► web panel
claude agents, /usage, niri  ──►   (+ ~/.claude sync)       (SQLite)   ──► Web Push
```

A small **collector** on each machine reports through Claude Code's hooks
and polls to the **hub**, which keeps the state and history and serves the
panel. Details: [architecture](docs/architecture.md).

## 3. Install

ccdash ships as a Nix flake with a NixOS module for the hub and a Home
Manager module for the workstations.

### 3.1 The hub

```nix
{
  imports = [ inputs.ccdash.nixosModules.hub ];

  services.ccdash-hub = {
    enable = true;
    tokenFile = "/run/agenix/ccdash-ui-token";         # bootstrap token
    collectors = {
      desk = "/run/agenix/ccdash-collector-desk";       # one token per machine
      laptop = "/run/agenix/ccdash-collector-laptop";
    };
    nginx = { enable = true; domain = "ccdash.example.org"; };
    backup.enable = true;                               # nightly database copy
    settings.workspaces.dirs = [ "/home/me/code/work" "/home/me/code/personal" ];
  };
  # TLS, listen addresses and access control of the vhost are yours:
  services.nginx.virtualHosts."ccdash.example.org" = { ... };
}
```

Keep the hub on your LAN or VPN: see [security](docs/security.md).

### 3.2 Each workstation

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    hub.url = "wss://ccdash.example.org/collect";
    hub.tokenFile = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-desk";
    sync.enable = true;                 # sessions and prompt history on every machine
    claudeCode.channel.enable = true;   # reply to sessions from the phone
    dms.enable = true;                  # the DankMaterialShell plugin
  };
}
```

The module wires the hooks into `programs.claude-code`. The machine's host
name must match a key of `collectors` on the hub. For replies from the phone
also allow the channel on the NixOS side:

```nix
imports = [ inputs.ccdash.nixosModules.default ];
programs.ccdash.channel.allow = true;
```

### 3.3 Your phone

```sh
sudo -u ccdash ccdash url --hub --config /etc/ccdash/hub.toml --qr
```

Scan the code: the link pairs the phone with a token of its own. Install the
panel from the browser's menu and turn on push in Settings.

## 4. Everyday use

On a workstation:

```sh
ccdash status               # this machine's sessions
ccdash open <id>            # focus its window, or attach a background session
ccdash rename <id> <name>   # Claude Code adopts it at the next prompt
ccdash reply <id> <text>    # a message into the session (needs the channel)
ccdash url                  # the panel's address
ccdash workspace status     # the workspace sync: where every unit stands here
```

In the panel:

| Page      | For                                                                    |
| --------- | ---------------------------------------------------------------------- |
| Now       | what needs you, with the answer inline; what is working; limits        |
| Sessions  | every session; its page has the chat, the shell and the details        |
| Projects  | each project's board, sessions, progress, documents, memories, graph   |
| History   | sessions over time as lanes, time per project, tools, the 5-hour limit |
| Analytics | waits, parallel work, limits, turns, context, tokens, permissions      |
| State     | each machine's copy of `~/.claude`, conflicts, sizes and growth        |
| Workspace | the workspace root on every machine: units × machines, conflicts, held |
| Settings  | devices, push, theme, quick replies and templates                      |

The panel in detail: [panel](docs/panel.md).

## 5. Configuration

The modules write the TOML files; anything not covered by an option goes
into `settings`:

```nix
services.ccdash-hub.settings = {
  store.retention_days = 90;          # history kept
  notify.turns = false;               # no "Finished" notifications
};
services.ccdash.settings = {
  procs.enable = false;               # no CPU and memory sampling
  inject.edits = false;               # no "who else edited this file" hints
};
```

Every key with its default: [configuration](docs/configuration.md).

## 6. Development

```sh
direnv allow                                     # or: nix develop
cargo run -- hub --config hub.toml
cargo run -- collector --config collector.toml   # hub.url = "ws://127.0.0.1:7837/collect"
cd web && npm ci && npm run dev                  # the panel on :5173
cargo test && cargo clippy --all-targets
nix flake check
```

More (generated API types, the panel's npm hash, a second collector next to
the real one): [development](docs/development.md).

## 7. Documentation

| Document                                                       | About                                                         |
| -------------------------------------------------------------- | ------------------------------------------------------------- |
| [architecture](docs/architecture.md)                           | components, protocol, what Claude Code interfaces are used    |
| [panel](docs/panel.md)                                         | every page of the panel and the DMS plugin                    |
| [prompts and notifications](docs/prompts-and-notifications.md) | answering prompts, Web Push, the channel, renaming            |
| [sessions](docs/sessions.md)                                   | start, continue, resolve, kill, open, forget                  |
| [state sync](docs/state-sync.md)                               | `~/.claude` on every machine, the one-machine rule, conflicts |
| [workspace](docs/workspace.md)                                 | the workspace root on every machine: units, snapshots, merges |
| [knowledge](docs/knowledge.md)                                 | notes, tools, injections, import and export                   |
| [security](docs/security.md)                                   | tokens, devices, what can reach the hub                       |
| [data](docs/data.md)                                           | what is stored where, retention, backups                      |
| [configuration](docs/configuration.md)                         | every configuration key                                       |
| [development](docs/development.md)                             | building, testing, the panel                                  |

## 8. License

[EUPL-1.2](LICENSE)
