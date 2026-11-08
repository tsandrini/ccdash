//! `ccdash hub`: the one process that owns the data. Collectors on each
//! workstation report over `/collect`; the web UI, the API and the
//! notifications are served from here.

pub mod backup;
pub mod collect;
pub mod core;
pub mod http;
pub mod import;
pub mod reports;
mod web;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
};
use tracing::{info, warn};

use crate::{
    config::HubConfig,
    hub::core::{Cmd, Hub, Notes},
    machine::Sampler,
    notify::{self, Composer, Delays},
    push,
    store::Store,
    util::now_ms,
    workspace::Workspaces,
};

const TICK: Duration = Duration::from_secs(15);
const MACHINE_EVERY: Duration = Duration::from_secs(30);
/// The lifecycle pass: once after start-up, then daily.
const REFLECT_FIRST: Duration = Duration::from_secs(120);
const REFLECT_EVERY: Duration = Duration::from_secs(24 * 3600);

pub fn run(cfg: HubConfig) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(Arc::new(cfg), None))
}

/// What `ccdash demo` runs next to its hub: stand-in machines in the place
/// of this machine's numbers.
pub type Demo = Box<dyn FnOnce(Hub) + Send>;

/// Runs the hub until a signal.
pub async fn serve(cfg: Arc<HubConfig>, demo: Option<Demo>) -> Result<()> {
    let host = cfg.host();
    let home = std::env::var("HOME").unwrap_or_default();
    let workspaces = Workspaces::new(&cfg.workspaces.dirs, &home);
    if cfg.workspaces.dirs.is_empty() {
        warn!(
            "no workspaces (`[workspaces] dirs`): the knowledge tools refuse every session until one is set"
        );
    }
    let store = Store::open(&cfg.store_path(), cfg.store.retention_days)?;
    let token = Arc::new(http::load_token(&cfg)?);
    let public_url = cfg.public_url();
    let push = cfg
        .push
        .enabled
        .then(|| {
            push::Keys::load_or_create(&cfg.push_key_file())
                .map(|keys| push::Push::new(keys, store.clone(), &cfg.push, &public_url))
        })
        .transpose()
        .context("web push")?;
    let notes = push.as_ref().map(|push| {
        let web_url = cfg
            .notify
            .web_url
            .clone()
            .unwrap_or_else(|| public_url.clone());
        let (tx, rx) = watch::channel(Vec::new());
        let delays = Delays::from(&cfg.notify);
        info!("web push on, linking to {web_url}");
        tokio::spawn(notify::run(delays, rx, push.clone()));
        Notes {
            composer: Composer::new(cfg.notify.clone(), web_url, now_ms()),
            tx,
        }
    });
    let repos = if cfg.workspace.enable {
        let root = cfg.sync_dir().join("workspace");
        let repos =
            Arc::new(crate::workspace::hub::Repos::open(&root).context("workspace repositories")?);
        info!("keeping the workspace repositories in {}", root.display());
        Some(repos)
    } else {
        None
    };
    let hub = Hub::spawn(
        host.clone(),
        public_url.clone(),
        workspaces.clone(),
        store.clone(),
        notes,
        cfg.knowledge.clone(),
        cfg.store.tool_output_bytes,
        repos.clone(),
        cfg.wake.via.clone(),
    );
    let git = repos.map(|repos| crate::workspace::hub::GitState {
        repos,
        cmds: hub.sender(),
    });
    let web_dir = cfg.web_dir();
    if web_dir.join("index.html").is_file() {
        info!("serving the panel from {}", web_dir.display());
    } else {
        warn!(
            "{} has no index.html: the panel will not load (build web/ or set http.web_dir)",
            web_dir.display()
        );
    }
    let sync = crate::sync::hub::Mirror::open(store.clone(), &cfg.sync_dir(), hub.clone())?;
    info!(
        "keeping Claude Code's state in {}",
        cfg.sync_dir().display()
    );
    let reports = Arc::new(
        reports::Reports::open(&cfg.reports_dir(), cfg.reports.max_mb).context("reports")?,
    );
    let catalog = crate::catalog::Catalog::open(&cfg.catalog_path(), &cfg.store_path())
        .context("session catalog")?;
    crate::catalog::spawn(catalog.clone(), cfg.sync_dir(), sync.changed.subscribe());
    info!(
        "keeping the session catalog in {}",
        cfg.catalog_path().display()
    );
    info!("keeping reports in {}", cfg.reports_dir().display());
    let tokens = collect::Tokens::new(cfg.collect.tokens.clone());
    if cfg.collect.tokens.is_empty() {
        info!("no collector tokens configured: nothing will report");
    }
    for addr in &cfg.http.listen {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding http {addr}"))?;
        info!("http listening on {addr}");
        tokio::spawn(http::serve(
            listener,
            hub.clone(),
            store.clone(),
            token.clone(),
            workspaces.clone(),
            tokens.clone(),
            public_url.clone(),
            push.clone(),
            web_dir.clone(),
            sync.clone(),
            git.clone(),
            reports.clone(),
            catalog.clone(),
        ));
    }
    tokio::spawn(ticker(hub.sender()));
    if let Some(demo) = demo {
        demo(hub.clone());
    } else {
        let mut disks = vec![PathBuf::from("/")];
        disks.extend(cfg.store_path().parent().map(Path::to_path_buf));
        tokio::spawn(machine(Sampler::new(disks), hub.sender()));
    }
    if cfg.knowledge.reflect {
        tokio::spawn(reflector(hub.sender()));
    }
    info!("ccdash hub up on {host}, web UI at {public_url}");

    crate::util::shutdown().await?;
    info!("shutting down");
    Ok(())
}

async fn ticker(tx: mpsc::Sender<Cmd>) {
    let mut ticker = tokio::time::interval(TICK);
    loop {
        ticker.tick().await;
        if tx.send(Cmd::Tick(now_ms())).await.is_err() {
            return;
        }
    }
}

/// Reads the hub's own machine every `MACHINE_EVERY`.
async fn machine(mut sampler: Sampler, tx: mpsc::Sender<Cmd>) {
    let mut ticker = tokio::time::interval(MACHINE_EVERY);
    loop {
        ticker.tick().await;
        let Ok((back, report)) = tokio::task::spawn_blocking(move || {
            let report = sampler.read();
            (sampler, report)
        })
        .await
        else {
            return;
        };
        sampler = back;
        if tx.send(Cmd::HubMachine(Box::new(report))).await.is_err() {
            return;
        }
    }
}

async fn reflector(tx: mpsc::Sender<Cmd>) {
    tokio::time::sleep(REFLECT_FIRST).await;
    loop {
        if tx.send(Cmd::Reflect).await.is_err() {
            return;
        }
        tokio::time::sleep(REFLECT_EVERY).await;
    }
}
