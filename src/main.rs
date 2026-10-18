mod actions;
mod agents;
mod analytics;
mod api;
mod ask;
mod channel;
mod client;
mod command;
mod config;
mod conversation;
mod daemon;
mod emit;
mod history;
mod hub;
mod markdown;
mod model;
mod niri;
mod notify;
mod otlp;
mod paths;
mod procs;
mod state;
mod store;
mod usage;
mod util;
mod waits;
mod wire;
mod workspace;

use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, wire::Source};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the collector daemon.
    Daemon {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Claude Code hook: forward the hook payload on stdin to the daemon.
    Emit,
    /// Claude Code PermissionRequest hook: hold the prompt open so ccdash can
    /// answer it, alongside the terminal dialog.
    Ask,
    /// Statusline wrapper: forward stdin to the daemon, then run COMMAND on it.
    Tee {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List the sessions the daemon knows about.
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
    /// Print the web UI address including the access token.
    Url {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Host name or address the phone uses to reach this machine.
        #[arg(long, conflicts_with = "lan")]
        host: Option<String>,
        /// Use this machine's LAN address.
        #[arg(long)]
        lan: bool,
        /// Also print a QR code to scan with the phone.
        #[arg(long)]
        qr: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Emit => return exit(emit::run(Source::Hook, &[])),
        Command::Ask => return exit(ask::run()),
        Command::Tee { command } => return exit(emit::run(Source::Statusline, &command)),
        Command::Daemon { config } => run_daemon(config),
        Command::Status { json } => client::status(json),
        Command::Open { id } => client::open(&id),
        Command::Rename { id, title } => client::rename(&id, &title.join(" ")),
        Command::Reply { id, text } => client::reply(&id, &text.join(" ")),
        Command::Channel => channel::run(),
        Command::Url {
            config,
            host,
            lan,
            qr,
        } => print_url(config, host, lan, qr),
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

fn run_daemon(config: Option<PathBuf>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CCDASH_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .without_time()
        .init();
    daemon::run(Config::load(config)?)
}

fn print_url(config: Option<PathBuf>, host: Option<String>, lan: bool, qr: bool) -> Result<()> {
    let cfg = Config::load(config)?;
    let token = api::http::load_token(&cfg)?;
    let port = cfg.http.listen.first().map_or(7837, |a| a.port());
    let host = match (host, lan) {
        (Some(host), _) => host,
        (None, true) => util::lan_address()
            .context("no LAN address found; pass --host")?
            .to_string(),
        (None, false) => cfg.host(),
    };
    let url = util::web_url(&host, port, &token);
    if qr {
        println!("{}", util::qr_text(&url)?);
    }
    println!("{url}");
    Ok(())
}
