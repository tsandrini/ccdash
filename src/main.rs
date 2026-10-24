mod actions;
mod agents;
mod analytics;
mod api;
mod ask;
mod channel;
mod claims;
mod client;
mod collector;
mod command;
mod config;
mod conversation;
mod emit;
mod git;
mod history;
mod hub;
mod knowledge;
mod markdown;
mod messages;
mod model;
mod niri;
mod notify;
mod otlp;
mod paths;
mod procs;
mod protocol;
mod push;
mod state;
mod store;
mod sync;
mod task_notification;
mod tokens;
mod usage;
mod util;
mod waits;
mod wire;
mod workspace;

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
    /// Run the hub: store, web UI, API, collector links, notifications.
    Hub {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the collector for this machine, reporting to the hub.
    Collector {
        #[arg(long)]
        config: Option<PathBuf>,
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
enum Import {
    /// A single-host ccdash database (events, samples, sessions…), rows
    /// keeping their host. Safe to repeat.
    Db {
        #[arg(long)]
        config: Option<PathBuf>,
        /// The database to read, e.g. a copy of `~/.local/state/ccdash/ccdash.db`.
        source: PathBuf,
    },
    /// A workspace's `.claude/memory` directory: one memory note per file,
    /// keyed by its `name`, sent to the hub through this machine's
    /// collector. Safe to repeat.
    Memory { dir: PathBuf },
    /// A markdown plan: the file as a document, its checkboxes as issues,
    /// its `## Log` entries as progress notes. Safe to repeat.
    Plan {
        file: PathBuf,
        /// The repo directory name the plan is about (default: none).
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
            config::load::<HubConfig>(config).and_then(hub::run)
        }
        Command::Collector { config } => {
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
            what: Import::Plan { file, project },
        } => std::path::absolute(&file)
            .map_err(Into::into)
            .and_then(|file| Ok((hub::import::plan_file(&file, project.as_deref())?, file)))
            .and_then(|(notes, file)| client::import(&file, notes)),
        Command::Backup { config, dest } => config::load::<HubConfig>(config)
            .and_then(|cfg| hub::backup::backup(&cfg.store_path(), &dest)),
        Command::Export { config, db, dir } => export(config, db, &dir),
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

fn logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CCDASH_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .without_time()
        .init();
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
    let path = match db {
        Some(db) => db,
        None => config::load::<HubConfig>(config)?.store_path(),
    };
    let conn = rusqlite::Connection::open_with_flags(
        &path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    let n = knowledge::export(&conn, dir)?;
    println!("{n} notes written under {}", dir.display());
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
