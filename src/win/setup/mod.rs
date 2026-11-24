//! `ccdash win setup`: ccdash for this user on a Windows workstation
//! (docs/windows.md). The binary on PATH, the ccdash plugin for every
//! Claude Code session, Claude Code's hooks, the tray at logon; then the
//! tray and its collector start anew. The binary carries what it installs,
//! so one downloaded file is the whole of it.
//!
//! Safe to run again: what is in place is kept, and the run of a newer
//! build puts its binary, plugin and wrapper in place. Everything beyond
//! reporting and answering prompts is off until a switch turns it on, as
//! with the Home Manager module; a switch adds what it needs where it is
//! missing, and what the config already says is kept.
//!
//! `ccdash win setup --remove` takes it all out again, the configuration and
//! the state excepted. Windows runs it for Uninstall in its list of
//! installed apps, where setup enters ccdash.

mod keys;
mod settings;

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};

use crate::{
    config::CollectorConfig,
    fsx, paths,
    sync::collector::default_root,
    win::{
        self,
        tray::{self, update},
    },
};

/// What `ccdash win setup` is asked for.
#[derive(clap::Args, Debug, Default)]
pub struct Options {
    /// The hub's `/collect` address, as `wss://ccdash.example.org/collect`
    /// (the first install).
    #[arg(long, value_name = "URL")]
    pub hub: Option<String>,
    /// The file with this machine's collector token; it is copied next to
    /// the config (the first install).
    #[arg(long, value_name = "FILE")]
    pub token: Option<PathBuf>,
    /// The name this machine reports, when not its DNS host name (the
    /// first install).
    #[arg(long, value_name = "NAME")]
    pub host: Option<String>,
    /// Replies from the panel: `[launch] channel`, and the `claude`
    /// wrapper in the PowerShell profile.
    #[arg(long)]
    pub channel: bool,
    /// The panel's Open: a background session attached in a console of
    /// its own (`[attach] command`).
    #[arg(long)]
    pub terminal: bool,
    /// The panel's Edit, in this editor (`nvim`) in a console of its own
    /// (`[edit] command`).
    #[arg(long, value_name = "PROGRAM")]
    pub editor: Option<String>,
    /// `~\.claude` in step with the other machines (`[sync] enable`).
    #[arg(long)]
    pub sync: bool,
    /// Where this machine keeps the hub's relative workspaces
    /// (`[workspace] root`).
    #[arg(long, value_name = "DIR")]
    pub root: Option<String>,
    /// Model, context, cost and limits per session: `ccdash tee` as
    /// Claude Code's statusline, in front of yours.
    #[arg(long)]
    pub status_line: bool,
    /// Claude Code's OpenTelemetry export into the collector.
    #[arg(long)]
    pub telemetry: bool,
    /// Only what a new build brings with it, where this binary runs from:
    /// its plugin, its wrapper, hooks it may have added. Nothing is
    /// registered, configured or restarted (the tray's updates).
    #[arg(long, hide = true, conflicts_with = "remove")]
    pub refresh: bool,
    /// Take ccdash out again: the tray and the collector, the start at
    /// logon, the hooks, the plugin, the binary. The configuration, the
    /// token and the state stay (Uninstall in Windows' installed apps).
    #[arg(long)]
    pub remove: bool,
    /// With `--remove`: the configuration, the token and the state too.
    #[arg(long, requires = "remove")]
    pub purge: bool,
}

/// The ccdash plugin as the repository has it, by its paths in the plugin.
const PLUGIN: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../../claude-plugin/.claude-plugin/plugin.json"),
    ),
    (
        ".mcp.json",
        include_str!("../../../claude-plugin/.mcp.json"),
    ),
    (
        "skills/project/SKILL.md",
        include_str!("../../../claude-plugin/skills/project/SKILL.md"),
    ),
    (
        "skills/report/SKILL.md",
        include_str!("../../../claude-plugin/skills/report/SKILL.md"),
    ),
    (
        "skills/report/reference.md",
        include_str!("../../../claude-plugin/skills/report/reference.md"),
    ),
    (
        "skills/report/template.html",
        include_str!("../../../claude-plugin/skills/report/template.html"),
    ),
    (
        "skills/wrap-up/SKILL.md",
        include_str!("../../../claude-plugin/skills/wrap-up/SKILL.md"),
    ),
];
/// `claude` with the ccdash channel, for the PowerShell profile.
const WRAPPER: &str = include_str!("../../../windows/claude-channel.ps1");
const WRAPPER_NAME: &str = "claude-channel.ps1";
/// What the profile gets to load the wrapper.
const PROFILE_LINES: &str = "# ccdash: `claude` with the ccdash channel\n. \"$env:USERPROFILE\\.local\\share\\ccdash\\claude-channel.ps1\"\n";
/// Where the user's programs that start at logon are listed.
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// ccdash's entry among the apps Windows lists as installed for the user.
const LISTED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ccdash";
/// The icon of that entry, next to the plugin, and the sizes it holds.
const ICON_NAME: &str = "ccdash.ico";
const ICON_SIZES: [usize; 6] = [16, 20, 24, 32, 48, 64];
/// How long a restart waits for the collector to reach its hub.
const CONNECT_WAIT: Duration = Duration::from_secs(20);

fn say(text: impl std::fmt::Display) {
    println!("{text}");
}

/// Where everything goes.
struct Places {
    home: PathBuf,
    /// `~\.local\bin\ccdash.exe`.
    exe: PathBuf,
    /// `~\.local\share\ccdash`.
    share: PathBuf,
    /// The plugin in `share`.
    plugin: PathBuf,
    /// The collector's `config.toml`.
    config: PathBuf,
    /// Claude Code's directory.
    claude: PathBuf,
}

impl Places {
    fn of(home: PathBuf) -> Self {
        let share = home.join(".local").join("share").join("ccdash");
        Self {
            exe: home.join(".local").join("bin").join("ccdash.exe"),
            plugin: share.join("claude-plugin"),
            share,
            config: paths::config_file(),
            claude: default_root(),
            home,
        }
    }
}

