mod actions;
mod agent_message;
mod agents;
mod analytics;
mod api;
mod ask;
mod catalog;
mod channel;
mod claims;
mod client;
mod collector;
mod command;
mod config;
mod conversation;
mod demo;
mod drafts;
mod emit;
mod fsx;
mod git;
mod history;
mod hub;
mod ipc;
mod knowledge;
mod machine;
mod markdown;
mod messages;
mod model;
mod niri;
mod notify;
mod otlp;
mod palette;
mod paths;
mod procs;
mod protocol;
mod push;
mod report;
mod state;
mod store;
mod sync;
mod task_notification;
mod tokens;
mod usage;
mod util;
mod waits;
#[cfg(windows)]
mod win;
mod wire;
mod workspace;
mod wrap_up;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{
    config::{CollectorConfig, HubConfig},
    wire::Source,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the hub: store, web UI, API, collector links, notifications
    /// (Linux only).
    Hub {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the collector for this machine, reporting to the hub.
    Collector {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Start it again with no console window, its log in
        /// `%LOCALAPPDATA%\ccdash\state\collector.log`, and return.
        #[cfg(windows)]
        #[arg(long)]
        background: bool,
    },
    /// Claude Code hook: forward the hook payload on stdin to the collector.
    Emit,
    /// Claude Code PermissionRequest hook: hold the prompt open so ccdash can
    /// answer it, alongside the terminal dialog.
    Ask,
    /// Statusline wrapper: forward stdin to the collector, then run COMMAND on it.
    Tee {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List this machine's sessions as the hub sees them.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Focus a session's window, or attach to a background session.
    Open {
        /// Session id, background short id, or unique id prefix.
        id: String,
    },
    /// Rename a session; Claude Code adopts the title at its next prompt.
    Rename {
        /// Session id, background short id, or unique id prefix.
        id: String,
        #[arg(required = true, trailing_var_arg = true)]
        title: Vec<String>,
    },
    /// Send a message to a session, as if typed into it (needs the ccdash channel).
    Reply {
        /// Session id, background short id, or unique id prefix.
        id: String,
        #[arg(required = true, trailing_var_arg = true)]
        text: Vec<String>,
    },
    /// MCP server for Claude Code's `--channels`: relays replies from
    /// ccdash into the session.
    Channel,
    /// Print the web UI address; with a hub config, a pairing link for one
    /// new device (good for an hour, revoked in Settings › Devices).
    Url {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Read the hub configuration and mint a device token through its
        /// store (default: this machine's collector config, no token).
        #[arg(long)]
        hub: bool,
        /// Also print a QR code to scan with the phone.
        #[arg(long)]
        qr: bool,
        /// How Settings › Devices names the new device (with --hub).
        #[arg(long, default_value = tokens::DEFAULT_LABEL)]
        label: String,
    },
    /// Load data into the hub's database.
    Import {
        #[command(subcommand)]
        what: Import,
    },
    /// Copy the hub's database through the backup API and check the copy.
    Backup {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Where the copy goes (replaced atomically).
        dest: PathBuf,
    },
    /// Run `git gc` over the hub's workspace repositories (nightly).
    Gc {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// The workspace sync on this machine: where every unit stands,
    /// resync, init a new machine, adopt existing copies.
    Workspace {
        #[command(subcommand)]
        what: Workspace,
    },
    /// HTML reports: build one in the shared design, publish one to the hub.
    Report {
        #[command(subcommand)]
        what: Report,
    },
    /// The hub's `pre-receive` hook (run by git, not by hand): a merge
    /// snapshot must build on the other hosts' current tips.
    #[command(hide = true)]
    PreReceive,
    /// A hub on a made-up workspace with two stand-in machines, to try the
    /// panel or take screenshots: a fresh database each run, nothing real
    /// (Linux only).
    Demo {
        /// Where its database and files go (made, or emptied when an earlier
        /// demo made it; default: a `ccdash-demo` temporary directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:7840")]
        listen: std::net::SocketAddr,
    },
    /// Ends Claude Code process PID and everything under it from inside its
    /// console, then turns off the terminal modes it left on (run by the
    /// collector's stop, not by hand).
    #[cfg(windows)]
    #[command(hide = true)]
    EndTree { pid: u32 },
    /// Write every note of the hub's database as markdown with frontmatter:
    /// `<dir>/<workspace>/<kind>/<slug>.md`. Reads the database directly,
    /// so it works on a backup copy too.
    Export {
        #[arg(long)]
        config: Option<PathBuf>,
        /// A database file to read instead of the configured one.
        #[arg(long)]
        db: Option<PathBuf>,
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum Workspace {
    /// Every unit with its state (or one unit).
    Status {
        unit: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Fetch and snapshot everything (or one unit) again, changed remotes
    /// and upstreams included; adds those the other machines' snapshots
    /// carry and this copy lacks.
    Resync { unit: Option<String> },
    /// A new machine: create every unit the hub has and this one lacks.
    Init,
    /// An existing copy of the workspace: take the first snapshot of the
    /// units that exist here and on the hub, then merge.
    Adopt {
        unit: Option<String>,
        /// Which side wins outright instead of merging: `mine` (this
        /// machine's content replaces the hub's) or `hub` (the hub's
        /// content replaces this copy). A stale copy adopts with `hub`,
        /// the freshest one with `mine`.
        #[arg(long, value_parser = ["mine", "hub"])]
        prefer: Option<String>,
    },
}

#[derive(Subcommand)]
enum Import {
    /// A single-host ccdash database (events, samples, sessions…), rows
    /// keeping their host. Safe to repeat.
    Db {
        #[arg(long)]
        config: Option<PathBuf>,
        /// The database to read, e.g. a copy of `~/.local/state/ccdash/ccdash.db`.
        source: PathBuf,
    },
    /// A workspace's `.claude/memory` directory, or one of its
    /// subdirectories: one memory note per file, keyed by its `name`, titled
    /// by its `MEMORY.md`/`INDEX.md` line, sent to the hub through this
    /// machine's collector. Safe to repeat: the hub keeps a note it changed
    /// after the file.
    Memory { dir: PathBuf },
    /// A markdown file: the file as a document and, unless `--doc`, its
    /// checkboxes as issues and its `## Log` entries as progress notes.
    /// Safe to repeat.
    Plan {
        file: PathBuf,
        /// The repo directory name the plan is about (default: none).
        #[arg(long)]
        project: Option<String>,
        /// The document's slug (default: the file stem, prefixed with its
        /// directory when that is not `plans`).
        #[arg(long)]
        slug: Option<String>,
        /// The document only: no issues, no progress notes.
        #[arg(long)]
        doc: bool,
    },
}

#[derive(Subcommand)]
enum Report {
    /// Wrap an HTML body fragment (sections under h2/h3 headings, tables,
    /// figures, tiles) in the report chrome the `ccdash:report` skill
    /// describes: top bar, contents, theme switch, numbered headings.
    /// Headings get ids. Refuses a body that fetches anything from the
    /// network.
    Build {
        /// The body fragment (`-` reads stdin).
        body: PathBuf,
        /// The report's title.
        #[arg(long)]
        title: String,
        /// The date shown (default: today, YYYY-MM-DD).
        #[arg(long)]
        date: Option<String>,
        /// Where the report goes.
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Publish a report to the hub through this machine's collector: the
    /// hub keeps the file, the panel shows it, and a `report` note with
    /// the abstract joins the workspace's knowledge (the workspace is the
    /// current directory's).
    Publish {
        /// The report (built with `build`).
        file: PathBuf,
        /// Title (default: the file's <title>).
        #[arg(long)]
        title: Option<String>,
        /// The note's slug (default: from the title).
        #[arg(long)]
        slug: Option<String>,
        /// One line for lists.
        #[arg(long)]
        summary: Option<String>,
        /// The abstract, markdown.
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        body: Option<String>,
        /// The abstract, read from a markdown file.
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// The project (a repo directory name; default: the directory's).
        #[arg(long)]
        project: Option<String>,
        /// When the report was made, YYYY-MM-DD (default: today).
        #[arg(long)]
        date: Option<String>,
    },
    /// Re-wrap every published report `build` made in the current chrome,
    /// on the hub: a change to the template (the text size switch, say)
    /// reaches published reports only this way, the chrome being baked into
    /// each file. The file replaced is copied first to
    /// `<reports dir>/.archive/<date>/<workspace>/<slug>.html`; the note's
    /// size and hash follow, its version and dates stay. Imported pages of
    /// their own design are left alone. `--dry-run` only lists.
    Rebuild {
        #[arg(long)]
        config: Option<PathBuf>,
        /// A database file instead of the configured one.
        #[arg(long)]
        db: Option<PathBuf>,
        /// Where the replaced files go (default: `.archive/<date>` under
        /// the reports directory).
        #[arg(long)]
        archive: Option<PathBuf>,
        /// List what would change, change nothing.
        #[arg(long)]
        dry_run: bool,
        /// Only these reports, as `workspace/slug` (or a slug alone).
        only: Vec<String>,
    },
    /// Bring older reports in: every `.html` under the paths (directories
    /// are walked; node_modules, target, build and the like skipped) that is
    /// a self-contained HTML document is published through the collector,
    /// titled by its <title>, dated by its modification time, with an
    /// abstract naming where it came from and its sections. Files that fetch
    /// from the network are listed and skipped. `--dry-run` only lists.
    Import {
        /// Files or directories to look through.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// List what would be published, publish nothing.
        #[arg(long)]
        dry_run: bool,
        /// The project every report goes under (default: the directory's).
        #[arg(long)]
        project: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Emit => return exit(emit::run(Source::Hook, &[])),
        Command::Ask => return exit(ask::run()),
        Command::Tee { command } => return exit(emit::run(Source::Statusline, &command)),
        Command::Hub { config } => {
            logging();
            hub_here("the hub")
                .and_then(|()| config::load::<HubConfig>(config))
                .and_then(hub::run)
        }
        #[cfg(windows)]
        Command::Collector {
            config,
            background: true,
        } => collector::background(config),
        Command::Collector { config, .. } => {
            logging();
            config::load::<CollectorConfig>(config).and_then(collector::run)
        }
        Command::Status { json } => client::status(json),
        Command::Open { id } => client::open(&id),
        Command::Rename { id, title } => client::rename(&id, &title.join(" ")),
        Command::Reply { id, text } => client::reply(&id, &text.join(" ")),
        Command::Channel => channel::run(),
        Command::Url {
            config,
            hub,
            qr,
            label,
        } => print_url(config, hub, qr, &label),
        Command::Import {
            what: Import::Db { config, source },
        } => import_db(config, &source),
        Command::Import {
            what: Import::Memory { dir },
        } => std::path::absolute(&dir)
            .map_err(Into::into)
            .and_then(|dir| Ok((hub::import::memory_dir(&dir)?, dir)))
            .and_then(|(notes, dir)| client::import(&dir, notes)),
        Command::Import {
            what:
                Import::Plan {
                    file,
                    project,
                    slug,
                    doc,
                },
        } => std::path::absolute(&file)
            .map_err(Into::into)
            .and_then(|file| {
                let notes =
                    hub::import::plan_file(&file, project.as_deref(), slug.as_deref(), doc)?;
                Ok((notes, file))
            })
            .and_then(|(notes, file)| client::import(&file, notes)),
        Command::Backup { config, dest } => config::load::<HubConfig>(config)
            .and_then(|cfg| hub::backup::backup(&cfg.store_path(), &dest)),
        Command::Gc { config } => config::load::<HubConfig>(config)
            .and_then(|cfg| workspace::hub::gc(&cfg.sync_dir().join("workspace"))),
        Command::Workspace { what } => match what {
            Workspace::Status { unit, json } => {
                client::workspace("status", unit.as_deref(), None, json)
            }
            Workspace::Resync { unit } => client::workspace("resync", unit.as_deref(), None, false),
            Workspace::Init => client::workspace("init", None, None, false),
            Workspace::Adopt { unit, prefer } => {
                client::workspace("adopt", unit.as_deref(), prefer.as_deref(), false)
            }
        },
        Command::Report { what } => match what {
            Report::Build {
                body,
                title,
                date,
                output,
            } => report_build(&body, &title, date.as_deref(), &output),
            Report::Import {
                paths,
                dry_run,
                project,
            } => report_import(&paths, dry_run, project.as_deref()),
            Report::Rebuild {
                config,
                db,
                archive,
                dry_run,
                only,
            } => report_rebuild(config, db, archive, &only, dry_run),
            Report::Publish {
                file,
                title,
                slug,
                summary,
                body,
                body_file,
                project,
                date,
            } => std::path::absolute(&file)
                .map_err(Into::into)
                .and_then(|file| {
                    let body = match body_file {
                        Some(path) => std::fs::read_to_string(&path)
                            .with_context(|| format!("reading {}", path.display()))?,
                        None => body.unwrap_or_default(),
                    };
                    client::tool(
                        collector::reports::TOOL,
                        serde_json::json!({
                            "path": file,
                            "title": title,
                            "slug": slug,
                            "summary": summary,
                            "body": body,
                            "project": project,
                            "date": date,
                        }),
                    )
                }),
        },
        Command::PreReceive => return exit(workspace::hub::pre_receive()),
        #[cfg(windows)]
        Command::EndTree { pid } => actions::end_tree_in_console(pid),
        Command::Export { config, db, dir } => export(config, db, &dir),
        Command::Demo { dir, listen } => {
            logging();
            hub_here("ccdash demo, a hub,").and_then(|()| demo::run(dir, listen))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ccdash: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn exit(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// The hub runs on Linux only, and so does the demo: a Windows machine is
/// a workstation, its collector reporting to a hub elsewhere.
fn hub_here(what: &str) -> Result<()> {
    if cfg!(windows) {
        anyhow::bail!(
            "{what} runs on Linux only: on Windows run the collector, against a hub on Linux"
        );
    }
    Ok(())
}

fn logging() {
    let builder = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_env("CCDASH_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
    );
    // NOTE: no journal on Windows: a redirected stderr is the log file itself
    if cfg!(windows) && !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        builder.with_ansi(false).init();
    } else {
        builder.without_time().init();
    }
}

/// The hub's web address: with a fresh device token minted in the hub's
/// store (`--hub`, on the hub), or just the address a collector reports
/// to.
fn print_url(config: Option<PathBuf>, hub: bool, qr: bool, label: &str) -> Result<()> {
    let url = if hub {
        let cfg: HubConfig = config::load(config)?;
        let label = tokens::label(label).context("the label is empty")?;
        let conn = store::open_writer(&cfg.store_path())?;
        let (_, secret) = tokens::mint(&conn, &label, util::now_ms())?;
        eprintln!(
            "A pairing link for one device, good for an hour; revoke it in Settings › Devices."
        );
        util::web_url(&cfg.public_url(), &secret)
    } else {
        let cfg: CollectorConfig = config::load(config)?;
        let base = cfg
            .hub
            .url
            .trim_end_matches("/collect")
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1);
        if base.is_empty() {
            anyhow::bail!("hub.url is not set; run with --hub on the hub for the token link");
        }
        base
    };
    if qr {
        println!("{}", util::qr_text(&url)?);
    }
    println!("{url}");
    Ok(())
}

fn export(config: Option<PathBuf>, db: Option<PathBuf>, dir: &Path) -> Result<()> {
    let cfg = config::load::<HubConfig>(config)?;
    let path = db.unwrap_or_else(|| cfg.store_path());
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    let reports = cfg.reports_dir();
    let n = knowledge::export(&conn, dir, reports.is_dir().then_some(reports.as_path()))?;
    println!("{n} notes written under {}", dir.display());
    Ok(())
}

/// `ccdash report import`: older reports found under `paths`, published
/// one by one (or only listed with `dry_run`).
fn report_import(paths: &[PathBuf], dry_run: bool, project: Option<&str>) -> Result<()> {
    let files = report::find_html(paths)?;
    if files.is_empty() {
        anyhow::bail!("no .html files under the given paths");
    }
    let (mut published, mut skipped, mut failed) = (0, 0, 0);
    for file in &files {
        let Some(found) = report::inspect(file)? else {
            println!("skip     {}: not an HTML document", file.display());
            skipped += 1;
            continue;
        };
        if !found.problems.is_empty() {
            println!(
                "skip     {} ({}): {}",
                file.display(),
                found.title,
                found.problems.join("; ")
            );
            skipped += 1;
            continue;
        }
        let size = format!("{} KB", found.bytes.div_ceil(1024));
        if dry_run {
            println!(
                "would    {} ({size}, {}) as `{}`: {}",
                file.display(),
                found.date,
                found.slug,
                found.title
            );
            continue;
        }
        match client::tool_text(
            collector::reports::TOOL,
            serde_json::json!({
                "path": file,
                "title": found.title,
                "slug": found.slug,
                "summary": found.summary,
                "body": found.body,
                "project": project,
                "date": found.date,
            }),
        ) {
            Ok(text) => {
                println!("ok       {}", text.lines().next().unwrap_or_default());
                published += 1;
            }
            Err(e) => {
                println!("failed   {}: {e:#}", file.display());
                failed += 1;
            }
        }
    }
    if dry_run {
        println!(
            "{} file(s) would be published, {skipped} skipped; run again without --dry-run",
            files.len() - skipped
        );
    } else {
        println!("{published} published, {skipped} skipped, {failed} failed");
    }
    Ok(())
}

/// `ccdash report rebuild`: every built report on the hub in the current
/// chrome, the old files archived.
fn report_rebuild(
    config: Option<PathBuf>,
    db: Option<PathBuf>,
    archive: Option<PathBuf>,
    only: &[String],
    dry_run: bool,
) -> Result<()> {
    let cfg = config::load::<HubConfig>(config)?;
    let path = db.unwrap_or_else(|| cfg.store_path());
    let dir = cfg.reports_dir();
    if !dir.is_dir() {
        anyhow::bail!("no reports directory at {}", dir.display());
    }
    let now = util::now_ms();
    let archive = archive.unwrap_or_else(|| dir.join(".archive").join(knowledge::ymd(now)));
    let conn = if dry_run {
        rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {}", path.display()))?
    } else {
        store::open_writer(&path)?
    };
    let done = hub::reports::rebuild(&conn, &dir, &archive, only, dry_run, now)?;
    if done.is_empty() {
        anyhow::bail!("no report matches");
    }
    let (mut rebuilt, mut outdated) = (0, 0);
    for (workspace, slug, what) in &done {
        use hub::reports::Outcome;
        let (verb, rest) = match what {
            Outcome::Rebuilt { archived, bytes } => {
                rebuilt += 1;
                (
                    "rebuilt ",
                    format!(
                        "({} KB); the old file is at {}",
                        bytes.div_ceil(1024),
                        archived.display()
                    ),
                )
            }
            Outcome::Outdated => {
                outdated += 1;
                ("outdated", "would be rebuilt".into())
            }
            Outcome::Current => ("current ", "already in this chrome".into()),
            Outcome::Foreign => ("foreign ", "its own design, left alone".into()),
            Outcome::Missing => ("missing ", "the note has no file".into()),
        };
        println!("{verb} {workspace}/{slug}: {rest}");
    }
    if dry_run {
        println!(
            "{outdated} of {} report(s) would be rebuilt; run again without --dry-run",
            done.len()
        );
    } else {
        println!("{rebuilt} of {} report(s) rebuilt", done.len());
    }
    Ok(())
}

/// `ccdash report build`: the chrome around a body fragment.
fn report_build(body: &Path, title: &str, date: Option<&str>, output: &Path) -> Result<()> {
    let fragment = if body == Path::new("-") {
        std::io::read_to_string(std::io::stdin())?
    } else {
        std::fs::read_to_string(body).with_context(|| format!("reading {}", body.display()))?
    };
    let today = knowledge::ymd(util::now_ms());
    let html = report::build(
        &report::Meta {
            title,
            date: date.unwrap_or(&today),
        },
        &fragment,
    );
    let problems = report::check(&html);
    for p in problems.iter().filter(|p| !p.fatal) {
        eprintln!("warning: {}", p.text);
    }
    let fatal: Vec<&str> = problems
        .iter()
        .filter(|p| p.fatal)
        .map(|p| p.text.as_str())
        .collect();
    if !fatal.is_empty() {
        anyhow::bail!(
            "the report would not be self-contained:\n- {}",
            fatal.join("\n- ")
        );
    }
    if let Some(dir) = output.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(output, &html).with_context(|| format!("writing {}", output.display()))?;
    println!(
        "wrote {} ({} KB, {} sections)",
        output.display(),
        html.len().div_ceil(1024),
        report::sections(&html).len()
    );
    Ok(())
}

fn import_db(config: Option<PathBuf>, source: &Path) -> Result<()> {
    let cfg: HubConfig = config::load(config)?;
    hub::import::check(source)?;
    let added = hub::import::import(source, &cfg.store_path())
        .with_context(|| format!("importing {}", source.display()))?;
    for (table, n) in added {
        println!("{table}: {n} rows added");
    }
    Ok(())
}
