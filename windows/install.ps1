# Installs or updates ccdash on a Windows workstation: the binary, the ccdash plugin for every
# Claude Code session, Claude Code's hooks, start at logon; then restarts the collector. Safe to
# rerun: what is in place is kept, and an update touches only the binary, plugin and wrapper.
#
#   From the repo (builds first):   pwsh -File windows\install.ps1 -Build
#   Bundle for a machine with no toolchain (no token in it):
#                                   pwsh -File windows\install.ps1 -Build -Pack D:\ccdash-bundle
#   From such a bundle:             pwsh -File .\install.ps1
#   First install also needs:       -Hub wss://ccdash.example.org/collect -Token <token file>
#                                   [-HostName <name>]
#
# Everything beyond reporting and answering prompts is off until a switch turns it on, as with
# the Home Manager module. A switch adds what it needs where it is missing, on any run; what
# the config already says is kept (edit %APPDATA%\ccdash\config.toml to change it):
#   -Channel        replies from the panel: the `claude` wrapper and `[launch] channel`
#   -Terminal       the panel's Open: a background session attached in a console of its own
#   -Editor <exe>   the panel's Edit in the editor (`nvim`), in a console of its own
#   -Sync           ~\.claude in step with the other machines (`[sync] enable`)
#   -StatusLine     model, context, cost and limits per session (`ccdash tee`)
#   -Telemetry      Claude Code's OpenTelemetry export into the collector
#
# Run it as yourself (not elevated) in PowerShell 7. Running sessions keep working: their
# `ccdash channel` servers keep the old binary, which is renamed aside rather than overwritten.
param(
  [string]$Hub,
  [string]$Token,
  [string]$HostName,
  [switch]$Channel,
  [switch]$Terminal,
  [string]$Editor,
  [switch]$Sync,
  [switch]$StatusLine,
  [switch]$Telemetry,
  [switch]$Build,
  [string]$Pack
)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw 'Run it with PowerShell 7: pwsh -File install.ps1' }
function Say($text) { Write-Host "== $text" }
$utf8 = [Text.UTF8Encoding]::new($false)
function Write-Utf8($path, $text) { [IO.File]::WriteAllText($path, $text, $utf8) }

# Where from: a bundle (ccdash.exe next to this script) or the repo (this script in windows\)
$here = $PSScriptRoot
if (Test-Path "$here\ccdash.exe") {
  if ($Build) { throw '-Build works in the repo, not in a bundle' }
  $srcExe, $srcPlugin = "$here\ccdash.exe", "$here\claude-plugin"
  $rev = (Get-Content "$here\REV" -ErrorAction SilentlyContinue) ?? 'unknown'
} else {
  $repo = Split-Path $here
  $srcExe, $srcPlugin = "$repo\target\release\ccdash.exe", "$repo\claude-plugin"
  $rev = git -C $repo describe --always --dirty
  if ($Build) {
    if (-not (Get-Command dlltool -ErrorAction SilentlyContinue)) {
      # NOTE: the GNU toolchain needs MinGW's dlltool and gcc (WinLibs from winget)
      $mingw = Get-ChildItem "$env:LOCALAPPDATA\Microsoft\WinGet\Packages\BrechtSanders.WinLibs*\mingw64\bin" -Directory -ErrorAction SilentlyContinue | Select-Object -First 1
      if ($mingw) { $env:Path = "$($mingw.FullName);$env:Path" }
    }
    cargo build --release --manifest-path "$repo\Cargo.toml"
    if ($LASTEXITCODE) { throw 'cargo build failed' }
  }
  if (-not (Test-Path $srcExe)) { throw "no ${srcExe}: build it first (-Build)" }
}
$wrapperSrc = "$here\claude-channel.ps1"

if ($Pack) {
  New-Item -ItemType Directory -Force $Pack | Out-Null
  Copy-Item -Force $srcExe, $wrapperSrc, $PSCommandPath $Pack
  Remove-Item -Recurse -Force "$Pack\claude-plugin" -ErrorAction SilentlyContinue
  Copy-Item -Recurse $srcPlugin "$Pack\claude-plugin"
  Write-Utf8 "$Pack\REV" "$rev`n"
  Say "bundle of $rev in ${Pack}: copy it over, then run pwsh -File .\install.ps1 there"
  return
}

