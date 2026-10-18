//! `ccdash hub`: the one process that owns the data. Collectors on each
//! workstation report over `/collect`; the web UI, the API and the
//! notifications are served from here.

pub mod backup;
pub mod collect;
pub mod core;
pub mod http;
pub mod import;
mod web;

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::{
    net::TcpListener,
    signal::unix::{SignalKind, signal},
    sync::{mpsc, watch},
};
use tracing::info;

use crate::{
    config::HubConfig,
    hub::core::{Cmd, Hub, Notes},
    notify::{self, Composer},
    store::Store,
    util::now_ms,
    workspace::Workspaces,
};

const TICK: Duration = Duration::from_secs(15);
/// The lifecycle pass: once after start-up, then daily.
const REFLECT_FIRST: Duration = Duration::from_secs(120);
const REFLECT_EVERY: Duration = Duration::from_secs(24 * 3600);

pub fn run(cfg: HubConfig) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(Arc::new(cfg)))
}

async fn serve(cfg: Arc<HubConfig>) -> Result<()> {
    let host = cfg.host();
    let home = std::env::var("HOME").unwrap_or_default();
    let workspaces = Workspaces::new(&cfg.workspaces.dirs, &home);
    let store = Store::open(&cfg.store_path(), cfg.store.retention_days)?;
    let token = Arc::new(http::load_token(&cfg)?);
    let public_url = cfg.public_url();
    let notes = (!cfg.notify.url.is_empty()).then(|| {
        let web_url = cfg
            .notify
            .web_url
            .clone()
            .unwrap_or_else(|| public_url.clone());
        info!("notifications to {} link to {web_url}", cfg.notify.url);
        let (tx, rx) = watch::channel(Vec::new());
        tokio::spawn(notify::run(cfg.notify.clone(), rx));
        Notes {
            composer: Composer::new(cfg.notify.clone(), web_url, now_ms()),
            tx,
        }
    });
    let hub = Hub::spawn(
        host.clone(),
        public_url.clone(),
        workspaces.clone(),
        store.clone(),
        notes,
        cfg.knowledge.clone(),
    );
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
        ));
    }
    tokio::spawn(ticker(hub.sender()));
    if cfg.knowledge.reflect {
        tokio::spawn(reflector(hub.sender()));
    }
    info!("ccdash hub up on {host}, web UI at {public_url}");

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
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

async fn reflector(tx: mpsc::Sender<Cmd>) {
    tokio::time::sleep(REFLECT_FIRST).await;
    loop {
        if tx.send(Cmd::Reflect).await.is_err() {
            return;
        }
        tokio::time::sleep(REFLECT_EVERY).await;
    }
}
