# `claude` with the ccdash channel: the Windows twin of the Home Manager module's `claude`
# wrapper (flake-parts/modules/home-manager/ccdash.nix, `claudeCode.channel.enable`).
# `install.ps1 -Channel` puts it in ~\.local\share\ccdash and dot-sources it from the
# PowerShell profile.
#
# Interactive sessions start with `--channels plugin:ccdash@inline --plugin-dir <plugin>`, so
# the panel can reply to them; the plugin's tools and skills reach every session anyway, from
# ~\.claude\skills\ccdash. Subcommands, -p/--print and --bg go to Claude Code as they are.
# `claude --project <name>` is ccdash's flag: it sets CCDASH_PROJECT, which the session's hooks
# hand to the hub. `claude.exe` itself is never changed.
#
# '--channels' needs the channel on Claude Code's allowlist: `allowedChannelPlugins` in managed
# settings. On a Team or Enterprise account that is the organization's server-managed settings,
# which outrank a local managed-settings.json. Without it, set
# $env:CCDASH_CHANNEL_FLAG = '--dangerously-load-development-channels' (Claude Code asks at
# every launch).

function claude {
  $real = "$env:USERPROFILE\.local\bin\claude.exe"
  if (-not (Test-Path $real)) {
    $real = (Get-Command claude -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1).Source
    if (-not $real) { Write-Error 'claude: Claude Code (claude.exe) not found'; return }
  }
  $plugin = "$env:USERPROFILE\.local\share\ccdash\claude-plugin"
  $flag = if ($env:CCDASH_CHANNEL_FLAG) { $env:CCDASH_CHANNEL_FLAG } else { '--channels' }
  # NOTE: `claude <subcommand>` parsers reject the channel flags
  $subcommands = 'agents', 'attach', 'auth', 'auto-mode', 'daemon', 'doctor', 'gateway', 'import',
    'install', 'logs', 'mcp', 'plugin', 'plugins', 'purge', 'rc', 'remote-control', 'respawn', 'rm',
    'setup-token', 'stop', 'kill', 'ultrareview', 'update', 'upgrade'
  if ($args.Count -gt 0 -and $subcommands -contains $args[0]) { & $real @args; return }

  $project = $null
  $rest = [System.Collections.Generic.List[object]]::new()
  for ($i = 0; $i -lt $args.Count; $i++) {
    $a = $args[$i]
    if ($a -eq '--') { for (; $i -lt $args.Count; $i++) { $rest.Add($args[$i]) }; break }
    elseif ($a -is [string] -and $a.StartsWith('--project=')) { $project = $a.Substring(10) }
    elseif ($a -eq '--project') {
      if ($i + 1 -ge $args.Count) { Write-Error 'claude: --project needs a project name'; return }
      $project = $args[++$i]
    }
    else { $rest.Add($a) }
  }
  foreach ($a in $rest) {
    if ($a -eq '--') { break }
    if ($a -in '--bg', '--background') {
      if ($project) { Write-Error 'claude: --project cannot reach a --bg session: start it from the panel'; return }
      & $real @rest; return
    }
    if ($a -in '-p', '--print') { & $real @rest; return }
  }

  $saved = $env:CCDASH_PROJECT
  try {
    if ($project) { $env:CCDASH_PROJECT = $project }
    & $real $flag 'plugin:ccdash@inline' --plugin-dir $plugin @rest
  } finally {
    $env:CCDASH_PROJECT = $saved
  }
}