$bin = "$env:USERPROFILE\.local\bin"
$share = "$env:USERPROFILE\.local\share\ccdash"
$exe, $plugin = "$bin\ccdash.exe", "$share\claude-plugin"
$cfgDir = "$env:APPDATA\ccdash"
$claudeDir = $env:CLAUDE_CONFIG_DIR ?? "$env:USERPROFILE\.claude"

# 1. The binary: the running one is renamed aside (Windows lets a running exe be renamed, not
# overwritten); old copies go once nothing runs them any more.
New-Item -ItemType Directory -Force $bin, $share | Out-Null
if ((Test-Path $exe) -and (Get-FileHash $exe).Hash -eq (Get-FileHash $srcExe).Hash) {
  Say 'ccdash.exe unchanged'
} else {
  Copy-Item -Force $srcExe "$exe.new"
  if (Test-Path $exe) { Move-Item $exe "$exe.old-$(Get-Date -Format yyyyMMddHHmmss)" }
  Move-Item "$exe.new" $exe
  Say "ccdash.exe $rev in $bin"
}
Get-ChildItem "$exe.old-*" -ErrorAction SilentlyContinue | ForEach-Object { try { Remove-Item $_ } catch {} }
Write-Utf8 "$share\REV" "$rev`n"
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $bin) {
  [Environment]::SetEnvironmentVariable('Path', "$bin;$userPath".TrimEnd(';'), 'User')
  Say "$bin added to your PATH (new windows)"
}

# 2. The plugin, its MCP server by absolute path (Claude Code may not have ~\.local\bin on PATH).
# Linked into ~\.claude\skills, every session loads it as a personal plugin (Claude Code
# 2.1.157 or later): the knowledge tools and the ccdash skills, as the Home Manager module's
# `claudeCode.plugin.enable` does. The composer's `/` menu finds its skills there too.
Remove-Item -Recurse -Force "$plugin.new" -ErrorAction SilentlyContinue
Copy-Item -Recurse $srcPlugin "$plugin.new"
$mcp = ConvertFrom-Json -AsHashtable ([IO.File]::ReadAllText("$plugin.new\.mcp.json"))
$mcp.mcpServers.ccdash.command = $exe
Write-Utf8 "$plugin.new\.mcp.json" (($mcp | ConvertTo-Json -Depth 10) + "`n")
Remove-Item -Recurse -Force $plugin -ErrorAction SilentlyContinue
Move-Item "$plugin.new" $plugin
$link = "$claudeDir\skills\ccdash"
$item = Get-Item $link -Force -ErrorAction SilentlyContinue
if (-not $item) {
  New-Item -ItemType Directory -Force (Split-Path $link) | Out-Null
  # NOTE: a junction, as no symbolic link may be made without Developer Mode or elevation
  New-Item -ItemType Junction $link -Target $plugin | Out-Null
  Say "plugin in $share, loaded by every session ($link)"
} elseif ($item.LinkTarget -ne $plugin) {
  Say "$link is not ccdash's: sessions load what is there, not $plugin"
} else {
  Say "plugin in $share"
}