/// A file as a program on any system wrote it: line ends as `\n`.
fn unix_lines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The text of a file whatever PowerShell or an editor saved it as: UTF-8
/// with or without its mark, or UTF-16 with one.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |bytes: &[u8], unit: fn([u8; 2]) -> u16| {
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| unit(*pair))
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xef, 0xbb, 0xbf, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        [0xff, 0xfe, rest @ ..] => utf16(rest, u16::from_le_bytes),
        [0xfe, 0xff, rest @ ..] => utf16(rest, u16::from_be_bytes),
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The text of `path`; none when there is no such file.
fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(decode(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes `text` to `path` through a file next to it, so that nothing
/// reads half of it; the directory is made. A link (settings or a profile
/// kept in a dotfiles repository) is written through instead: a file
/// renamed over it would stand where the link was.
fn write(path: &Path, text: &str) -> Result<()> {
    let written = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path
            .symlink_metadata()
            .is_ok_and(|held| held.file_type().is_symlink())
        {
            return std::fs::write(path, text);
        }
        let mut name = path.as_os_str().to_owned();
        name.push(".ccdash-new");
        let new = PathBuf::from(name);
        std::fs::write(&new, text)?;
        std::fs::rename(&new, path)
    };
    written().with_context(|| format!("writing {}", path.display()))
}

/// Writes `text` to `path` unless that is what it holds; whether it wrote.
fn write_changed(path: &Path, text: &str) -> Result<bool> {
    if std::fs::read(path).is_ok_and(|held| held == text.as_bytes()) {
        return Ok(false);
    }
    write(path, text)?;
    Ok(true)
}

/// Whether two paths name the same place, as Windows compares names.
fn same_place(a: &Path, b: &Path) -> bool {
    let plain = |p: &Path| {
        p.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    plain(a) == plain(b)
}

/// Takes a browser download's mark off `file`: its `Zone.Identifier`
/// stream, which a copy takes along. Explorer starts the installed binary
/// at every logon, and asks about a marked file or does not start it.
fn unmark(file: &Path) {
    let mut mark = file.as_os_str().to_owned();
    mark.push(":Zone.Identifier");
    let _ = std::fs::remove_file(mark);
}

/// 1. The binary: this executable where PATH finds it. The one there may
///    be running (the collector, each session's `ccdash channel`), so it
///    goes aside instead of being written over.
fn binary(places: &Places) -> Result<()> {
    let me = std::env::current_exe()?;
    let build = update::this_build();
    let in_place = match (me.canonicalize(), places.exe.canonicalize()) {
        (Ok(me), Ok(there)) => me == there,
        _ => false,
    };
    if in_place {
        say(format_args!("{build} is in place"));
    } else if places.exe.exists() && update::hash(&me)? == update::hash(&places.exe)? {
        say(format_args!("{build}: unchanged"));
    } else {
        let bin = places.exe.parent().context("no directory for the binary")?;
        std::fs::create_dir_all(bin).with_context(|| format!("creating {}", bin.display()))?;
        let mut new = places.exe.as_os_str().to_owned();
        new.push(".new");
        let new = PathBuf::from(new);
        std::fs::copy(&me, &new).with_context(|| format!("writing {}", new.display()))?;
        unmark(&new);
        update::replace(&places.exe, &new)?;
        say(format_args!("{build} in {}", bin.display()));
    }
    update::sweep(&places.exe);
    Ok(())
}

/// `%NAME%`s of an entry of PATH as the registry holds it, expanded.
fn expanded(entry: &str) -> String {
    let mut out = String::new();
    let mut rest = entry;
    while let Some((before, after)) = rest.split_once('%') {
        out.push_str(before);
        match after.split_once('%') {
            Some((name, tail)) if !name.is_empty() => {
                match std::env::var(name) {
                    Ok(value) => out.push_str(&value),
                    Err(_) => out.push_str(&format!("%{name}%")),
                }
                rest = tail;
            }
            _ => {
                out.push('%');
                rest = after;
            }
        }
    }
    out + rest
}

/// The user's PATH with `dir` in front, unless it holds `dir` already.
fn path_with(path: &str, dir: &Path) -> Option<String> {
    let held = path
        .split(';')
        .any(|entry| same_place(Path::new(&expanded(entry.trim())), dir));
    if held {
        return None;
    }
    let rest = path.trim_matches(';');
    Some(if rest.is_empty() {
        dir.display().to_string()
    } else {
        format!("{};{rest}", dir.display())
    })
}

/// 2. PATH: the hooks in Claude Code's settings run `ccdash`, by name.
fn on_path(places: &Places) -> Result<()> {
    let bin = places.exe.parent().context("no directory for the binary")?;
    // NOTE: a PATH that is there and cannot be read is not an empty one to write over
    let (path, expands) = win::user_registry_string("Environment", "Path")
        .context("reading your PATH (HKCU\\Environment); it was left as it is")?
        .unwrap_or((String::new(), true));
    if let Some(path) = path_with(&path, bin) {
        win::set_user_registry_string("Environment", "Path", &path, expands)
            .context("adding to the user's PATH")?;
        win::environment_changed();
        say(format_args!(
            "{} added to your PATH (for new windows)",
            bin.display()
        ));
    }
    Ok(())
}

/// The plugin's `.mcp.json` with its server started by the full path of
/// the binary: Claude Code may run without `~\.local\bin` on its PATH.
fn mcp_json(text: &str, exe: &Path) -> Result<String> {
    let mut json: settings::Json = serde_json::from_str(text).context("the plugin's .mcp.json")?;
    let command = json
        .member("mcpServers", settings::Json::Null)
        .and_then(|servers| servers.member("ccdash", settings::Json::Null))
        .and_then(|server| server.member("command", settings::Json::Null))
        .context("the plugin's .mcp.json names no ccdash server")?;
    *command = settings::Json::String(exe.to_string_lossy().into_owned());
    settings::to_text(&json)
}

/// The files under `dir`, links not followed.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files_under(&entry.path(), out),
            Ok(_) => out.push(entry.path()),
            Err(_) => {}
        }
    }
}

