# Troubleshooting

Where to look: the collector's log with `journalctl --user -u ccdash` on the
workstation (on Windows `%LOCALAPPDATA%\ccdash\state\collector.log`), the
hub's with `journalctl -u ccdash-hub` on the hub, and `ccdash status` for
what the hub knows of this machine.

1. [A machine does not show up](#1-a-machine-does-not-show-up)
2. [The knowledge tools refuse](#2-the-knowledge-tools-refuse)
3. [A session has no ccdash tools or skills](#3-a-session-has-no-ccdash-tools-or-skills)
4. [A button is missing](#4-a-button-is-missing)
5. [No notifications](#5-no-notifications)
6. [The panel asks for a token](#6-the-panel-asks-for-a-token)

## 1. A machine does not show up

Read the collector's log; the first line about the hub says why.

| The collector says                                                   | Because                                                                                                             |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `connecting: HTTP error: 401 Unauthorized`                           | the token is not one the hub lists: another secret, the wrong file, or the hub not rebuilt since                    |
| `refused: this token is desk's, but the machine calls itself laptop` | the host name differs from its key in the hub's `collectors`: rename the key or set the collector's `settings.host` |
| `refused: the hub speaks protocol 16, not 15`                        | the hub and this collector are different releases: update both ([installation](installation.md#8-updating))         |
| `connecting: … Connection refused`                                   | the hub is down, or `hub.url` points elsewhere (`/collect` on the hub's address, `wss://` behind nginx)             |

Nothing in the log at all: `systemctl --user status ccdash`. The collector
starts with `graphical-session.target`; a machine without a graphical
session needs `services.ccdash.systemdTarget = "default.target"` and
lingering ([installation](installation.md#3-one-machine)).

While the hub is unreachable the collector queues what happens and sends
it once the link is back; nothing is lost.

## 2. The knowledge tools refuse

`… is outside every ccdash workspace`: the session runs in a directory
under none of the hub's `workspaces`. With `the hub has none yet` the list
is empty; set `services.ccdash-hub.workspaces`. Otherwise start the session
inside a workspace, at the same path as on the other machines
([getting started](getting-started.md#2-workspaces-and-projects)).

## 3. A session has no ccdash tools or skills

- `claude plugin list` should show `ccdash@skills-dir`.
- The plugin needs `claudeCode.plugin.enable` (the default), Claude Code
  2.1.157 or later, and a Home Manager with `programs.claude-code.plugins`;
  without that option the build warns and only channel sessions get it.
- A session started before the switch keeps what it had: start a new one,
  or `claude --resume` it.

## 4. A button is missing

The panel offers only what the machine does.

| Missing                           | Turn on                                                                                           |
| --------------------------------- | ------------------------------------------------------------------------------------------------- |
| Reply, Resolved                   | the channel: `claudeCode.channel.enable` and `programs.ccdash.channel.allow`; restart the session |
| Open, Edit in the editor          | `services.ccdash.terminal` and `editor`                                                           |
| Start a session (no machine)      | a connected collector with Claude Code (`launch.command`)                                         |
| Lock, Suspend                     | `power.lock`, `power.suspend`                                                                     |
| The Claude state, Workspace pages | `sync.enable`; the hub's and the collectors' `workspace.enable`                                   |

On Windows the script's switches turn these on
([Windows](windows.md#2-optional-features)).

Without the allowlist Claude Code does not load the channel, so Reply stays
away; `claudeCode.channel.devFlag` loads it anyway, at the price of a
warning dialog at every start.

## 5. No notifications

- Web Push needs the panel over HTTPS (or `localhost`), and push turned on
  for the device in Settings › Devices; **Send a test** there checks it.
- A notification waits 15 s, so a prompt answered in the meantime sends
  none.
- What notifies is set on the hub (`notify.*` in the
  [configuration](configuration.md#3-hub-keys)).

## 6. The panel asks for a token

Open the link `sudo -u ccdash ccdash url --hub --config /etc/ccdash/hub.toml`
prints on the hub ([installation](installation.md#6-your-phone)): it signs
the browser in with a token of its own. A device signed out after the
bootstrap token changed pairs again the same way.
