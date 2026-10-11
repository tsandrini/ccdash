# ccdash DMS plugin

[DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) plugin
for [ccdash](../README.md): your running Claude Code sessions in the bar, the
dash and notifications.

- **Bar pill:** how many sessions need you (orange while one waits for a
  permission), how many are working. Left click lists them, a row focuses
  its terminal; right click opens the dash tab.
- **Dash tab** (`dms ipc call dash toggle plugin_ccdash`): sessions by state
  on the left, the selected one on the right (what it waits for or runs,
  Claude's latest text, tasks, subagents, resources). Keys: `↑`/`↓` or
  `j`/`k` select, `Enter` focuses, `e` (or `F2`) renames, `w` opens the
  conversation in the web UI, `r` opens it in Claude, `[`/`]` switch
  workspace. "Connect phone" opens the web UI's QR code for the phone.
- **Dash card:** the same counts and the 5-hour limit on the overview.
- **Notifications** when a session starts waiting for you and its window is
  not focused; clicking one focuses the session.
- **IPC:** `dms ipc call ccdash focusWaiting` focuses the session that has
  waited longest (permission prompts first); also `open <id>`,
  `rename <id> <title>` and `status`.

Links to the web UI carry the access token (read with `ccdash url`), so the
desktop browser never asks for it.

It talks to the ccdash daemon over `$XDG_RUNTIME_DIR/ccdash/api.sock` and
focuses windows with `ccdash open`, so both must be available. Install it by
linking this directory to `~/.config/DankMaterialShell/plugins/ccdash` (the
Home Manager option `services.ccdash.dms.enable` does that), then enable it
under Settings → Plugins and add the widget to a bar.
