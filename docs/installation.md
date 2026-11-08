# Installation

ccdash runs on NixOS with flakes and Home Manager: a **hub** keeps the
state and serves the panel, a **collector** on every workstation reports
its Claude Code sessions. Both can be one machine.

1. [Before you start](#1-before-you-start)
2. [The flake input](#2-the-flake-input)
3. [One machine](#3-one-machine)
4. [A hub and workstations](#4-a-hub-and-workstations)
5. [Tokens](#5-tokens)
6. [Your phone](#6-your-phone)
7. [Optional features](#7-optional-features)
8. [Updating](#8-updating)

## 1. Before you start

- NixOS with flakes, and Home Manager (as a NixOS module or standalone)
  with `programs.claude-code` enabled.
- Claude Code 2.1.157 or later as `programs.claude-code.package`, so every
  session gets the ccdash plugin (the knowledge tools and skills). A Home
  Manager without `programs.claude-code.plugins` builds with a warning, and
  only sessions with the channel get them.
- To look around first: `nix run github:tsandrini/ccdash#demo` serves the
  panel on a made-up workspace at <http://127.0.0.1:7840/#token=demo>.
- ccdash builds from source (a Rust daemon and a Svelte panel, a few
  minutes). A hub on a Raspberry Pi is better built on a faster machine:
  the flake has a cross-compiled `ccdash-aarch64-linux`.

  ```nix
  services.ccdash-hub = {
    package = inputs.ccdash.packages.x86_64-linux.ccdash-aarch64-linux;
    webPackage = inputs.ccdash.packages.x86_64-linux.ccdash-web; # static files
  };
  ```

## 2. The flake input

```nix
inputs.ccdash = {
  url = "github:tsandrini/ccdash";
  inputs.nixpkgs.follows = "nixpkgs";
  inputs.home-manager.follows = "home-manager"; # only its tests use it
};
```

The hub's module is `inputs.ccdash.nixosModules.hub`, the workstation's
`inputs.ccdash.homeModules.default`; `inputs.ccdash.nixosModules.default`
allowlists the reply channel (section 7).

## 3. One machine

The hub and a workstation in one, for a laptop or a desktop you use alone:

```nix
# NixOS
imports = [ inputs.ccdash.nixosModules.hub ];
services.ccdash-hub = {
  enable = true;
  localCollector = { enable = true; user = "me"; };
  workspaces = [ "/home/me/code" ];
};

# Home Manager, user me
imports = [ inputs.ccdash.homeModules.default ];
services.ccdash = {
  enable = true;
  hub.local = true;
};
```

The hub generates its tokens once (`/var/lib/ccdash-hub/ui-token` and the
local collector's) and listens on `127.0.0.1:7837`. To open the panel:

```sh
sudo -u ccdash ccdash url --hub --config /etc/ccdash/hub.toml
```

It prints a link that signs the browser in with a token of its own. The
**workspaces** are the directories your projects live in; see
[getting started](getting-started.md#2-workspaces-and-projects).

The collector starts with `graphical-session.target`. On a machine nobody
logs in to graphically, start it with the user manager instead:

```nix
services.ccdash.systemdTarget = "default.target"; # Home Manager
users.users.me.linger = true; # NixOS: the user manager runs from boot
```

## 4. A hub and workstations

### 4.1 The hub

An always-on machine (a home server, a Pi) that every workstation and the
phone reach, on your LAN or VPN:

```nix
imports = [ inputs.ccdash.nixosModules.hub ];
services.ccdash-hub = {
  enable = true;
  collectors = {
    desk = "/run/agenix/ccdash-collector-desk"; # host name → its token file
    laptop = "/run/agenix/ccdash-collector-laptop";
  };
  workspaces = [ "/home/me/code/work" "/home/me/code/personal" ];
  nginx = { enable = true; domain = "ccdash.example.org"; };
  backup.enable = true;
};
# TLS, listen addresses and access control of the vhost are yours:
services.nginx.virtualHosts."ccdash.example.org" = {
  enableACME = true;
  forceSSL = true;
};
```

Keep it off the public internet: see [security](security.md).

### 4.2 Each workstation

```nix
imports = [ inputs.ccdash.homeModules.default ];
services.ccdash = {
  enable = true;
  hub.url = "wss://ccdash.example.org/collect";
  hub.tokenFile = "\${XDG_RUNTIME_DIR}/agenix/ccdash-collector-desk";
};
```

The machine's host name must be its key in the hub's `collectors`
(`desk` above). Every option: [Home Manager module
options](options/home-manager.md), [hub module options](options/hub.md).

A workstation can also run Windows, with a script in the place of Home
Manager: [a Windows workstation](windows.md).

## 5. Tokens

- **The bootstrap token** of the panel: leave `tokenFile` unset and the hub
  generates one; devices then pair with links of their own (section 6).
- **One collector token per workstation**: any secret string, the same on
  both ends. Make one with `openssl rand -hex 32`.
- The hub's copy must be readable by the `ccdash` user, the workstation's
  by you. Any secrets tool works: agenix (`owner = "ccdash"` on the hub, the
  Home Manager module on the workstation), sops-nix, or plain files with
  mode `0400`.
- One machine (section 3) needs none of this: `localCollector` generates its
  collector's token.

## 6. Your phone

The panel is a web app, installable from the browser's menu. The phone
needs to reach the hub (a VPN, or nginx on your LAN), and Web Push needs
HTTPS. On the hub:

```sh
sudo -u ccdash ccdash url --hub --config /etc/ccdash/hub.toml --qr
```

Scan the code: the link pairs the phone with a token of its own (good for
an hour). Then Settings › Devices turns push on. More:
[prompts and notifications](prompts-and-notifications.md).

## 7. Optional features

Everything beyond reporting and answering is off until you turn it on.

| Feature                                                 | Turn on                                                                     | More                                          |
| ------------------------------------------------------- | --------------------------------------------------------------------------- | --------------------------------------------- |
| Replies from the panel (the channel)                    | HM `claudeCode.channel.enable`, NixOS `programs.ccdash.channel.allow`       | [prompts](prompts-and-notifications.md)       |
| Open a background session, edit a prompt in your editor | HM `terminal` (`{ command = [ "ghostty" "-e" ]; appId = …; }`) and `editor` | [sessions](sessions.md)                       |
| Sessions and prompt history on every machine            | HM `sync.enable`                                                            | [state sync](state-sync.md)                   |
| The workspace root on every machine                     | hub `workspace.enable`, HM `workspace.enable` and `workspace.root`          | [workspace sync](workspace.md)                |
| Lock and suspend from the panel                         | HM `power.lock`, `power.suspend`                                            | [machines](machines.md)                       |
| Model, context, cost and limits per session             | HM `claudeCode.statusLine.enable`                                           | [panel](panel.md)                             |
| Tokens, cache hits, commits in Analytics                | HM `claudeCode.telemetry.enable`                                            | [panel](panel.md#4-history-and-analytics)     |
| The DankMaterialShell bar pill and dash tab             | HM `dms.enable`                                                             | [panel](panel.md#7-desktop-dankmaterialshell) |
| A nightly database copy                                 | hub `backup.enable`                                                         | [data](data.md)                               |

The panel shows only what the machines do: without the syncs there is no
Workspace or Claude state page, without a terminal no Open button.

## 8. Updating

```sh
nix flake update ccdash
```

Then rebuild the hub and every workstation together. Hub and collectors
speak a versioned protocol; a collector older or newer than its hub is
refused (`refused: the hub speaks protocol 16, not 15` in its log) until
both run the same release.