/// 3. The plugin: the files this binary carries, the server of its
///    `.mcp.json` started by the binary's full path. What an earlier
///    build's plugin had and this one has not goes.
fn plugin(places: &Places) -> Result<()> {
    let mut wanted = Vec::new();
    for (name, text) in PLUGIN {
        let mut text = unix_lines(text);
        if *name == ".mcp.json" {
            text = mcp_json(&text, &places.exe)?;
        }
        let path = name
            .split('/')
            .fold(places.plugin.clone(), |path, part| path.join(part));
        write_changed(&path, &text)?;
        wanted.push(path);
    }
    // NOTE: what an earlier build's plugin had and this one has not
    let mut held = Vec::new();
    files_under(&places.plugin, &mut held);
    for stale in held.iter().filter(|file| !wanted.contains(file)) {
        let _ = std::fs::remove_file(stale);
        if let Some(dir) = stale.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
    Ok(())
}

/// 4. The plugin's link in Claude Code's skills: every session loads the
///    plugin from there as a personal plugin (Claude Code 2.1.157 or
///    later), the knowledge tools and the ccdash skills, as the Home
///    Manager module's `claudeCode.plugin.enable` does.
fn plugin_link(places: &Places) -> Result<()> {
    let link = places.claude.join("skills").join("ccdash");
    let dir = places.share.display();
    if link.symlink_metadata().is_err() {
        let skills = link.parent().context("no skills directory")?;
        std::fs::create_dir_all(skills)
            .with_context(|| format!("creating {}", skills.display()))?;
        // NOTE: a junction, as no symbolic link may be made without Developer Mode or elevation
        win::junction(&link, &places.plugin)
            .with_context(|| format!("linking {}", link.display()))?;
        say(format_args!(
            "plugin in {dir}, loaded by every session ({})",
            link.display()
        ));
    } else if std::fs::read_link(&link).is_ok_and(|target| same_place(&target, &places.plugin)) {
        say(format_args!("plugin in {dir}"));
    } else {
        say(format_args!(
            "plugin in {dir}; {} is not ccdash's: sessions load what is there",
            link.display()
        ));
    }
    Ok(())
}

/// Claude Code by its full path: the native installer's, else what PATH
/// has (npm's `claude.cmd`).
fn claude(places: &Places) -> Result<String> {
    let native = places.home.join(".local").join("bin").join("claude.exe");
    let found = if native.exists() {
        Some(native)
    } else {
        win::find_program("claude")
    };
    let found = found.context("Claude Code (`claude`) is not on PATH: install it first")?;
    Ok(found.to_string_lossy().into_owned())
}

/// The hub's address and the token's file of a first install.
///
/// # Errors
///
/// One of them is missing, or cannot be what it should be.
fn first_install<'a>(places: &Places, options: &'a Options) -> Result<(&'a str, &'a Path)> {
    let (Some(hub), Some(token)) = (&options.hub, &options.token) else {
        bail!(
            "no config yet at {}: a first install needs --hub wss://<hub>/collect and --token <token file>",
            places.config.display()
        );
    };
    if !hub.starts_with("ws://") && !hub.starts_with("wss://") {
        bail!("--hub is the collectors' WebSocket address: wss://<hub>/collect");
    }
    if !token.is_file() {
        bail!("--token: {} is not a file", token.display());
    }
    Ok((hub, token))
}

/// The config of a first install, its token copied next to it.
fn first_config(places: &Places, options: &Options) -> Result<String> {
    let (hub, token) = first_install(places, options)?;
    let dir = places
        .config
        .parent()
        .context("no directory for the config")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let kept = dir.join("token");
    if !same_place(token, &kept) {
        std::fs::copy(token, &kept).with_context(|| format!("reading {}", token.display()))?;
    }
    fsx::set_mode(&kept, 0o600).with_context(|| format!("closing {}", kept.display()))?;
    let mut lines = vec![
        "# ccdash collector, written by `ccdash win setup` at the first install and yours to edit since".to_owned(),
        "# (docs/windows.md, docs/configuration.md). The switches of `ccdash win setup` add their keys".to_owned(),
        "# where missing.".to_owned(),
    ];
    if let Some(host) = &options.host {
        lines.push(format!("host = {}", keys::literal(host)?));
    }
    lines.extend([
        String::new(),
        "[hub]".to_owned(),
        format!("url = {}", keys::literal(hub)?),
        format!("token_file = {}", keys::literal(&kept.to_string_lossy())?),
        String::new(),
        "# Claude Code by its full path, with the plugin: `--channels plugin:ccdash@inline` names it".to_owned(),
        "[launch]".to_owned(),
        format!(
            "command = {}",
            keys::list(&[
                &claude(places)?,
                "--plugin-dir",
                &places.plugin.to_string_lossy()
            ])?
        ),
    ]);
    say(format_args!(
        "config and token (yours alone to read) in {}",
        dir.display()
    ));
    Ok(lines.join("\n") + "\n")
}

/// 5. The collector's config: written at the first install, then the
///    switches' keys where they are missing.
fn config(places: &Places, options: &Options) -> Result<()> {
    let existing = match std::fs::read_to_string(&places.config) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).with_context(|| format!("reading {}", places.config.display()));
        }
    };
    if existing.is_some() && (options.hub.is_some() || options.token.is_some()) {
        say(format_args!(
            "config: kept; the hub and the token are changed in {}",
            places.config.display()
        ));
    }
    let mut text = match &existing {
        Some(text) => text.clone(),
        None => first_config(places, options)?,
    };
    let mut add = |table: &str, key: &str, value: &str| {
        let (with, added) = keys::add(&text, table, key, value);
        text = with;
        match added {
            keys::Added::Now(what) => say(format_args!("config: {what}")),
            keys::Added::Kept(line) => say(format_args!("config: kept [{table}] {line}")),
        }
    };
    if options.channel {
        add("launch", "channel", "true");
    }
    if options.terminal {
        add(
            "attach",
            "command",
            &keys::list(&[&claude(places)?, "attach", "{id}"])?,
        );
    }
    if let Some(editor) = &options.editor {
        add("edit", "command", &keys::list(&[editor, "{file}"])?);
    }
    if options.sync {
        add("sync", "enable", "true");
    }
    if let Some(root) = &options.root {
        add("workspace", "root", &keys::literal(root)?);
    }
    if options.telemetry {
        add("telemetry", "listen", "['127.0.0.1:7839']");
    }
    if existing.as_ref() != Some(&text) {
        // NOTE: not a config the collector would then refuse to start with
        toml::from_str::<CollectorConfig>(&text).with_context(|| {
            format!(
                "{} would not load with these keys; nothing was written",
                places.config.display()
            )
        })?;
        write(&places.config, &text)?;
    }
    Ok(())
}

/// PowerShell 7's profile of the console, as PowerShell itself names it.
fn powershell_profile() -> Option<PathBuf> {
    // NOTE: into a pipe PowerShell writes in the console's code page unless told otherwise,
    // and a profile under `C:\Users\Jiří` or `OneDrive\Документы` would come back as another path
    let output = Command::new(win::find_program("pwsh")?)
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
        .arg("[Console]::OutputEncoding = [Text.UTF8Encoding]::new(); $PROFILE.CurrentUserCurrentHost")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !path.is_empty()).then(|| PathBuf::from(path))
}