# 3. The collector's config: written at the first install, then the switches' keys where missing
$cfgPath = "$cfgDir\config.toml"
$claudeExe = "$env:USERPROFILE\.local\bin\claude.exe"
if (-not (Test-Path $claudeExe)) { $claudeExe = (Get-Command claude -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source }
function Lit($s) {
  if ($s -match "'") { throw "$s cannot be written into config.toml as is: edit it there" }
  "'$s'"
}
if (Test-Path $cfgPath) {
  $config = [IO.File]::ReadAllText($cfgPath)
} else {
  if (-not ($Hub -and $Token)) { throw 'first install: pass -Hub wss://<hub>/collect and -Token <token file>' }
  New-Item -ItemType Directory -Force $cfgDir | Out-Null
  Copy-Item $Token "$cfgDir\token"
  icacls "$cfgDir\token" /inheritance:r /grant:r "${env:USERNAME}:F" | Out-Null
  $config = @(
    "# ccdash collector, written by install.ps1 at the first install and yours to edit since"
    "# (docs/windows.md, docs/configuration.md). install.ps1's switches add their keys where missing."
    ($HostName ? "host = $(Lit $HostName)" : $null)
    ""
    "[hub]"
    "url = $(Lit $Hub)"
    "token_file = $(Lit "$cfgDir\token")"
    ""
    '# Claude Code by its full path, with the plugin: `--channels plugin:ccdash@inline` names it'
    "[launch]"
    "command = [$(Lit $claudeExe), '--plugin-dir', $(Lit $plugin)]"
  ) | Where-Object { $null -ne $_ }
  $config = ($config -join "`n") + "`n"
  Say "config and token (yours only) in $cfgDir"
}
$before = $config
# Adds `key = value` to [table] unless the table has the key (then it is kept). Enough for the
# flat tables of ccdash's config.
function Add-ConfigKey([string]$table, [string]$key, [string]$value) {
  $lines = [Collections.Generic.List[string]]::new([string[]]($script:config -split '\r?\n'))
  $header = -1
  for ($i = 0; $i -lt $lines.Count -and $header -lt 0; $i++) { if ($lines[$i].Trim() -eq "[$table]") { $header = $i } }
  if ($header -lt 0) {
    $script:config = $script:config.TrimEnd() + "`n`n[$table]`n$key = $value`n"
    Say "config: [$table] $key = $value"
    return
  }
  $end = $header + 1
  while ($end -lt $lines.Count -and $lines[$end].TrimStart() -notmatch '^\[') { $end++ }
  for ($i = $header + 1; $i -lt $end; $i++) {
    if ($lines[$i] -match "^\s*$([regex]::Escape($key))\s*=") {
      Say "config: kept [$table] $($lines[$i].Trim())"
      return
    }
  }
  $lines.Insert($header + 1, "$key = $value")
  $script:config = $lines -join "`n"
  Say "config: [$table] $key = $value"
}
if ($Channel) { Add-ConfigKey launch channel 'true' }
if ($Terminal) { Add-ConfigKey attach command "[$(Lit $claudeExe), 'attach', '{id}']" }
if ($Editor) { Add-ConfigKey edit command "[$(Lit $Editor), '{file}']" }
if ($Sync) { Add-ConfigKey sync enable 'true' }
if ($Telemetry) { Add-ConfigKey telemetry listen '["127.0.0.1:7839"]' }
if ($config -ne $before -or -not (Test-Path $cfgPath)) { Write-Utf8 $cfgPath $config }

# 4. The `claude` wrapper, which the profile dot-sources: sessions started in a terminal load
# the channel. Refreshed on every run once the profile loads it.
$profilePath = $PROFILE.CurrentUserCurrentHost
$profileText = (Test-Path $profilePath) ? [IO.File]::ReadAllText($profilePath) : ''
$wrapped = $profileText -match 'claude-channel\.ps1'
if ($Channel -or $wrapped) { Copy-Item -Force $wrapperSrc "$share\claude-channel.ps1" }
if ($Channel -and -not $wrapped) {
  New-Item -ItemType Directory -Force (Split-Path $profilePath) | Out-Null
  $line = "# ccdash: ``claude`` with the ccdash channel`n. `"`$env:USERPROFILE\.local\share\ccdash\claude-channel.ps1`"`n"
  Write-Utf8 $profilePath (($profileText.TrimEnd() + "`n`n" + $line).TrimStart())
  Say "profile loads the channel's wrapper: $profilePath"
  Say 'the channel needs allowedChannelPlugins in managed settings, else $env:CCDASH_CHANNEL_FLAG (docs/windows.md)'
}

# 5. Claude Code: ccdash's hooks (added where missing); the statusline and telemetry with their
# switches
$settingsPath = "$claudeDir\settings.json"
$before = (Test-Path $settingsPath) ? [IO.File]::ReadAllText($settingsPath) : '{}'
$settings = (ConvertFrom-Json -AsHashtable $before) ?? [ordered]@{}
$changed = 0
$tools = 'PreToolUse', 'PostToolUse', 'PostToolUseFailure', 'PermissionRequest', 'PermissionDenied'
$events = $tools + 'SessionStart', 'SessionEnd', 'UserPromptSubmit', 'Notification', 'Stop',
  'StopFailure', 'PostCompact', 'SubagentStart', 'SubagentStop', 'TaskCreated', 'TaskCompleted',
  'MessageDisplay'
if (-not $settings.Contains('hooks')) { $settings.hooks = [ordered]@{} }
foreach ($event in $events) {
  $existing = @($settings.hooks[$event] | Where-Object { $_ })
  if ($existing | Where-Object { @($_.hooks.command) -match '^ccdash (emit|ask)' }) { continue }
  $hook = if ($event -eq 'PermissionRequest') {
    [ordered]@{ type = 'command'; command = 'ccdash ask'; timeout = 86400 }
  } else {
    [ordered]@{ type = 'command'; command = 'ccdash emit'; timeout = 5 }
  }
  $entry = [ordered]@{}
  if ($tools -contains $event) { $entry.matcher = '*' }
  $entry.hooks = @($hook)
  $settings.hooks[$event] = $existing + $entry
  $changed++
}
if ($StatusLine) {
  if (-not $settings.Contains('statusLine')) {
    $settings.statusLine = [ordered]@{ type = 'command'; command = 'ccdash tee'; padding = 0 }; $changed++
  } elseif ("$($settings.statusLine.command)" -notmatch '^ccdash tee') {
    $settings.statusLine.command = "ccdash tee -- $($settings.statusLine.command)"; $changed++
  }
}
if ($Telemetry) {
  if (-not $settings.Contains('env')) { $settings.env = [ordered]@{} }
  $otel = [ordered]@{
    CLAUDE_CODE_ENABLE_TELEMETRY = '1'; OTEL_METRICS_EXPORTER = 'otlp'; OTEL_LOGS_EXPORTER = 'otlp'
    OTEL_EXPORTER_OTLP_PROTOCOL = 'http/json'; OTEL_EXPORTER_OTLP_ENDPOINT = 'http://127.0.0.1:7839'
    OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE = 'delta'
  }
  foreach ($k in $otel.Keys) { if (-not $settings.env.Contains($k)) { $settings.env[$k] = $otel[$k]; $changed++ } }
}
if ($changed) {
  New-Item -ItemType Directory -Force $claudeDir | Out-Null
  if (Test-Path $settingsPath) { Copy-Item -Force $settingsPath "$settingsPath.before-ccdash" }
  Write-Utf8 $settingsPath (($settings | ConvertTo-Json -Depth 20) + "`n")
  Say "Claude Code settings: $changed additions (backup: settings.json.before-ccdash)"
}

# 6. Start at logon, and (re)start the collector now
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
if (-not (Get-ItemProperty $run -Name ccdash-collector -ErrorAction SilentlyContinue)) {
  Set-ItemProperty $run ccdash-collector "`"$exe`" collector --background"
  Say 'collector starts at logon (Run key ccdash-collector)'
}
$old = @(Get-CimInstance Win32_Process -Filter "Name='ccdash.exe'" | Where-Object CommandLine -Match '\scollector(\s|$)')
foreach ($p in $old) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
foreach ($p in $old) { Wait-Process -Id $p.ProcessId -Timeout 10 -ErrorAction SilentlyContinue }
if ($old) { Say "collector stopped (pid $($old.ProcessId -join ', '))" }
# NOTE: not `& $exe`: the collector would inherit this script's output handles, and whatever
# reads them (a pipe, a tool) would wait for the collector to exit
$start = Start-Process $exe 'collector', '--background' -WindowStyle Hidden -PassThru
$start.WaitForExit()
if ($start.ExitCode) { throw "the collector did not start: see $env:LOCALAPPDATA\ccdash\state\collector.log" }
$log = "$env:LOCALAPPDATA\ccdash\state\collector.log"
foreach ($i in 1..20) {
  if (Select-String -Path $log -Pattern 'connected to the hub' -Quiet -ErrorAction SilentlyContinue) { Say "ccdash $rev up and connected"; return }
  Start-Sleep -Seconds 1
}
Get-Content $log -Tail 5 | ForEach-Object { "   $_" }
Say 'not connected to the hub yet (off the hub''s network?); it keeps trying and queues meanwhile'
