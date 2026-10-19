//! HTTP API for the web UI and the phone: reads, plus renaming sessions,
//! replying to them and answering their prompts, and the knowledge layer
//! (`/notes`, `/projects`: list, search, read, write, verify).
//! Every `/api` route needs `Authorization: Bearer <token>`. `/act/{key}/…`
//! takes the one-time key of a notification button instead, `/collect`
//! a collector's token (see `super::collect`).

use std::{
    convert::Infallible,
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
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
use ts_rs::TS;

use super::{
    collect::{self, Tokens},
    core::Hub,
    web,
};
use crate::{
    analytics::{self, Analytics},
    ask::Answer,
    config::HubConfig,
    conversation,
    history::{self, History},
    knowledge::{self, Card, Filter, NoteKind, NoteOp, Status, Upsert},
    messages,
    model::{Activity, Snapshot},
    paths,
    push::{self, Push},
    store::Store,
    util::{self, now_ms},
    workspace::{Scope, Workspaces},
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
/// Messages shown with a session's conversation.
const SESSION_MESSAGES: usize = 50;

#[derive(Clone)]
pub struct App {
    hub: Hub,
    store: Store,
    workspaces: Arc<Workspaces>,
    token: Arc<String>,
    /// Where phones open the UI.
    public_url: Arc<String>,
    /// Web Push, when it is on.
    push: Option<Arc<Push>>,
    /// The panel's files (see `web`).
    pub web_dir: Arc<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listener: TcpListener,
    hub: Hub,
    store: Store,
    token: Arc<String>,
    workspaces: Workspaces,
    tokens: Tokens,
    public_url: String,
    push: Option<Arc<Push>>,
    web_dir: PathBuf,
) {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/sessions/{id}", get(session))
        .route("/sessions/{id}/title", post(rename))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/messages", post(message))
        .route("/sessions/{id}/asks/{ask}", post(answer))
        .route("/prompts", get(prompts))
        .route("/pair", get(pair))
        .route("/history", get(history))
        .route("/analytics", get(analytics))
        .route("/stream", get(stream))
        .route("/projects", get(projects))
        .route("/notes", get(notes))
        .route("/notes/{workspace}/{slug}", get(note).put(put_note))
        .route("/notes/{workspace}/{slug}/op", post(note_op))
        .route("/push", get(push_info))
        .route(
            "/push/subscriptions",
            post(push_subscribe).delete(push_unsubscribe),
        )
        .route("/push/test", post(push_test))
        .layer(middleware::from_fn_with_state(token.clone(), auth));
    // NOTE: the previous panel under /v1/ resolves its relative API and
    // button links there
    let app = Router::new()
        .route("/act/{key}/{action}", post(button))
        .route("/v1/act/{key}/{action}", post(button))
        .route(
            "/collect",
            get(collect::collect).with_state((hub.clone(), tokens)),
        )
        .nest("/api/v1", api.clone())
        .nest("/v1/api/v1", api)
        .fallback(get(web::serve))
        .with_state(App {
            hub,
            store,
            workspaces: Arc::new(workspaces),
            token,
            public_url: Arc::new(public_url),
            push,
            web_dir: Arc::new(web_dir),
        });
    if let Err(e) = axum::serve(listener, app).await {
        warn!("http: {e}");
    }
}

/// Reads the configured token file, or the generated one in the state dir
/// (creating it on first use).
pub fn load_token(cfg: &HubConfig) -> Result<String> {
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
    let (conversation, stats, resources, messages) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let conn = store.reader()?;
            let conversation = conversation::conversation(&conn, &session_id, query.before, limit)?;
            let (stats, resources, messages) = match query.before {
                None => (
                    Some(conversation::stats(
                        &conn,
                        &session_id,
                        started_at,
                        end,
                        live,
                    )?),
                    Some(conversation::resources(&conn, &session_id, end - HOUR_MS)?),
                    Some(messages::of_session(&conn, &session_id, SESSION_MESSAGES)?),
                ),
                Some(_) => (None, None, None),
            };
            Ok((conversation, stats, resources, messages))
        })
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(json!({
        "session": session,
        "conversation": conversation,
        "stats": stats,
        "resources": resources,
        "messages": messages,
    })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct Rename {
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

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct Reply {
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

/// Leaves a message for the session's next prompt: context, not a turn.
async fn message(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<Reply>,
) -> Result<Json<Value>, ApiError> {
    let message = app
        .hub
        .message(&id, &body.text)
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "message": message })))
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

#[derive(Deserialize)]
struct PromptsQuery {
    /// One session (id, short id or prefix); else every session in scope.
    session: Option<String>,
    /// Comma-separated workspace names; every workspace when empty.
    workspace: Option<String>,
    limit: Option<usize>,
}

/// Recent prompts, newest first and de-duplicated, for the composer's
/// history: `session=` narrows to one session, `workspace=` to workspaces.
async fn prompts(
    State(app): State<App>,
    Query(query): Query<PromptsQuery>,
) -> Result<Json<Value>, ApiError> {
    let dirs = match query.workspace.as_deref().filter(|w| !w.is_empty()) {
        None => None,
        Some(names) => Some(
            serde_json::to_string(
                &names
                    .split(',')
                    .map(|name| {
                        app.workspaces.dir(name).map(str::to_owned).ok_or(ApiError(
                            StatusCode::BAD_REQUEST,
                            format!("unknown workspace {name:?}"),
                        ))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .map_err(anyhow::Error::from)?,
        ),
    };
    let session = match query.session.filter(|s| !s.is_empty()) {
        None => None,
        Some(key) => Some(app.hub.resolve(&key).await?.map_or(key, |s| s.id)),
    };
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let store = app.store.clone();
    let prompts = tokio::task::spawn_blocking(move || {
        conversation::prompts(&store.reader()?, session.as_deref(), dirs.as_deref(), limit)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "prompts": prompts })))
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

/// The address a phone opens, token included, and its QR code.
async fn pair(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let url = util::web_url(&app.public_url, &app.token);
    let svg = util::qr_svg(&url)?;
    Ok(Json(json!({ "url": url, "svg": svg, "lan": true })))
}

/// Time range and workspace of a history or analytics request.
#[derive(Deserialize)]
struct RangeQuery {
    since: Option<i64>,
    /// Defaults to now; never later.
    until: Option<i64>,
    /// Comma-separated workspace names from the snapshot's list.
    workspace: Option<String>,
    /// One host name from the snapshot's list.
    host: Option<String>,
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
            host: self.host.clone().filter(|h| !h.is_empty()),
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

// --- knowledge --------------------------------------------------------------

#[derive(Deserialize)]
struct NotesQuery {
    /// Comma-separated workspace names; every workspace when empty.
    workspace: Option<String>,
    /// Free text; without it the newest notes are listed.
    q: Option<String>,
    kind: Option<String>,
    project: Option<String>,
    /// Comma-separated statuses; default active and stale.
    status: Option<String>,
    limit: Option<usize>,
}

impl NotesQuery {
    fn workspaces(&self, all: &Workspaces) -> Result<Vec<String>, ApiError> {
        match self.workspace.as_deref().filter(|w| !w.is_empty()) {
            None => Ok(all.names()),
            Some(names) => names
                .split(',')
                .map(|name| {
                    all.dir(name).map(|_| name.to_owned()).ok_or(ApiError(
                        StatusCode::BAD_REQUEST,
                        format!("unknown workspace {name:?}"),
                    ))
                })
                .collect(),
        }
    }
}

/// What the Projects page shows: every workspace's live notes grouped by
/// project, plus the notes the lifecycle pass flagged.
async fn projects(
    State(app): State<App>,
    Query(query): Query<NotesQuery>,
) -> Result<Json<Value>, ApiError> {
    let names = query.workspaces(&app.workspaces)?;
    let store = app.store.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Value> {
        let conn = store.reader()?;
        let mut workspaces = Vec::new();
        for ws in names {
            let cards = knowledge::list(
                &conn,
                &ws,
                &Filter {
                    statuses: &[Status::Active, Status::Stale],
                    ..Filter::default()
                },
            )?;
            let closed = knowledge::list(
                &conn,
                &ws,
                &Filter {
                    kinds: &[NoteKind::Issue],
                    statuses: &[Status::Archived],
                    limit: 20,
                    ..Filter::default()
                },
            )?;
            let flagged: Vec<&Card> = cards.iter().filter(|c| c.status == Status::Stale).collect();
            let mut by_project: std::collections::BTreeMap<Option<String>, Vec<&Card>> =
                std::collections::BTreeMap::new();
            for card in cards.iter().filter(|c| c.status == Status::Active) {
                by_project.entry(card.project.clone()).or_default().push(card);
            }
            let projects: Vec<Value> = by_project
                .into_iter()
                .map(|(project, cards)| {
                    let of = |kind: NoteKind| -> Vec<&Card> {
                        cards.iter().copied().filter(|c| c.kind == kind).collect()
                    };
                    let mut issues = of(NoteKind::Issue);
                    issues.sort_by(|a, b| b.priority.cmp(&a.priority).then(b.updated_at.cmp(&a.updated_at)));
                    json!({
                        "project": project,
                        "docs": of(NoteKind::Doc),
                        "issues": issues,
                        "progress": of(NoteKind::Progress).into_iter().take(8).collect::<Vec<_>>(),
                        "memories": of(NoteKind::Memory),
                        "closed": closed.iter().filter(|c| c.project == project).collect::<Vec<_>>(),
                    })
                })
                .collect();
            workspaces.push(json!({
                "name": ws,
                "projects": projects,
                "flagged": flagged,
                "total": cards.len(),
            }));
        }
        Ok(json!({ "workspaces": workspaces }))
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(out))
}

/// Search or list notes across the given workspaces.
async fn notes(
    State(app): State<App>,
    Query(query): Query<NotesQuery>,
) -> Result<Json<Value>, ApiError> {
    let names = query.workspaces(&app.workspaces)?;
    let kinds: Vec<NoteKind> = match query.kind.as_deref().filter(|k| !k.is_empty()) {
        None => Vec::new(),
        Some(k) => vec![NoteKind::parse(k).ok_or(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown kind {k:?}"),
        ))?],
    };
    let statuses: Vec<Status> = match query.status.as_deref().filter(|s| !s.is_empty()) {
        None => vec![Status::Active, Status::Stale],
        Some(list) => list
            .split(',')
            .map(|s| {
                Status::parse(s).ok_or(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("unknown status {s:?}"),
                ))
            })
            .collect::<Result<_, _>>()?,
    };
    let limit = query.limit.unwrap_or(30).clamp(1, 200);
    let store = app.store.clone();
    let hits = tokio::task::spawn_blocking(move || -> Result<Vec<knowledge::Hit>> {
        let conn = store.reader()?;
        let mut hits = Vec::new();
        for ws in &names {
            match query.q.as_deref().filter(|q| !q.trim().is_empty()) {
                Some(text) => hits.extend(knowledge::search(
                    &conn,
                    ws,
                    &knowledge::Query {
                        text: Some(text.to_owned()),
                        kinds: kinds.clone(),
                        project: query.project.clone(),
                        limit: Some(limit.min(50)),
                        slug: None,
                    },
                )?),
                None => hits.extend(
                    knowledge::list(
                        &conn,
                        ws,
                        &Filter {
                            kinds: &kinds,
                            statuses: &statuses,
                            project: query.project.as_deref(),
                            limit,
                        },
                    )?
                    .into_iter()
                    .map(|card| knowledge::Hit {
                        snippet: card.excerpt.clone(),
                        card,
                    }),
                ),
            }
        }
        if query.q.is_none() {
            hits.sort_by_key(|a| std::cmp::Reverse(a.card.updated_at));
        }
        hits.truncate(limit);
        Ok(hits)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "hits": hits })))
}

