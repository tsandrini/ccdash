//! HTTP API for the web UI and the phone: reads, plus renaming sessions,
//! replying to them and answering their prompts.
//! Every `/api` route needs `Authorization: Bearer <token>`. `/act/{key}/…`
//! takes the one-time key of a notification button instead.

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
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_stream::{Stream, StreamExt, wrappers::BroadcastStream};
use tracing::warn;

use super::web;
use crate::{
    analytics::{self, Analytics},
    ask::Answer,
    config::Config,
    conversation,
    history::{self, History},
    hub::Hub,
    model::{Activity, Snapshot},
    paths,
    store::Store,
    util::{self, now_ms},
    workspace::{Scope, Workspaces},
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;

#[derive(Clone)]
struct App {
    hub: Hub,
    store: Store,
    workspaces: Arc<Workspaces>,
    token: Arc<String>,
    port: u16,
}

pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    store: Store,
    token: Arc<String>,
    workspaces: Workspaces,
) {
    let port = listener.local_addr().map_or(7837, |a| a.port());
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/sessions/{id}", get(session))
        .route("/sessions/{id}/title", post(rename))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/asks/{ask}", post(answer))
        .route("/pair", get(pair))
        .route("/history", get(history))
        .route("/analytics", get(analytics))
        .route("/stream", get(stream))
        .layer(middleware::from_fn_with_state(token.clone(), auth));
    let app = Router::new()
        .route("/", get(web::index))
        .route("/{asset}", get(web::asset))
        .route("/act/{key}/{action}", post(button))
        .nest("/api/v1", api)
        .with_state(App {
            hub,
            store,
            workspaces: Arc::new(workspaces),
            token,
            port,
        });
    if let Err(e) = axum::serve(listener, app).await {
        warn!("http: {e}");
    }
}

/// Reads the configured token file, or the generated one in the state dir
/// (creating it on first use).
pub fn load_token(cfg: &Config) -> Result<String> {
    if let Some(path) = &cfg.http.token_file {
        let path = std::path::PathBuf::from(util::expand_env(&path.to_string_lossy()));
        let token = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
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
    if given.is_some_and(|given| util::constant_time_eq(given.as_bytes(), token.as_bytes())) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
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
    let live = session.activity != Activity::Gone;
    let end = if live {
        now_ms()
    } else {
        session.activity_since
    };
    let limit = query.limit.unwrap_or(10).clamp(1, 50);
    let (conversation, stats, resources) = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = store.reader()?;
        let conversation = conversation::conversation(&conn, &session_id, query.before, limit)?;
        let (stats, resources) = match query.before {
            None => (
                Some(conversation::stats(
                    &conn,
                    &session_id,
                    started_at,
                    end,
                    live,
                )?),
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

#[derive(Deserialize)]
struct Rename {
    title: String,
}

/// Renames a session; Claude Code adopts the title at its next prompt.
async fn rename(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Rename>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .rename(&id, &body.title)
        .await?
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    Ok(Json(json!({ "session": session })))
}

#[derive(Deserialize)]
struct Reply {
    text: String,
}

/// Sends a message to a session through its ccdash channel.
async fn reply(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Reply>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .reply(&id, &body.text)
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// Answers one of the session's open prompts; the terminal dialog closes
/// with it.
async fn answer(
    State(app): State<App>,
    Path((id, ask)): Path<(String, String)>,
    Json(answer): Json<Answer>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .answer(&id, &ask, answer)
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// A notification button, sent by the phone's ntfy app: the key in the
/// link is the ask's own secret, valid until the prompt closes.
async fn button(
    State(app): State<App>,
    Path((key, action)): Path<(String, String)>,
) -> Result<&'static str, ApiError> {
    app.hub
        .button(&key, &action)
        .await?
        .map_err(|e| ApiError(StatusCode::GONE, e))?;
    Ok("done")
}

/// The address a phone on the LAN opens, token included, and its QR code.
async fn pair(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let lan = util::lan_address();
    let host = lan.map_or_else(util::hostname, |ip| ip.to_string());
    let url = util::web_url(&host, app.port, &app.token);
    let svg = util::qr_svg(&url)?;
    Ok(Json(
        json!({ "url": url, "svg": svg, "lan": lan.is_some() }),
    ))
}

/// Time range and workspace of a history or analytics request.
#[derive(Deserialize)]
struct RangeQuery {
    since: Option<i64>,
    /// Defaults to now; never later.
    until: Option<i64>,
    /// Comma-separated workspace names from the snapshot's list.
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
        let dirs = match &self.workspace {
            None => None,
            Some(names) => Some(
                names
                    .split(',')
                    .map(|name| {
                        workspaces.dir(name).map(str::to_owned).ok_or(ApiError(
                            StatusCode::BAD_REQUEST,
                            format!("unknown workspace {name:?}"),
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        };
        Ok(Scope {
            since,
            until,
            now,
            dirs,
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
    let snapshot = app.hub.snapshot().await?;
    let (store, workspaces) = (app.store.clone(), app.workspaces.clone());
    let analytics = tokio::task::spawn_blocking(move || {
        analytics::analytics(
            &store.reader()?,
            &scope,
            tz,
            &snapshot.sessions,
            snapshot.rate_limits.as_ref(),
            &workspaces,
        )
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
