//! Read-only HTTP API for the web UI and the phone. Every `/api` route needs
//! `Authorization: Bearer <token>`.

use std::{
    convert::Infallible,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::warn;

use super::web;
use crate::{
    config::Config,
    history::{self, History},
    hub::Hub,
    markdown,
    model::Snapshot,
    paths,
    store::{self, Store},
    util::now_ms,
};

const DAY_MS: i64 = 86_400_000;

#[derive(Clone)]
struct App {
    hub: Hub,
    store: Store,
}

pub async fn serve(listener: TcpListener, hub: Hub, store: Store, token: Arc<String>) {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/sessions/{id}", get(session))
        .route("/history", get(history))
        .route("/stream", get(stream))
        .layer(middleware::from_fn_with_state(token, auth));
    let app = Router::new()
        .route("/", get(web::index))
        .route("/{asset}", get(web::asset))
        .nest("/api/v1", api)
        .with_state(App { hub, store });
    if let Err(e) = axum::serve(listener, app).await {
        warn!("http: {e}");
    }
}

/// Reads the configured token file, or the generated one in the state dir
/// (creating it on first use).
pub fn load_token(cfg: &Config) -> Result<String> {
    if let Some(path) = &cfg.http.token_file {
        let token =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let token = token.trim();
        if token.is_empty() {
            bail!("{} is empty", path.display());
        }
        return Ok(token.to_owned());
    }
    let path = paths::state_dir().join("token");
    if let Ok(token) = std::fs::read_to_string(&path)
        && !token.trim().is_empty()
    {
        return Ok(token.trim().to_owned());
    }
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(paths::state_dir())?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?
        .write_all(token.as_bytes())?;
    Ok(token)
}

async fn auth(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if given.is_some_and(|given| constant_time_eq(given.as_bytes(), token.as_bytes())) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn snapshot(State(app): State<App>) -> Result<Json<Snapshot>, ApiError> {
    Ok(Json(app.hub.snapshot().await?))
}

#[derive(Deserialize)]
struct EventsQuery {
    limit: Option<u32>,
}

async fn session(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .resolve(&id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?;
    let (store, session_id) = (app.store.clone(), session.id.clone());
    let limit = query.limit.unwrap_or(200).min(2000);
    let events =
        tokio::task::spawn_blocking(move || store::events(&store.reader()?, &session_id, limit))
            .await
            .map_err(anyhow::Error::from)??;
    let prompt_html = session.prompt.as_ref().map(|p| markdown::render(&p.text));
    let reply_html = session.reply.as_ref().map(|r| markdown::render(&r.text));
    Ok(Json(json!({
        "session": session,
        "events": events,
        "prompt_html": prompt_html,
        "reply_html": reply_html,
    })))
}

#[derive(Deserialize)]
struct HistoryQuery {
    since: Option<i64>,
}

async fn history(
    State(app): State<App>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<History>, ApiError> {
    let now = now_ms();
    let since = query
        .since
        .unwrap_or(now - DAY_MS)
        .clamp(now - 31 * DAY_MS, now);
    let live = app.hub.snapshot().await?.sessions;
    let store = app.store.clone();
    let history =
        tokio::task::spawn_blocking(move || history::history(&store.reader()?, since, now, &live))
            .await
            .map_err(anyhow::Error::from)??;
    Ok(Json(history))
}

async fn stream(State(app): State<App>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let updates = BroadcastStream::new(app.hub.subscribe());
    let first = app
        .hub
        .snapshot()
        .await
        .ok()
        .and_then(|s| Event::default().event("snapshot").json_data(s).ok());
    let updates = updates.filter_map(|update| match update {
        Ok(update) => Event::default().event("update").json_data(update).ok(),
        Err(_) => Some(Event::default().event("lagged").data("")),
    });
    let events = tokio_stream::iter(first).chain(updates).map(Ok);
    Sse::new(events).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

struct ApiError(StatusCode, String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