/// A profile that loads the wrapper; none when it does already.
fn profile_with_wrapper(profile: &str) -> Option<String> {
    if profile.contains(WRAPPER_NAME) {
        return None;
    }
    let before = unix_lines(profile);
    Some(if before.trim().is_empty() {
        PROFILE_LINES.to_owned()
    } else {
        format!("{}\n\n{PROFILE_LINES}", before.trim_end())
    })
}

/// 6. The `claude` wrapper, which the PowerShell profile loads: sessions
///    started in a terminal get the channel. Refreshed on every run once
///    it is installed.
fn wrapper(places: &Places, options: &Options) -> Result<()> {
    let file = places.share.join(WRAPPER_NAME);
    if !options.channel && !file.exists() {
        return Ok(());
    }
    write_changed(&file, &unix_lines(WRAPPER))?;
    if !options.channel {
        return Ok(());
    }
    let Some(profile) = powershell_profile() else {
        say(format_args!(
            "no PowerShell 7 (`pwsh`) on PATH: load {} from your shell's profile for `claude` with the channel",
            file.display()
        ));
        return Ok(());
    };
    if let Some(text) = profile_with_wrapper(&read(&profile)?.unwrap_or_default()) {
        write(&profile, &text)?;
        say(format_args!(
            "profile loads the channel's wrapper: {}",
            profile.display()
        ));
        say(
            "the channel needs `allowedChannelPlugins` in Claude Code's managed settings, else $env:CCDASH_CHANNEL_FLAG (docs/windows.md)",
        );
    }
    Ok(())
}

/// 7. Claude Code: ccdash's hooks where missing; the statusline and the
///    telemetry variables with their switches.
fn claude_settings(places: &Places, options: &Options) -> Result<()> {
    let path = places.claude.join("settings.json");
    let before = read(&path)?;
    let extras = settings::Extras {
        status_line: options.status_line,
        telemetry: options.telemetry,
    };
    let (text, added) = settings::merge(before.as_deref().unwrap_or_default(), extras)
        .with_context(|| format!("{} cannot take ccdash's hooks", path.display()))?;
    if added == 0 {
        return Ok(());
    }
    // NOTE: the file as it was before ccdash first touched it: a later run, or an update,
    // does not put a file with ccdash's hooks in its place
    let kept = path.with_file_name("settings.json.before-ccdash");
    let keeps = before.is_some() && !kept.exists();
    if keeps {
        std::fs::copy(&path, &kept).with_context(|| format!("writing {}", kept.display()))?;
    }
    write(&path, &text)?;
    say(format_args!(
        "Claude Code settings: {added} additions{}",
        if keeps {
            " (the file as it was: settings.json.before-ccdash)"
        } else {
            ""
        }
    ));
    Ok(())
}

/// 8. The tray at logon, which starts the collector: the `Run` key needs
///    no elevation (a scheduled task would).
fn at_logon(places: &Places) -> Result<()> {
    let wanted = format!("\"{}\" win tray", places.exe.display());
    // NOTE: what windows/install.ps1 set: the collector alone, without the tray
    win::delete_user_registry_value(RUN_KEY, "ccdash-collector")
        .context("removing the old start at logon")?;
    // NOTE: the value is ccdash's own: one that cannot be read is set anew like one that differs
    let held = win::user_registry_string(RUN_KEY, "ccdash")
        .ok()
        .flatten()
        .map(|(value, _)| value);
    if held.as_deref() != Some(&wanted) {
        win::set_user_registry_string(RUN_KEY, "ccdash", &wanted, false)
            .context("setting the start at logon")?;
        say("the tray and its collector start at logon (Run key `ccdash`)");
    }
    Ok(())
}

/// 9. Windows' list of installed apps (Settings › Apps): an entry of the
///    user's own under `key`, which takes no elevation, with the command
///    that removes ccdash again. With `only_there` an entry is renewed
///    (its version, after an update) and none is made.
fn listed(places: &Places, key: &str, only_there: bool) -> Result<()> {
    let there = win::user_registry_string(key, "DisplayName")
        .ok()
        .flatten()
        .is_some();
    if only_there && !there {
        return Ok(());
    }
    // NOTE: the binary has no icon of its own: the list shows this file, drawn as the tray's
    let icon = places.share.join(ICON_NAME);
    let image = tray::icon::ico(&ICON_SIZES, tray::icon::Badge::None);
    if !std::fs::read(&icon).is_ok_and(|held| held == image) {
        std::fs::create_dir_all(&places.share)
            .and_then(|()| std::fs::write(&icon, &image))
            .with_context(|| format!("writing {}", icon.display()))?;
    }
    let remove = format!("\"{}\" win setup --remove", places.exe.display());
    let authors = env!("CARGO_PKG_AUTHORS");
    let bin = places.exe.parent().unwrap_or(&places.exe);
    let texts = [
        ("DisplayName", "ccdash".to_owned()),
        ("DisplayVersion", crate::version().to_owned()),
        // NOTE: the name without the address after it
        (
            "Publisher",
            authors.split(" <").next().unwrap_or(authors).to_owned(),
        ),
        ("DisplayIcon", icon.display().to_string()),
        ("InstallLocation", bin.display().to_string()),
        ("UninstallString", remove.clone()),
        ("QuietUninstallString", remove),
        ("URLInfoAbout", env!("CARGO_PKG_REPOSITORY").to_owned()),
    ];
    let size = std::fs::metadata(&places.exe).map_or(0, |held| held.len() / 1024);
    let numbers = [
        ("NoModify", 1),
        ("NoRepair", 1),
        ("EstimatedSize", u32::try_from(size).unwrap_or(u32::MAX)),
    ];
    let set = || -> std::io::Result<()> {
        for (name, text) in &texts {
            win::set_user_registry_string(key, name, text, false)?;
        }
        for (name, number) in numbers {
            win::set_user_registry_number(key, name, number)?;
        }
        Ok(())
    };
    set().context("entering ccdash among the installed apps")?;
    if !there {
        say(
            "ccdash is in Windows' installed apps; Uninstall there runs `ccdash win setup --remove`",
        );
    }
    Ok(())
}

/// A profile without the lines that load the wrapper; none when it loads
/// none.
fn profile_without_wrapper(profile: &str) -> Option<String> {
    if !profile.contains(WRAPPER_NAME) {
        return None;
    }
    let comment = PROFILE_LINES.lines().next().unwrap_or_default();
    let kept: Vec<&str> = profile
        .lines()
        .filter(|line| {
            let line = line.trim();
            // NOTE: what setup wrote, not a line of the user's that names the file otherwise
            line != comment && !(line.starts_with(". ") && line.contains(WRAPPER_NAME))
        })
        .collect();
    let text = kept.join("\n");
    Some(match text.trim_end() {
        "" => String::new(),
        text => format!("{text}\n"),
    })
}

