# ccdash

A live dashboard for all your Claude Code sessions, on every machine: which
are working, which wait for you, and what they did. A web panel for the
phone, a [DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell)
plugin for the desktop, and a hub that keeps your sessions, prompt history
and notes in step across machines.

> **Status:** work in progress, in daily use.

![The Now page: a session waiting on a permission prompt with Allow and Deny, one on your turn with Claude's reply, the 5-hour, 7-day and model limits with their forecasts, the machines and recent progress](docs/images/now.png)

The screenshots come from `nix run github:tsandrini/ccdash#demo`, a made-up
workspace; more are in the [panel](docs/panel.md) guide.

## Contents

- [1. What it does](#1-what-it-does)
- [2. How it fits together](#2-how-it-fits-together)
- [3. Install](#3-install)
  - [3.1 The hub](#31-the-hub)
  - [3.2 Each workstation](#32-each-workstation)
  - [3.3 Your phone](#33-your-phone)
  - [3.4 One machine](#34-one-machine)
  - [3.5 A Windows workstation](#35-a-windows-workstation)
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
- **Talk to** a running session, **stop** its turn when it heads the wrong
  way and tell it what you meant, **start** one in a known project,
  **find and resume** any past one, from any machine, on the machine you
  pick (a search over what was said), mark one **resolved** (Claude
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
- **Keep the reports** sessions make: one self-contained HTML file in a
  shared design, published to the hub, read in the panel, searched like a
  note, downloaded to send on. ([reports](docs/reports.md))
- **Look back:** history lanes and analytics (waits, limits, turns,
  tokens, permissions). ([panel](docs/panel.md#4-history-and-analytics))
- **Machines:** uptime, CPU, temperature, memory, disks and battery of
  the hub and every machine; **lock** a screen left unlocked, **suspend**
  a machine from the phone and **wake** it again over the LAN, without
  ever typing a MAC.
  ([machines](docs/machines.md))
- **On the desktop:** a DankMaterialShell bar pill and dash tab, and a key
  to focus the session that waited longest.
  ([panel](docs/panel.md#7-desktop-dankmaterialshell))

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
Manager module for the workstations. It needs NixOS, flakes and Home
Manager (with `programs.claude-code`).

```nix
inputs.ccdash = {
  url = "github:tsandrini/ccdash";
  inputs.nixpkgs.follows = "nixpkgs";
  inputs.home-manager.follows = "home-manager"; # only its tests use it
};
```

Everything beyond reporting is off until you turn it on: the syncs,
replies from the phone, opening sessions in a terminal, the desktop plugin.

To look around first, `nix run github:tsandrini/ccdash#demo` serves the
panel on a made-up workspace with two stand-in machines at
<http://127.0.0.1:7840/#token=demo>; nothing in it is real.

### 3.1 The hub

```nix
{
  imports = [ inputs.ccdash.nixosModules.hub ];

  services.ccdash-hub = {
    enable = true;
    collectors = {
      desk = "/run/agenix/ccdash-collector-desk";       # one token per machine
      laptop = "/run/agenix/ccdash-collector-laptop";
    };
    workspaces = [ "/home/me/code/work" "/home/me/code/personal" ];
    nginx = { enable = true; domain = "ccdash.example.org"; };
    backup.enable = true;                               # nightly database copy
  };
  # TLS, listen addresses and access control of the vhost are yours:
  services.nginx.virtualHosts."ccdash.example.org" = { ... };
}
```

A collector token is any secret string in a file the `ccdash` user can
read (`openssl rand -hex 32`), kept with agenix, sops-nix or by hand. The
**workspaces** are the directories your projects live in, the same paths
on every machine: notes, issues and reports belong to one, and the
knowledge tools refuse sessions outside them. Keep the hub on your LAN or
VPN: see [security](docs/security.md).

### 3.2 Each workstation

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    hub.url = "wss://ccdash.example.org/collect";
    hub.tokenFile = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-desk";
    terminal = { command = [ "ghostty" "-e" ]; appId = "com.mitchellh.ghostty"; };
    editor = "nvim";                    # the panel's Edit in the editor
    # optional:
    sync.enable = true;                 # sessions and prompt history on every machine
    claudeCode.channel.enable = true;   # reply to sessions from the phone
    dms.enable = true;                  # the DankMaterialShell plugin
  };
}
```

The module wires the hooks into `programs.claude-code` and gives every
session the ccdash plugin: the knowledge tools and the `ccdash:wrap-up`,
`ccdash:report` and `ccdash:project` skills. The machine's host name must
match a key of `collectors` on the hub. For replies from the phone also
allow the channel on the NixOS side:

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

### 3.4 One machine

The hub and a workstation in one: the hub module generates the local
collector's token and the Home Manager module picks it up.

```nix
# NixOS
services.ccdash-hub = {
  enable = true;
  localCollector = { enable = true; user = "me"; };
  workspaces = [ "/home/me/code" ];
};
# Home Manager (user me)
services.ccdash = {
  enable = true;
  hub.local = true;
};
```

The panel listens on `127.0.0.1:7837`: the command in 3.3 without `--qr`
prints a link that signs this browser in; a phone needs the hub reachable
from it first (nginx, a VPN). Notes from before ccdash come in with
`ccdash import memory <dir>` ([knowledge](docs/knowledge.md#6-import-and-export)).

### 3.5 A Windows workstation

A workstation can also run Windows, reporting to a hub on Linux (the hub
runs on Linux only). From the repository:

```powershell
pwsh -File windows\install.ps1 -Build -Hub wss://ccdash.example.org/collect -Token .\token
```

It installs the collector, the hooks and the plugin, and starts the
collector at logon; the channel, Open and Edit are switches, off until
given. Steps and differences: [Windows](docs/windows.md).

## 4. Everyday use

On a workstation:

```sh
ccdash status               # this machine's sessions
ccdash open <id>            # focus its window, or attach a background session
ccdash rename <id> <name>   # Claude Code adopts it at the next prompt
ccdash reply <id> <text>    # a message into the session (needs the channel); wakes a sleeping one
claude --project ccdash     # a new session in this workspace, under that project
ccdash url                  # the panel's address
ccdash workspace status     # the workspace sync: where every unit stands here
```

In the panel:

| Page         | For                                                                                                                                          |
| ------------ | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Now          | the page to leave open: what needs you, with the answer inline; what each session is doing; limits, machines, progress                       |
| Sessions     | every session; its page has the chat, the shell and the details                                                                              |
| Projects     | each project's board, sessions, progress, documents, reports, memories, graph                                                                |
| Reports      | every published report, newest first; a report's page frames it with its abstract, Download and Open in a tab                                |
| History      | sessions over time as lanes, time per project, tools, the 5-hour limit                                                                       |
| Analytics    | waits, parallel work, limits, turns, context, tokens, permissions                                                                            |
| Machines     | every machine: its link, sessions, Claude state and workspace sync                                                                           |
| Claude state | each machine's copy of `~/.claude`, conflicts, sizes and growth                                                                              |
| Workspace    | the workspace root on every machine: overview, units × machines, each machine's link and cost, statistics, the rules and what they leave out |
| Settings     | devices, push, theme, quick replies and templates                                                                                            |

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

The documentation is also a site, with the module options generated from
the modules: <https://tsandrini.github.io/ccdash/>. Start with
[installation](docs/installation.md), [getting
started](docs/getting-started.md) and
[troubleshooting](docs/troubleshooting.md).

| Document                                                       | About                                                         |
| -------------------------------------------------------------- | ------------------------------------------------------------- |
| [architecture](docs/architecture.md)                           | components, protocol, what Claude Code interfaces are used    |
| [panel](docs/panel.md)                                         | every page of the panel and the DMS plugin                    |
| [prompts and notifications](docs/prompts-and-notifications.md) | answering prompts, Web Push, the channel, renaming            |
| [sessions](docs/sessions.md)                                   | start, resume (past sessions), resolve, kill, open, forget    |
| [state sync](docs/state-sync.md)                               | `~/.claude` on every machine, the one-machine rule, conflicts |
| [workspace](docs/workspace.md)                                 | the workspace root on every machine: units, snapshots, merges |
| [knowledge](docs/knowledge.md)                                 | notes, tools, injections, import and export                   |
| [reports](docs/reports.md)                                     | HTML reports: the skill, publishing, the viewer, storage      |
| [machines](docs/machines.md)                                   | uptime and basic numbers, lock, suspend, wake-on-LAN          |
| [windows](docs/windows.md)                                     | a Windows workstation: install, hooks, paths, differences     |
| [security](docs/security.md)                                   | tokens, devices, what can reach the hub                       |
| [data](docs/data.md)                                           | what is stored where, retention, backups                      |
| [configuration](docs/configuration.md)                         | every configuration key                                       |
| [development](docs/development.md)                             | building, testing, the panel                                  |

## 8. License

[EUPL-1.2](LICENSE)