/// One note with its rendered body, links and backlinks.
async fn note(
    State(app): State<App>,
    Path((workspace, slug)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = app.store.clone();
    let view =
        tokio::task::spawn_blocking(move || knowledge::view(&store.reader()?, &workspace, &slug))
            .await
            .map_err(anyhow::Error::from)??
            .ok_or(ApiError(StatusCode::NOT_FOUND, "no such note".into()))?;
    let html = knowledge::render_body(&view.note.body);
    let mut value = serde_json::to_value(&view).map_err(anyhow::Error::from)?;
    value["html"] = json!(html);
    Ok(Json(value))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct PutNote {
    kind: NoteKind,
    title: String,
    body: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    status: Option<Status>,
    #[serde(default)]
    #[ts(type = "Record<string, unknown> | null")]
    extra: Option<Value>,
    /// The version the edit is based on; refused when the note moved on.
    #[serde(default)]
    base_version: Option<u32>,
}

/// Creates or replaces a note from the panel's editor.
async fn put_note(
    State(app): State<App>,
    Path((workspace, slug)): Path<(String, String)>,
    Json(body): Json<PutNote>,
) -> Result<Json<Value>, ApiError> {
    app.workspaces.dir(&workspace).ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        format!("unknown workspace {workspace:?}"),
    ))?;
    let now = now_ms();
    let (ws, s) = (workspace.clone(), slug.clone());
    let result = app
        .store
        .exec(move |conn| -> Result<Result<knowledge::Note, String>> {
            let old = knowledge::get(conn, &ws, &s)?;
            if let (Some(old), Some(base)) = (&old, body.base_version)
                && old.version != base
            {
                return Ok(Err(format!(
                    "someone changed this note (version {}, you edited {base}); reload and retry",
                    old.version
                )));
            }
            let up = Upsert {
                slug: s.clone(),
                kind: body.kind,
                project: body.project.filter(|p| !p.is_empty()),
                title: body.title,
                summary: body.summary.filter(|p| !p.trim().is_empty()),
                status: body
                    .status
                    .or_else(|| old.as_ref().map(|o| o.status))
                    .unwrap_or(Status::Active),
                extra: body
                    .extra
                    .or_else(|| old.as_ref().map(|o| o.extra.clone()))
                    .unwrap_or_else(|| json!({})),
                body: body.body,
                links: Vec::new(),
                created_at: None,
                updated_at: None,
                verified: false,
            };
            knowledge::upsert(conn, &ws, &up, Some("panel"), now)?;
            Ok(Ok(knowledge::get(conn, &ws, &s)?.context("note vanished")?))
        })
        .await??;
    let note = result.map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    app.hub.notes_changed(vec![note.clone()]).await?;
    Ok(Json(json!({ "note": note })))
}

