# Prompts, notifications and replies

1. [Answering prompts](#1-answering-prompts)
2. [Notifications (Web Push)](#2-notifications-web-push)
3. [Replying to a session](#3-replying-to-a-session)
4. [Renaming](#4-renaming)

## 1. Answering prompts

Tool approvals, questions (`AskUserQuestion`) and plans (`ExitPlanMode`)
show up in the panel while the terminal dialog stays open; whichever answer
comes first wins.

| Prompt    | Answers                                                                        |
| --------- | ------------------------------------------------------------------------------ |
| Tool call | Allow, Deny, Always allow (when Claude Code suggests a rule), Deny with a note |
| Question  | one button per option (several for multi-select), or your own text             |
| Plan      | Approve, Keep planning (with a note)                                           |

"Always allow" saves the rule where the terminal's "don't ask again" does,
usually the project's `.claude/settings.local.json`. Deny stops Claude like
the terminal's No; "Deny with a note" lets it carry on with your note.

How: the PermissionRequest hook is `ccdash ask`. It hands the prompt to the
collector, which holds it until the hub answers or the prompt closes in the
terminal (or the hub link drops: then the hook lets go and the terminal
alone decides). Claude Code runs the hook alongside the dialog (verified on
2.1.302; the [hooks reference](https://code.claude.com/docs/en/hooks)
promises this only for SDK hosts, so recheck after updates).
`services.ccdash.claudeCode.asks.enable = false` makes the hook report
prompts only.

## 2. Notifications (Web Push)

Served over HTTPS, the panel installs as an app and takes Web Push:
Settings → "Turn on push notifications", then "Send a test".

- A notification shows the prompt with as many buttons as the platform
  allows (two on Android); tapping it opens the session. A watch mirroring
  the phone can press the buttons too.
- It waits 15 s (2 minutes while the session's window has focus), so a
  prompt answered at the desk never sends one, and it closes once the
  prompt is answered anywhere.
- A finished turn of a session started from the panel notifies "Finished".
- The hub generates its VAPID key pair on first start; messages are
  encrypted for the device (RFC 8291), so the push service sees only
  ciphertext. Devices the service reports gone are forgotten.
- A button calls `POST /act/<key>/<action>` with a random key that belongs
  to that one prompt and dies with it; the access token never goes into a
  notification. The phone has to reach the hub (LAN or VPN).

```nix
services.ccdash-hub.settings.push.subject = "mailto:you@example.org";
services.ccdash-hub.settings.notify = {
  turns = false;              # no "Finished"
  workspaces = [ "work" ];    # only these workspaces
};
```

## 3. Replying to a session

With the channel, a session's page gets a message box. A message sent while
Claude works waits for its next turn; Claude sees it as a
`<channel source="plugin:ccdash:ccdash">` event and the terminal shows
`← ccdash: …`.

```nix
# Home Manager: start interactive sessions with the channel
services.ccdash.claudeCode.channel.enable = true;
# NixOS: allow the plugin in Claude Code's managed settings
programs.ccdash.channel.allow = true;
```

This uses Claude Code
[channels](https://code.claude.com/docs/en/channels-reference), a research
preview: `ccdash channel` is an MCP server every such session runs,
connected to the collector.

- Without the allowlist entry, `claudeCode.channel.devFlag = true` loads the
  channel with `--dangerously-load-development-channels`, and Claude Code
  asks for confirmation at every launch.
- Claude Code's startup notice lists `plugin:ccdash@inline · plugin not
installed`: it only counts marketplace plugins; the channel works anyway.
- Team and Enterprise accounts need an admin to enable channels.
- Sessions started before (or without the wrapper) show how to restart them.

## 4. Renaming

Sessions can be renamed in the panel, the DMS plugin and with
`ccdash rename`. The new name shows at once and reaches Claude Code at the
session's next prompt (or resume) through the `sessionTitle` output of
ccdash's hook, so the terminal title and Remote Control follow.
