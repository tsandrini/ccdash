mod actions;
mod agents;
mod api;
mod client;
mod config;
mod conversation;
mod daemon;
mod emit;
mod history;
mod hub;
mod markdown;
mod model;
mod niri;
mod paths;
mod state;
mod store;
mod util;
mod wire;

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
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
    /// Print the web UI address including the access token.
    Url {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Host name or address the phone uses to reach this machine.
        #[arg(long)]
        host: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Emit => return exit(emit::run(Source::Hook, &[])),
        Command::Tee { command } => return exit(emit::run(Source::Statusline, &command)),
        Command::Daemon { config } => run_daemon(config),
        Command::Status { json } => client::status(json),
        Command::Open { id } => client::open(&id),
        Command::Url { config, host } => print_url(config, host),
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

fn print_url(config: Option<PathBuf>, host: Option<String>) -> Result<()> {
    let cfg = Config::load(config)?;
    let token = api::http::load_token(&cfg)?;
    let port = cfg.http.listen.first().map_or(7837, |a| a.port());
    let host = host.unwrap_or_else(|| cfg.host());
    println!("http://{host}:{port}/#token={token}");
    Ok(())
}
