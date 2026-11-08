# ccdash

A live dashboard for all your Claude Code sessions, on every machine: which
are working, which wait for you, and what they did. A web panel for the
phone and the desk, notes every session shares, and a hub that keeps them
in step across machines. It runs on NixOS with flakes and Home Manager.

![The Now page: a session waiting on a permission prompt with Allow and Deny, one on your turn with Claude's reply, the limits with their forecasts, the machines and recent progress](images/now.png)

## Start here

- **Look around first:** `nix run github:tsandrini/ccdash#demo` serves the
  panel on a made-up workspace at <http://127.0.0.1:7840/#token=demo>.
- [Installation](installation.md): one machine, or a hub and your
  workstations, then your phone. A workstation can also run
  [Windows](windows.md).
- [Getting started](getting-started.md): workspaces, the knowledge tools and
  skills, a first session from the phone.
- [Troubleshooting](troubleshooting.md) when a machine does not show up.

## The guides

| Page                                                      | About                                                         |
| --------------------------------------------------------- | ------------------------------------------------------------- |
| [The panel](panel.md)                                     | every page of the panel and the desktop plugin                |
| [Prompts and notifications](prompts-and-notifications.md) | answering prompts, Web Push, the channel, renaming            |
| [Sessions](sessions.md)                                   | start, resume, resolve, kill, open, forget                    |
| [Knowledge](knowledge.md)                                 | notes, tools, injections, import and export                   |
| [Reports](reports.md)                                     | HTML reports: the skill, publishing, the viewer, storage      |
| [Machines](machines.md)                                   | uptime and basic numbers, lock, suspend, wake-on-LAN          |
| [State sync](state-sync.md)                               | `~/.claude` on every machine, the one-machine rule, conflicts |
| [Workspace sync](workspace.md)                            | the workspace root on every machine                           |
| [Configuration](configuration.md)                         | every key, and the module options                             |
| [Architecture](architecture.md)                           | components, protocol, the Claude Code interfaces used         |
| [Data](data.md)                                           | what is stored where, retention, backups                      |
| [Security](security.md)                                   | tokens, devices, what can reach the hub                       |
| [Development](development.md)                             | building, testing, the demo, the screenshots                  |

The source and issues are on
[GitHub](https://github.com/tsandrini/ccdash); the license is EUPL-1.2.