/// Runs a tool operation (verify, issue_update, …) from the panel on the
/// note; `slug` in the path wins over one in the body.
async fn note_op(
    State(app): State<App>,
    Path((workspace, slug)): Path<(String, String)>,
    Json(mut op): Json<NoteOp>,
) -> Result<Json<Value>, ApiError> {
    app.workspaces.dir(&workspace).ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        format!("unknown workspace {workspace:?}"),
    ))?;
    match &mut op {
        NoteOp::Verify { slug: s }
        | NoteOp::IssueUpdate { slug: s, .. }
        | NoteOp::DocWrite { slug: s, .. } => *s = slug.clone(),
        NoteOp::Remember { slug: s, .. } => *s = Some(slug.clone()),
        NoteOp::IssueAdd { .. } | NoteOp::Progress { .. } => {}
    }
    let now = now_ms();
    let ws = workspace.clone();
    let result = app
        .store
        .exec(
            move |conn| -> Result<(knowledge::Written, Vec<knowledge::Note>)> {
                let w = knowledge::apply(conn, &ws, None, Some("panel"), &op, now)?;
                let mut notes = Vec::new();
                for s in [
                    Some(w.slug.clone()),
                    match &op {
                        NoteOp::Remember { supersedes, .. } => supersedes.clone(),
                        _ => None,
                    },
                ]
                .into_iter()
                .flatten()
                {
                    if let Some(n) = knowledge::get(conn, &ws, &s)? {
                        notes.push(n);
                    }
                }
                Ok((w, notes))
            },
        )
        .await?
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let (written, notes) = result;
    app.hub.notes_changed(notes).await?;
    Ok(Json(json!({ "written": written })))
}

