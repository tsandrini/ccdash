use std::{
    fs::Permissions,
    os::unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    net::{TcpListener, UnixDatagram, UnixListener},
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};
use tracing::{debug, info, warn};

use crate::{
    agents,
    api::{http, unix},
    config::Config,
    hub::{Cmd, Hub, Input},
    niri, paths, procs,
    state::State,
    store::Store,
    util::now_ms,
    wire::{self, Envelope},
};

const TICK: Duration = Duration::from_secs(15);

pub fn run(cfg: Config) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(Arc::new(cfg)))
}

async fn serve(cfg: Arc<Config>) -> Result<()> {
    let dir = paths::runtime_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, Permissions::from_mode(0o700))?;
    let (ingest_path, api_path) = (paths::ingest_socket(), paths::api_socket());
    if StdUnixStream::connect(&api_path).is_ok() {
        bail!(
            "another ccdash daemon is listening on {}",
            api_path.display()
        );
    }

    let host = cfg.host();
    let store = Store::open(&cfg.store_path(), host.clone(), cfg.store.retention_days)?;
    let token = Arc::new(http::load_token(&cfg)?);
    let hub = Hub::spawn(
        State::new(host.clone(), cfg.niri.terminals.clone()),
        store.clone(),
    );

    let ingest = UnixDatagram::bind(replace(&ingest_path)?)
        .with_context(|| format!("binding {}", ingest_path.display()))?;
    let api = UnixListener::bind(replace(&api_path)?)
        .with_context(|| format!("binding {}", api_path.display()))?;
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
        ));
    }

    tokio::spawn(ingest_loop(ingest, hub.sender()));
    tokio::spawn(unix::serve(api, hub.clone(), cfg.clone()));
    tokio::spawn(agents::run(
        cfg.agents.command.clone(),
        Duration::from_secs(cfg.agents.interval_secs.max(1)),
        hub.sender(),
    ));
    if cfg.niri.enable {
        tokio::spawn(niri::run(hub.sender()));
    }
    if cfg.procs.enable {
        tokio::spawn(procs::run(
            hub.clone(),
            Duration::from_secs(cfg.procs.interval_secs.max(1)),
        ));
    }
    tokio::spawn(ticker(hub.sender()));
    info!("ccdash daemon up on {host}, sockets in {}", dir.display());

    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    info!("shutting down");
    let _ = std::fs::remove_file(&ingest_path);
    let _ = std::fs::remove_file(&api_path);
    Ok(())
}

fn replace(path: &Path) -> Result<&Path> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing stale {}", path.display()))
        }
        _ => Ok(path),
    }
}

async fn ingest_loop(socket: UnixDatagram, tx: mpsc::Sender<Cmd>) {
    let mut buf = vec![0u8; wire::MAX_DATAGRAM + 1024];
    loop {
        let len = match socket.recv(&mut buf).await {
            Ok(len) => len,
            Err(e) => {
                warn!("ingest: {e}");
                continue;
            }
        };
        match serde_json::from_slice::<Envelope>(&buf[..len]) {
            Ok(env) if env.v == wire::VERSION => {
                if tx.send(Cmd::Input(Input::Envelope(env))).await.is_err() {
                    return;
                }
            }
            Ok(env) => debug!("ignoring envelope version {}", env.v),
            Err(e) => debug!("bad datagram: {e}"),
        }
    }
}

async fn ticker(tx: mpsc::Sender<Cmd>) {
    let mut ticker = tokio::time::interval(TICK);
    loop {
        ticker.tick().await;
        if tx.send(Cmd::Input(Input::Tick(now_ms()))).await.is_err() {
            return;
        }
    }
}
