# ccdash

A live dashboard for all your running Claude Code sessions: which ones are
working, which are waiting on you, and which look stuck. On the desktop as a
[DankMaterialShell](https://github.com/AvengeMedia/DankMaterialShell) plugin,
on the phone as a web page.

> **Status:** work in progress. The collector daemon and the web UI work;
> the DMS plugin is not built yet.

## How it works

```text
Claude Code hooks ──► ccdash emit ─┐
statusline ─────────► ccdash tee ──┼──► ccdash daemon ──┬──► web UI (LAN / VPN)
claude agents --json ──────────────┤    (per host)      └──► DMS plugin
niri window events ────────────────┘
```

- Every session reports through Claude Code hooks to a small daemon on the
  same machine. The daemon keeps live state in memory and history in SQLite.
- The **web UI** is read-only and meant for the phone. Tapping a session opens
  it in the Claude app through Remote Control.
- The **DMS plugin** adds a bar indicator, a quick list, and a full dashboard
  tab. Clicking a session focuses its terminal window in niri.

Only public Claude Code interfaces are used (`claude agents --json`, hooks,
the statusline payload); no internal state files.

## Usage

With Home Manager (the module also wires the hooks into
`programs.claude-code`):

```nix
{
  imports = [ inputs.ccdash.homeModules.default ];

  services.ccdash = {
    enable = true;
    settings.http.listen = [ "[::]:7837" ];
    # keep your statusline, ccdash reads the stats on its way through
    claudeCode.statusLine = {
      enable = true;
      command = lib.getExe pkgs.ccstatusline;
    };
  };
}
```

Then:

```sh
ccdash status          # what every session is doing
ccdash open <id>       # focus its window, or attach to a background session
ccdash url --host <ip> # web address for the phone, token included
```

The web UI lives on the HTTP port: sessions that need you first, then the
working and idle ones, a detail view with the last prompt and reply, and a
history view with usage and a per-session activity timeline. Open the
address from `ccdash url` once; the token is kept in the browser after that.

The HTTP API is read-only and needs the bearer token (generated in
`~/.local/state/ccdash/token` unless `http.token_file` is set). Keep the
port behind your firewall or VPN.

## Development

```sh
direnv allow   # or: nix develop
cargo run -- daemon
nix build
```

`CCDASH_WEB_DIR=web` serves the UI from disk instead of the embedded copy,
so edits show up on reload.

## License

[EUPL-1.2](LICENSE)