/// The binary and what updates left next to it: the files they moved aside
/// and a download that never got in place.
fn binaries(exe: &Path) -> Vec<PathBuf> {
    let mut files = vec![exe.to_owned()];
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name().and_then(|n| n.to_str())) else {
        return files;
    };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file = entry.file_name();
        let rest = file.to_str().and_then(|file| file.strip_prefix(name));
        if rest.is_some_and(|rest| rest.starts_with(".old-") || rest == ".new") {
            files.push(entry.path());
        }
    }
    files
}

/// Takes a file of ccdash's away: deleted, or, while a process still runs
/// it (this one, a session's `ccdash channel`), moved into the temporary
/// folder, which Windows clears in its own time. Where it went, when moved.
fn take_away(file: &Path, name: &str) -> std::io::Result<Option<PathBuf>> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => {
            let to = std::env::temp_dir().join(name);
            std::fs::rename(file, &to)?;
            Ok(Some(to))
        }
    }
}

/// Removes a directory with what it holds; one that is not there is
/// removed already.
fn gone(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", dir.display())),
    }
}

/// ccdash's part of Claude Code's settings, taken out again.
fn settings_removed(places: &Places) -> Result<()> {
    let path = places.claude.join("settings.json");
    let Some(before) = read(&path)? else {
        return Ok(());
    };
    let (text, removed) = settings::unmerge(&before)
        .with_context(|| format!("{} cannot be read for ccdash's hooks", path.display()))?;
    if removed > 0 {
        write(&path, &text)?;
        say(format_args!(
            "Claude Code settings: ccdash's {removed} entries are out"
        ));
    }
    // NOTE: the file as it was before the install that ends here: a later install keeps the
    // file as it is then, not this one
    let _ = std::fs::remove_file(path.with_file_name("settings.json.before-ccdash"));
    Ok(())
}

/// Leaves `text` in `remove.log` of the state directory, when there is
/// one: Windows starts an uninstall with no console to say it on.
fn noted(text: &str) {
    let dir = paths::state_dir();
    if dir.is_dir() {
        let _ = std::fs::write(dir.join("remove.log"), format!("{text}\n"));
    }
}

/// The profile no longer loads the wrapper, which is about to go.
fn profile_removed() -> Result<()> {
    let Some(profile) = powershell_profile() else {
        return Ok(());
    };
    let without = read(&profile)?.as_deref().and_then(profile_without_wrapper);
    if let Some(text) = without {
        write(&profile, &text)?;
        say(format_args!(
            "{} no longer loads the wrapper",
            profile.display()
        ));
    }
    Ok(())
}

/// The plugin's link in Claude Code's skills, when it is ccdash's, and then
/// what ccdash keeps next to the plugin.
fn plugin_removed(places: &Places) -> Result<()> {
    let link = places.claude.join("skills").join("ccdash");
    if std::fs::read_link(&link).is_ok_and(|target| same_place(&target, &places.plugin)) {
        std::fs::remove_dir(&link).with_context(|| format!("removing {}", link.display()))?;
    }
    gone(&places.share)
}