// --- web push ---------------------------------------------------------------

impl App {
    fn push(&self) -> Result<&Arc<Push>, ApiError> {
        self.push.as_ref().ok_or(ApiError(
            StatusCode::CONFLICT,
            "web push is off in the hub's configuration".into(),
        ))
    }
}

/// Whether push is on, the key the browser subscribes with, and the
/// devices subscribed so far.
async fn push_info(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let store = app.store.clone();
    let subscriptions = tokio::task::spawn_blocking(move || push::subscriptions(&store.reader()?))
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(json!({
        "enabled": app.push.is_some(),
        "key": app.push.as_ref().map(|p| p.public_key()),
        "subscriptions": subscriptions,
    })))
}

/// Stores the browser's `PushSubscription` (idempotent per endpoint).
async fn push_subscribe(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<push::Subscribe>,
) -> Result<Json<Value>, ApiError> {
    app.push()?;
    body.validate()
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| util::truncate(ua, 200));
    let now = now_ms();
    let subscription = app
        .store
        .exec(move |conn| push::subscribe(conn, &body, user_agent.as_deref(), now))
        .await??;
    Ok(Json(json!({ "subscription": subscription })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct Unsubscribe {
    endpoint: String,
}

async fn push_unsubscribe(
    State(app): State<App>,
    Json(body): Json<Unsubscribe>,
) -> Result<Json<Value>, ApiError> {
    let removed = app
        .store
        .exec(move |conn| push::unsubscribe(conn, &body.endpoint))
        .await??;
    Ok(Json(json!({ "removed": removed })))
}

/// Sends a plain message to every subscribed device, right away.
async fn push_test(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let push = app.push()?;
    let results = push
        .send_all(&push::test_payload(&app.public_url), false, "test")
        .await?;
    push::log_failures(&results);
    let results: Vec<Value> = results
        .into_iter()
        .map(|(sub, outcome)| {
            let mut v = serde_json::to_value(outcome).unwrap_or_default();
            v["id"] = sub.id.into();
            v["endpoint"] = sub.endpoint.into();
            v["user_agent"] = sub.user_agent.into();
            v
        })
        .collect();
    Ok(Json(json!({ "results": results })))
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
