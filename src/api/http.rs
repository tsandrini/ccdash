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
    analytics::{self, Analytics},
    config::Config,
    conversation,
    history::{self, History},
    hub::Hub,
    model::{Activity, Snapshot},
    paths,
    store::Store,
    util::now_ms,
    workspace::{Scope, Workspaces},
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

#[derive(Clone)]
struct App {
    hub: Hub,
    store: Store,
    workspaces: Arc<Workspaces>,
}

pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    store: Store,
    token: Arc<String>,
    workspaces: Workspaces,
) {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/sessions/{id}", get(session))
        .route("/history", get(history))
        .route("/analytics", get(analytics))
        .route("/stream", get(stream))
        .layer(middleware::from_fn_with_state(token, auth));
    let app = Router::new()
        .route("/", get(web::index))
        .route("/{asset}", get(web::asset))
        .nest("/api/v1", api)
        .with_state(App {
            hub,
            store,
            workspaces: Arc::new(workspaces),
        });
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
struct ConversationQuery {
    /// Page backwards: only exchanges that started before this timestamp.
    before: Option<i64>,
    limit: Option<usize>,
}

/// The session, its conversation (newest `limit` exchanges, default 10), and
/// whole-session stats on the first page.
async fn session(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .resolve(&id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?;
    let store = app.store.clone();
    let (session_id, started_at) = (session.id.clone(), session.started_at);
    let end = match session.activity {
        Activity::Gone => session.activity_since,
        _ => now_ms(),
    };
    let limit = query.limit.unwrap_or(10).clamp(1, 50);
    let (conversation, stats, resources) = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = store.reader()?;
        let conversation = conversation::conversation(&conn, &session_id, query.before, limit)?;
        let (stats, resources) = match query.before {
            None => (
                Some(conversation::stats(&conn, &session_id, started_at, end)?),
                Some(conversation::resources(&conn, &session_id, end - HOUR_MS)?),
            ),
            Some(_) => (None, None),
        };
        Ok((conversation, stats, resources))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({
        "session": session,
        "conversation": conversation,
        "stats": stats,
        "resources": resources,
    })))
}

/// Time range and workspace of a history or analytics request.
#[derive(Deserialize)]
struct RangeQuery {
    since: Option<i64>,
    /// Defaults to now; never later.
    until: Option<i64>,
    /// Workspace name from the snapshot's list.
    workspace: Option<String>,
    /// Viewer's offset from UTC in minutes (east positive), for day buckets.
    tz: Option<i64>,
}

impl RangeQuery {
    /// Clamped to the retention window; at least a minute long.
    fn scope(&self, workspaces: &Workspaces, default_ms: i64) -> Result<Scope, ApiError> {
        let now = now_ms();
        let oldest = now - 31 * DAY_MS;
        let until = self.until.unwrap_or(now).clamp(oldest + 60_000, now);
        let since = self
            .since
            .unwrap_or(until - default_ms)
            .clamp(oldest, until - 60_000);
        let dir = match &self.workspace {
            None => None,
            Some(name) => Some(
                workspaces
                    .dir(name)
                    .ok_or(ApiError(
                        StatusCode::BAD_REQUEST,
                        "unknown workspace".into(),
                    ))?
                    .to_owned(),
            ),
        };
        Ok(Scope {
            since,
            until,
            now,
            dir,
        })
    }
}

async fn history(
    State(app): State<App>,
    Query(query): Query<RangeQuery>,
) -> Result<Json<History>, ApiError> {
    let scope = query.scope(&app.workspaces, DAY_MS)?;
    let live = app.hub.snapshot().await?.sessions;
    let store = app.store.clone();
    let history =
        tokio::task::spawn_blocking(move || history::history(&store.reader()?, &scope, &live))
            .await
            .map_err(anyhow::Error::from)??;
    Ok(Json(history))
}

async fn analytics(
    State(app): State<App>,
    Query(query): Query<RangeQuery>,
) -> Result<Json<Analytics>, ApiError> {
    let scope = query.scope(&app.workspaces, 7 * DAY_MS)?;
    let tz = query.tz.unwrap_or(0).clamp(-14 * 60, 14 * 60);
    let live = app.hub.snapshot().await?.sessions;
    let store = app.store.clone();
    let analytics = tokio::task::spawn_blocking(move || {
        analytics::analytics(&store.reader()?, &scope, tz, &live)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(analytics))
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