/// The configuration with its token, and the state. The runtime directory
/// may be one the user named (`CCDASH_RUNTIME_DIR`): of that, only what
/// ccdash put there.
fn purged(places: &Places) -> Result<()> {
    let config = places
        .config
        .parent()
        .context("no directory for the config")?;
    let (state, runtime) = (paths::state_dir(), paths::runtime_dir());
    gone(config)?;
    gone(&state)?;
    gone(&runtime.join("titles"))?;
    let _ = std::fs::remove_dir(&runtime);
    // NOTE: `%LOCALAPPDATA%\ccdash`, when the two were all it held
    if let Some(parent) = state.parent()
        && parent.file_name().is_some_and(|name| name == "ccdash")
    {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// `ccdash win setup --remove`: what setup put in place. First what calls
/// ccdash, Claude Code's hooks and the profile's line: while those cannot
/// be taken out, the binary they call stays, and the entry among the
/// installed apps with it, to try again from. Past that it goes on over
/// what it cannot remove, and names it at the end, in `remove.log` too.
/// `~\.local\bin` stays on PATH: Claude Code lives there too.
fn remove(places: &Places, purge: bool) -> Result<()> {
    if let Err(e) = settings_removed(places).and_then(|()| profile_removed()) {
        let why =
            format!("ccdash is not removed: what calls it could not be taken out first ({e:#})");
        noted(&why);
        bail!(why);
    }
    let mut left: Vec<String> = Vec::new();
    let mut tried = |what: &str, done: Result<()>| {
        if let Err(e) = done {
            left.push(format!("{what}: {e:#}"));
        }
    };
    // NOTE: the tray first, or it starts the collector again
    let stopped = [tray::end_tray(), tray::end_collector()];
    if stopped.iter().any(Option::is_some) {
        say("the tray and the collector are stopped");
    }
    let at_logon = ["ccdash", "ccdash-collector"]
        .into_iter()
        .try_for_each(|value| win::delete_user_registry_value(RUN_KEY, value));
    tried("the start at logon", at_logon.map_err(Into::into));
    tried("the plugin", plugin_removed(places));
    tried(
        "the entry among the installed apps",
        win::delete_user_registry_key(LISTED_KEY).map_err(Into::into),
    );
    if purge {
        tried("the configuration and the state", purged(places));
    }
    let stamp = crate::util::now_ms();
    for (n, file) in binaries(&places.exe).iter().enumerate() {
        match take_away(file, &format!("ccdash-removed-{stamp}-{n}.exe")) {
            Ok(None) => {}
            Ok(Some(to)) => say(format_args!(
                "{} is still run by a process: moved to {}, where Windows clears it away",
                file.display(),
                to.display()
            )),
            Err(e) => tried(
                "the binary",
                Err(e).with_context(|| format!("{} is in use; delete it later", file.display())),
            ),
        }
    }
    say("ccdash is removed. Sessions that are open call its hooks until they are started again.");
    if !purge {
        let config = places.config.parent().unwrap_or(&places.config);
        say(format_args!(
            "kept, for a later install: the configuration and its token in {}, the state and the logs in {}",
            config.display(),
            paths::state_dir().display()
        ));
    }
    if !left.is_empty() {
        let why = format!("not all of it could be removed:\n- {}", left.join("\n- "));
        noted(&why);
        bail!(why);
    }
    Ok(())
}

/// 10. What runs is ended, and the installed tray started: it starts the
///     collector. Sessions go on: their channels find the new collector.
fn restart(places: &Places) -> Result<()> {
    // NOTE: the tray first, or it starts the collector again
    let tray = tray::end_tray();
    let collector = tray::end_collector();
    if tray.is_some() || collector.is_some() {
        say("what ran of ccdash was stopped (the tray, the collector)");
    }
    let mut child = tray::start_detached(&places.exe, None, None)?;
    let log = paths::state_dir();
    let tries = CONNECT_WAIT.as_millis() / 500;
    for _ in 0..tries {
        std::thread::sleep(Duration::from_millis(500));
        // NOTE: its console start hands over to a tray without one and ends well; a tray that
        // cannot run ends badly
        if let Some(status) = child.try_wait()?
            && !status.success()
        {
            bail!(
                "the tray ended at once ({status}); see {}",
                log.join("tray.log").display()
            );
        }
        if tray::snapshot().is_some_and(|s| s.hosts.first().is_some_and(|host| host.connected)) {
            say(format_args!(
                "{} is up and connected; its icon is in the taskbar (under ^ until you drag it out)",
                update::this_build()
            ));
            return Ok(());
        }
    }
    if tray::snapshot().is_some() {
        say(format_args!(
            "not connected to the hub yet (off its network?): the collector keeps trying and queues meanwhile; its log is {}",
            log.join("collector.log").display()
        ));
    } else {
        say(format_args!(
            "the collector does not answer: it may not start with this configuration; its log is {}, the tray's {}",
            log.join("collector.log").display(),
            log.join("tray.log").display()
        ));
    }
    Ok(())
}

/// A variable that moves ccdash's files and is set for this process alone:
/// the tray that Explorer starts at logon will not have it, and will look
/// for the config and the state in the usual places.
fn only_here(name: &str) -> Option<String> {
    let here = std::env::var(name).ok().filter(|value| !value.is_empty())?;
    // NOTE: the registry may hold it with `%NAME%`s in it, which the process has expanded
    let kept = |value: Option<String>| {
        value.is_some_and(|kept| same_place(Path::new(&expanded(&kept)), Path::new(&here)))
    };
    let user = win::user_registry_string("Environment", name)
        .ok()
        .flatten()
        .map(|(value, _)| value);
    let machine = win::machine_environment(name);
    (!kept(user) && !kept(machine)).then_some(here)
}

/// Says so when the places this run uses are not the ones a logon will.
fn warn_of_places() {
    for name in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "CCDASH_RUNTIME_DIR"] {
        if let Some(value) = only_here(name) {
            say(format_args!(
                "warning: {name} ({value}) is set in this shell only. The tray started at logon will not see it, and will not find what this run put there: set it for your user (`setx {name} ...`) or run setup without it"
            ));
        }
    }
}

/// `ccdash win setup`.
///
/// # Errors
///
/// A first install lacks its hub or token: nothing is done then. Or a step
/// could not be done; the steps before it stay done, and a second run goes
/// on from there.
pub fn run(options: &Options) -> Result<()> {
    let home = paths::home().context("USERPROFILE is not set")?;
    let mut places = Places::of(home);
    if options.remove {
        return remove(&places, options.purge);
    }
    if options.refresh {
        // NOTE: an update put this binary where the old one ran, wherever that is; what the
        // user registered or took out again (PATH, the start at logon, the link) is theirs
        places.exe = std::env::current_exe()?;
        plugin(&places)?;
        wrapper(&places, options)?;
        claude_settings(&places, options)?;
        // NOTE: the version the list of installed apps shows, when ccdash is in it
        return listed(&places, LISTED_KEY, true);
    }
    // NOTE: asked before anything is put in place, not after half of it is
    if !places.config.exists() {
        first_install(&places, options)?;
    }
    binary(&places)?;
    on_path(&places)?;
    plugin(&places)?;
    plugin_link(&places)?;
    config(&places, options)?;
    wrapper(&places, options)?;
    claude_settings(&places, options)?;
    at_logon(&places)?;
    listed(&places, LISTED_KEY, false)?;
    // NOTE: what windows/install.ps1 kept of the installed commit; `ccdash --version` says it now
    let _ = std::fs::remove_file(places.share.join("REV"));
    warn_of_places();
    restart(&places)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_carries_every_file_of_the_plugin() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("claude-plugin");
        let mut files = Vec::new();
        files_under(&root, &mut files);
        let mut in_repo: Vec<String> = files
            .iter()
            .map(|file| {
                let inside = file.strip_prefix(&root).unwrap();
                inside.to_string_lossy().replace('\\', "/")
            })
            .collect();
        in_repo.sort();
        let mut carried: Vec<&str> = PLUGIN.iter().map(|(name, _)| *name).collect();
        carried.sort_unstable();
        assert_eq!(carried, in_repo, "add the new file to PLUGIN");
    }

    #[test]
    fn the_plugins_server_is_started_by_the_installed_binary() {
        let (_, text) = PLUGIN
            .iter()
            .find(|(name, _)| *name == ".mcp.json")
            .unwrap();
        let exe = Path::new(r"C:\Users\me\.local\bin\ccdash.exe");
        let json: serde_json::Value = serde_json::from_str(&mcp_json(text, exe).unwrap()).unwrap();
        assert_eq!(
            json["mcpServers"]["ccdash"],
            serde_json::json!({"command": r"C:\Users\me\.local\bin\ccdash.exe", "args": ["channel"]})
        );
        assert!(mcp_json("{}", exe).is_err() || mcp_json("[]", exe).is_err());
    }

    #[test]
    fn the_plugin_is_written_whole_and_what_it_no_longer_has_goes() {
        let dir = std::env::temp_dir().join(format!("ccdash-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut places = Places::of(dir.join("home"));
        places.claude = dir.join("home").join(".claude");
        let stale = places.plugin.join("skills").join("gone").join("SKILL.md");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, "from an earlier build").unwrap();
        plugin(&places).unwrap();
        for (name, text) in PLUGIN.iter().filter(|(name, _)| *name != ".mcp.json") {
            let held = std::fs::read_to_string(places.plugin.join(name)).unwrap();
            assert_eq!(held, unix_lines(text), "{name}");
        }
        assert!(!stale.parent().unwrap().exists(), "the stale skill went");
        plugin_link(&places).unwrap();
        let link = places.claude.join("skills").join("ccdash");
        assert!(link.join(".mcp.json").exists(), "sessions find the plugin");
        assert!(same_place(
            &std::fs::read_link(&link).unwrap(),
            &places.plugin
        ));
        plugin(&places).unwrap();
        plugin_link(&places).unwrap();
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_gets_the_directory_once_whatever_way_it_is_written() {
        let bin = Path::new(r"C:\Users\me\.local\bin");
        assert_eq!(
            path_with(r"C:\tools;C:\other", bin).as_deref(),
            Some(r"C:\Users\me\.local\bin;C:\tools;C:\other")
        );
        assert_eq!(
            path_with("", bin).as_deref(),
            Some(r"C:\Users\me\.local\bin")
        );
        assert_eq!(path_with(r"C:\tools;c:\users\ME\.local\bin\;", bin), None);
        // SAFETY: no other test reads this variable
        unsafe { std::env::set_var("CCDASH_TEST_HOME", r"C:\Users\me") };
        assert_eq!(path_with(r"%CCDASH_TEST_HOME%\.local\bin", bin), None);
        assert_eq!(
            expanded("100%;%NO_SUCH_CCDASH_VAR%\\x"),
            "100%;%NO_SUCH_CCDASH_VAR%\\x"
        );
    }

    #[test]
    fn a_profile_gets_the_wrapper_once_after_what_it_holds() {
        assert_eq!(profile_with_wrapper("").as_deref(), Some(PROFILE_LINES));
        let mine = "Set-Alias g git\r\n\r\n";
        let with = profile_with_wrapper(mine).unwrap();
        assert_eq!(with, format!("Set-Alias g git\n\n{PROFILE_LINES}"));
        assert_eq!(profile_with_wrapper(&with), None);
    }

    #[test]
    fn a_file_reads_whatever_powershell_saved_it_as() {
        assert_eq!(decode(b"plain"), "plain");
        assert_eq!(decode(b"\xef\xbb\xbfmarked"), "marked");
        assert_eq!(decode(b"\xff\xfeh\0i\0"), "hi");
        assert_eq!(decode(b"\xfe\xff\0h\0i"), "hi");
    }

    #[test]
    fn a_first_install_writes_a_config_the_collector_loads_and_switches_add_to_it() {
        let dir = std::env::temp_dir().join(format!("ccdash-setup-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let home = dir.join("home");
        std::fs::create_dir_all(home.join(".local").join("bin")).unwrap();
        std::fs::write(home.join(".local").join("bin").join("claude.exe"), "").unwrap();
        std::fs::write(dir.join("token file"), "secret\n").unwrap();
        let mut places = Places::of(home);
        places.config = dir.join("config").join("config.toml");
        assert!(
            config(&places, &Options::default()).is_err(),
            "a first install names its hub"
        );
        let first = Options {
            hub: Some("wss://hub.example.org/collect".into()),
            token: Some(dir.join("token file")),
            host: Some("winbox".into()),
            channel: true,
            ..Options::default()
        };
        config(&places, &first).unwrap();
        let load = || {
            let text = std::fs::read_to_string(&places.config).unwrap();
            toml::from_str::<CollectorConfig>(&text).unwrap()
        };
        let cfg = load();
        assert_eq!(cfg.host.as_deref(), Some("winbox"));
        assert_eq!(cfg.hub.url, "wss://hub.example.org/collect");
        let token = cfg.hub.token_file.unwrap();
        assert_eq!(std::fs::read_to_string(token).unwrap(), "secret\n");
        assert!(cfg.launch.channel);
        assert_eq!(
            cfg.launch.command[1..],
            [
                "--plugin-dir".to_owned(),
                places.plugin.display().to_string()
            ]
        );
        assert!(cfg.attach.command.is_empty() && !cfg.sync.enable);
        let later = Options {
            terminal: true,
            editor: Some("nvim".into()),
            sync: true,
            root: Some(r"D:\src".into()),
            telemetry: true,
            hub: Some("wss://another.example.org/collect".into()),
            ..Options::default()
        };
        config(&places, &later).unwrap();
        let cfg = load();
        assert_eq!(
            cfg.hub.url, "wss://hub.example.org/collect",
            "the hub is kept"
        );
        assert_eq!(
            cfg.attach.command[1..],
            ["attach".to_owned(), "{id}".to_owned()]
        );
        assert_eq!(cfg.edit.command, ["nvim", "{file}"]);
        assert!(cfg.sync.enable);
        let before = std::fs::read_to_string(&places.config).unwrap();
        config(&places, &later).unwrap();
        assert_eq!(std::fs::read_to_string(&places.config).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_profile_loses_the_wrapper_and_nothing_else() {
        let mine = "Set-Alias g git\n\n# mine\n";
        let with = profile_with_wrapper(mine).unwrap();
        assert_eq!(
            profile_without_wrapper(&with).as_deref(),
            Some("Set-Alias g git\n\n# mine\n")
        );
        assert_eq!(profile_without_wrapper(mine), None, "it loads none");
        assert_eq!(
            profile_without_wrapper(PROFILE_LINES).as_deref(),
            Some(""),
            "a profile that was setup's alone is left empty"
        );
        // NOTE: saved by an editor with other line ends, and a line of the user's that only
        // names the file
        let edited = format!(
            "# see {WRAPPER_NAME}\r\n{}Set-Alias g git\r\n",
            PROFILE_LINES.replace('\n', "\r\n")
        );
        assert_eq!(
            profile_without_wrapper(&edited).unwrap(),
            format!("# see {WRAPPER_NAME}\nSet-Alias g git\n")
        );
    }

    #[test]
    fn the_installed_apps_get_an_entry_that_removes_ccdash_again() {
        let dir = scratch("listed");
        let key = format!(r"Software\ccdash-test-listed-{}", std::process::id());
        let mut places = Places::of(dir.join("home"));
        places.exe = dir.join("home").join("bin").join("ccdash.exe");
        std::fs::create_dir_all(places.exe.parent().unwrap()).unwrap();
        std::fs::write(&places.exe, vec![0u8; 5000]).unwrap();
        let value = |name: &str| {
            win::user_registry_string(&key, name)
                .unwrap()
                .map(|(text, _)| text)
        };
        listed(&places, &key, true).unwrap();
        assert_eq!(value("DisplayName"), None, "an update makes no entry");
        listed(&places, &key, false).unwrap();
        assert_eq!(value("DisplayName").as_deref(), Some("ccdash"));
        assert_eq!(value("DisplayVersion").as_deref(), Some(crate::version()));
        assert_eq!(
            value("UninstallString"),
            Some(format!("\"{}\" win setup --remove", places.exe.display()))
        );
        let icon = PathBuf::from(value("DisplayIcon").unwrap());
        assert_eq!(icon, places.share.join(ICON_NAME));
        assert_eq!(std::fs::read(&icon).unwrap()[..4], [0, 0, 1, 0]);
        // NOTE: kept current by an update, once it is there
        win::set_user_registry_string(&key, "DisplayVersion", "an older one", false).unwrap();
        listed(&places, &key, true).unwrap();
        assert_eq!(value("DisplayVersion").as_deref(), Some(crate::version()));
        win::delete_user_registry_key(&key).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Set for the copy of this test binary that stands for a running ccdash.
    const HOLD: &str = "CCDASH_TEST_HOLD_SETUP";

    #[test]
    fn a_binary_that_still_runs_is_moved_out_of_the_way() {
        use std::io::Read;
        if std::env::var_os(HOLD).is_some() {
            // NOTE: the copy: runs until its stdin closes (or it is ended)
            let _ = std::io::stdin().read(&mut [0u8; 1]);
            return;
        }
        let dir = scratch("remove");
        let exe = dir.join("ccdash.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
        std::fs::write(dir.join("ccdash.exe.old-1"), "an earlier build").unwrap();
        std::fs::write(dir.join("ccdash.exe.new"), "a download").unwrap();
        std::fs::write(dir.join("claude.exe"), "not ccdash's").unwrap();
        let mut found = binaries(&exe);
        found.sort();
        assert_eq!(
            found,
            [
                exe.clone(),
                dir.join("ccdash.exe.new"),
                dir.join("ccdash.exe.old-1")
            ]
        );
        let mut running = Command::new(&exe)
            .args([
                "--exact",
                "win::setup::tests::a_binary_that_still_runs_is_moved_out_of_the_way",
                "--nocapture",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env(HOLD, "1")
            .spawn()
            .unwrap();
        let name = format!("ccdash-removed-test-{}.exe", std::process::id());
        let moved = take_away(&exe, &name).unwrap();
        assert_eq!(moved, Some(std::env::temp_dir().join(&name)));
        assert!(!exe.exists(), "nothing of it where it was installed");
        assert_eq!(
            take_away(&dir.join("ccdash.exe.old-1"), &name).unwrap(),
            None
        );
        assert!(
            !dir.join("ccdash.exe.old-1").exists(),
            "nothing runs it: deleted"
        );
        assert_eq!(take_away(&exe, &name).unwrap(), None, "gone already");
        running.kill().unwrap();
        running.wait().unwrap();
        for _ in 0..50 {
            if std::fs::remove_file(moved.as_ref().unwrap()).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ccdash-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_downloads_mark_does_not_reach_the_installed_binary() {
        let dir = scratch("mark");
        let (downloaded, installed) = (dir.join("ccdash-x86_64.exe"), dir.join("ccdash.exe.new"));
        let mark = |file: &Path| {
            let mut mark = file.as_os_str().to_owned();
            mark.push(":Zone.Identifier");
            PathBuf::from(mark)
        };
        std::fs::write(&downloaded, "the binary").unwrap();
        // NOTE: a file system without streams keeps no marks either
        if std::fs::write(mark(&downloaded), "[ZoneTransfer]\r\nZoneId=3\r\n").is_ok() {
            std::fs::copy(&downloaded, &installed).unwrap();
            assert!(
                std::fs::read(mark(&installed)).is_ok(),
                "a copy takes the mark along"
            );
            unmark(&installed);
            assert!(std::fs::read(mark(&installed)).is_err());
            assert_eq!(std::fs::read_to_string(&installed).unwrap(), "the binary");
            unmark(&installed);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_is_replaced_whole_and_a_link_written_through() {
        let dir = scratch("write");
        let plain = dir.join("deeper").join("settings.json");
        write(&plain, "one").unwrap();
        write(&plain, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&plain).unwrap(), "two");
        assert_eq!(
            std::fs::read_dir(plain.parent().unwrap()).unwrap().count(),
            1
        );
        let (real, link) = (dir.join("dotfiles.json"), dir.join("settings.json"));
        std::fs::write(&real, "{}").unwrap();
        // NOTE: a symbolic link takes Developer Mode or elevation: without, nothing to see here
        if std::os::windows::fs::symlink_file(&real, &link).is_ok() {
            write(&link, "through").unwrap();
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_to_string(&real).unwrap(), "through");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removal_takes_ccdashs_settings_out_and_the_old_copy_with_them() {
        let dir = scratch("unsettings");
        let mut places = Places::of(dir.join("home"));
        places.claude = dir.join("claude");
        let (settings, kept) = (
            places.claude.join("settings.json"),
            places.claude.join("settings.json.before-ccdash"),
        );
        std::fs::create_dir_all(&places.claude).unwrap();
        settings_removed(&places).unwrap();
        std::fs::write(&settings, "{\"model\": \"opus\"}\n").unwrap();
        claude_settings(&places, &Options::default()).unwrap();
        assert!(kept.exists());
        settings_removed(&places).unwrap();
        let left: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(left, serde_json::json!({"model": "opus"}));
        assert!(
            !kept.exists(),
            "a later install keeps the file as it is then"
        );
        // NOTE: settings that cannot be read may still name ccdash's hooks: the removal
        // stops there, before the binary they call goes
        std::fs::write(&settings, "{not json").unwrap();
        assert!(settings_removed(&places).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_settings_as_they_were_are_kept_once() {
        let dir = scratch("settings");
        let mut places = Places::of(dir.join("home"));
        places.claude = dir.join("claude");
        let (settings, kept) = (
            places.claude.join("settings.json"),
            places.claude.join("settings.json.before-ccdash"),
        );
        std::fs::create_dir_all(&places.claude).unwrap();
        std::fs::write(&settings, "{\"model\": \"opus\"}\n").unwrap();
        claude_settings(&places, &Options::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "{\"model\": \"opus\"}\n"
        );
        let with_line = Options {
            status_line: true,
            ..Options::default()
        };
        claude_settings(&places, &with_line).unwrap();
        assert!(
            std::fs::read_to_string(&settings)
                .unwrap()
                .contains("ccdash tee")
        );
        assert_eq!(
            std::fs::read_to_string(&kept).unwrap(),
            "{\"model\": \"opus\"}\n",
            "not the file that has ccdash's hooks already"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
