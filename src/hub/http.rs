//! HTTP API for the web UI and the phone: reads, plus renaming sessions,
//! replying to them and answering their prompts, starting, stopping,
//! forgetting and opening them (`/launches`), and the knowledge layer
//! (`/notes`, `/projects`: list, search, read, write, verify), the
//! devices' tokens (`/tokens`) and how the machines' copies of Claude
//! Code's state stand (`/sync`). Every `/api` route needs
//! `Authorization: Bearer <token>`: the bootstrap token from the
//! configuration or a device's own (see `crate::tokens`). `/act/{key}/…`
//! takes the one-time key of a notification button instead, `/collect`
//! and `/sync/` a collector's token (see `super::collect`,
//! `crate::sync::hub`).

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
    Extension, Json, Router,
    extract::{ConnectInfo, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{delete, get, patch, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{broadcast, oneshot},
};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::warn;
use ts_rs::TS;

use super::{
    collect::{self, Tokens},
    core::{Cmd, Dirs, Hub, LaunchIssue, LaunchReq},
    reports, web,
};
use crate::{
    actions::{self, EFFORTS, MAX_PROMPT, PERMISSION_MODES},
    analytics::{self, Analytics, LimitTracks},
    ask::Answer,
    catalog::{self, Catalog},
    config::HubConfig,
    conversation,
    history::{self, History},
    knowledge::{
        self, Filter, NoteKind, NoteOp, Status, Upsert,
        export::NoteRef,
        projects::{self, Graph, Projects},
    },
    machine::PowerAction,
    messages,
    model::{Activity, Kind, LaunchTarget, Session, Snapshot, TargetWorkspace},
    paths,
    push::{self, Push},
    store::{self, Store},
    sync::{self, SyncView},
    tokens,
    util::{self, now_ms},
    workspace::{
        self as ws, Coverage, HeldAction, HeldView, Preview, Scope, Side, UnitView, WorkspaceStats,
        WorkspaceView, Workspaces,
        hub::{self as wshub, GitState, UnitOp},
    },
};

const DAY_MS: i64 = 86_400_000;
const HOUR_MS: i64 = 3_600_000;
/// Messages shown with a session's conversation.
const SESSION_MESSAGES: usize = 50;
/// Notes in one export.
const MAX_EXPORT: usize = 500;
/// How long a stop waits for the session's host.
const STOP_WAIT: Duration = Duration::from_secs(30);
/// A suspend hands over pending state (up to 12 s) and runs its command
/// (up to 15 s) before the collector answers.
const POWER_WAIT: Duration = Duration::from_secs(40);

#[derive(Clone)]
pub struct App {
    pub(crate) hub: Hub,
    pub(crate) store: Store,
    pub(crate) workspaces: Arc<Workspaces>,
    /// Where phones open the UI.
    pub(crate) public_url: Arc<String>,
    /// Web Push, when it is on.
    push: Option<Arc<Push>>,
    /// The panel's files (see `web`).
    pub web_dir: Arc<PathBuf>,
    /// The reports' files and view links (see `reports`).
    pub(crate) reports: Arc<reports::Reports>,
    /// Ids of tokens revoked since the start; open streams on them end.
    revoked: broadcast::Sender<i64>,
    /// The workspace repositories, when the workspace sync is on.
    git: Option<GitState>,
    /// Every past session, from the synced transcripts.
    catalog: Catalog,
}

/// What `auth` checks a bearer against.
#[derive(Clone)]
pub(crate) struct Guard {
    /// The configured token (`http.token_file`): accepted as is.
    bootstrap: Arc<String>,
    /// The devices' tokens, hashed.
    store: Store,
}

/// Who a request came from; on every `/api` request as an extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Auth {
    Bootstrap,
    Device(i64),
}

impl Auth {
    fn device(self) -> Option<i64> {
        match self {
            Self::Device(id) => Some(id),
            Self::Bootstrap => None,
        }
    }
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
    sync: sync::hub::Mirror,
    git: Option<GitState>,
    reports: Arc<reports::Reports>,
    catalog: Catalog,
) {
    let api = Router::new()
        .route("/snapshot", get(snapshot))
        .route("/sessions/{id}", get(session))
        .route("/sessions/{id}/title", post(rename))
        .route("/sessions/{id}/project", post(set_project))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/messages", post(message))
        .route("/sessions/{id}/asks/{ask}", post(answer))
        .route("/sessions/{id}/stop", post(stop))
        .route("/sessions/{id}/interrupt", post(interrupt))
        .route("/sessions/{id}/resolved", post(resolve).delete(keep_open))
        .route("/sessions/{id}/forget", post(forget))
        .route("/sessions/{id}/open", post(open))
        .route("/machines/{host}", delete(forget_machine))
        .route("/machines/{host}/suspend", post(suspend_machine))
        .route("/machines/{host}/lock", post(lock_machine))
        .route("/machines/{host}/wake", post(wake_machine))
        .route("/launches", post(launch))
        .route("/launch-targets", get(launch_targets))
        .route("/here", get(here))
        .route("/drafts/{key}", put(put_draft).delete(delete_draft))
        .route("/drafts/{key}/edit", post(edit_draft))
        .route("/drafts/{key}/edit/cancel", post(cancel_edit))
        .route("/skills", get(skills))
        .route("/launches/{id}", get(launch_of))
        .route("/past", get(past))
        .route("/past/{id}", get(past_session))
        .route("/past/{id}/conversation", get(past_conversation))
        .route("/prompts", get(prompts))
        .route("/history", get(history))
        .route("/analytics", get(analytics))
        .route("/limits", get(limits))
        .route("/stream", get(stream))
        .route("/projects", get(projects_view))
        .route("/graph", get(graph))
        .route("/notes", get(notes))
        .route("/notes/export", post(export_notes))
        .route("/notes/{workspace}/{slug}", get(note).put(put_note))
        .route("/notes/{workspace}/{slug}/op", post(note_op))
        .route("/reports/{workspace}/{slug}/view", post(reports::mint))
        .route("/reports/{workspace}/{slug}/file", get(reports::file))
        .route("/push", get(push_info))
        .route(
            "/push/subscriptions",
            post(push_subscribe).delete(push_unsubscribe),
        )
        .route("/push/test", post(push_test))
        .route("/push/prefs", put(push_prefs))
        .route("/tokens", get(devices).post(mint))
        .route("/tokens/{id}", patch(relabel).delete(revoke))
        .route("/sync", get(sync_view))
        .route("/sync/{host}/resync", post(sync_resync))
        .route("/sync/conflicts/{id}", delete(sync_dismiss))
        .route("/sync/conflicts/{id}/resolve", post(sync_resolve))
        .route("/sync/stats", get(sync_stats))
        .route("/workspace", get(workspace_view))
        .route("/workspace/stats", get(workspace_stats))
        .route("/workspace/coverage", get(workspace_coverage))
        .route("/workspace/config", put(workspace_config))
        .route("/workspace/config/preview", post(workspace_preview))
        .route("/workspace/units/{*unit}", post(workspace_unit))
        .route("/workspace/history/{*unit}", get(workspace_history))
        .route("/workspace/held/{host}/{*unit}", post(workspace_held))
        .route("/workspace/conflicts/{id}", post(workspace_conflict))
        .route("/workspace/conflicts/{id}/blob", get(workspace_blob))
        .route("/workspace/{host}/resync", post(workspace_resync))
        .layer(middleware::from_fn_with_state(
            Guard {
                bootstrap: token,
                store: store.clone(),
            },
            auth,
        ));
    let mut app = Router::new()
        .route("/act/{key}/{action}", post(button))
        .route("/reports/-/view/{key}", get(reports::view))
        .merge(reports::upload_routes(tokens.clone(), reports.max_bytes()))
        .route(
            "/collect",
            get(collect::collect).with_state((
                hub.clone(),
                tokens.clone(),
                sync.changed.subscribe(),
            )),
        )
        .nest("/sync", sync::hub::routes(sync, tokens.clone()));
    if let Some(git) = git.clone() {
        app = app.nest("/git", wshub::routes(git, tokens));
    }
    let app = app
        .nest("/api/v1", api)
        .fallback(get(web::serve))
        .with_state(App {
            hub,
            store,
            workspaces: Arc::new(workspaces),
            public_url: Arc::new(public_url),
            push,
            web_dir: Arc::new(web_dir),
            revoked: broadcast::channel(16).0,
            git,
            reports,
            catalog,
        });
    if let Err(e) = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    {
        warn!("http: {e}");
    }
}

/// The address a request came from: the peer's, or behind a proxy on this
/// machine (the peer is loopback) what the proxy says: `X-Real-IP`, else
/// the last `X-Forwarded-For` entry (the one the proxy appended; earlier
/// ones a client may have sent itself).
pub(crate) fn client_ip(headers: &HeaderMap, peer: std::net::SocketAddr) -> std::net::IpAddr {
    if !peer.ip().is_loopback() {
        return peer.ip();
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(ip) = header("x-real-ip").and_then(|v| v.trim().parse().ok()) {
        return ip;
    }
    header("x-forwarded-for")
        .and_then(|v| v.rsplit(',').next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(peer.ip())
}

/// The machine this request's device sits on: the connected collector
/// that links from the same address, if one does. The panel opens drafts
/// in an editor there.
async fn here(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let host = app.hub.here(client_ip(&headers, peer)).await?;
    Ok(Json(json!({ "host": host })))
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

/// The bootstrap token in constant time, else a device's by its hash.
async fn auth(State(guard): State<Guard>, mut request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let Some(given) = given else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let who = if util::constant_time_eq(given.as_bytes(), guard.bootstrap.as_bytes()) {
        Auth::Bootstrap
    } else {
        match guard.device(&given, user_agent(request.headers())).await {
            Ok(Some(id)) => Auth::Device(id),
            Ok(None) => return StatusCode::UNAUTHORIZED.into_response(),
            Err(e) => {
                warn!("auth: {e:#}");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    };
    request.extensions_mut().insert(who);
    next.run(request).await
}

impl Guard {
    /// The device behind `secret`, if it is a live token; its first use
    /// claims it and its seen stamp moves at most once a minute, written
    /// before the request runs so what it reads is current.
    async fn device(&self, secret: &str, user_agent: Option<String>) -> Result<Option<i64>> {
        let store = self.store.clone();
        let secret = secret.to_owned();
        let now = now_ms();
        let found =
            tokio::task::spawn_blocking(move || tokens::lookup(&store.reader()?, &secret, now))
                .await??;
        let Some(token) = found else {
            return Ok(None);
        };
        let stale = token.expires_at.is_some()
            || token.user_agent != user_agent
            || token
                .last_seen_at
                .is_none_or(|seen| now - seen >= tokens::SEEN_EVERY_MS);
        if stale {
            let id = token.id;
            self.store
                .exec(move |conn| tokens::touch(conn, id, user_agent.as_deref(), now))
                .await??;
        }
        Ok(Some(token.id))
    }
}

fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| util::truncate(ua, 200))
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
    let session = match app.hub.resolve(&id).await? {
        Some(session) => session,
        None => ended(&app, &id)
            .await?
            .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?,
    };
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

/// `POST /sessions/{id}/project`: a project of the session's workspace (a
/// directory under it on some host), or null for the directory's own.
#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct SetProject {
    project: Option<String>,
}

/// Pins a session's project: its notes default there and the project's
/// other sessions are the ones it hears about.
async fn set_project(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(body): Json<SetProject>,
) -> Result<Json<Value>, ApiError> {
    let project = body
        .project
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty());
    let session = app
        .hub
        .set_project(&id, project)
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

/// A session the live state no longer holds, from its row in the store.
async fn ended(app: &App, key: &str) -> Result<Option<Session>, ApiError> {
    let (store, workspaces, key) = (app.store.clone(), app.workspaces.clone(), key.to_owned());
    Ok(tokio::task::spawn_blocking(move || -> Result<_> {
        store::ended_session(&store.reader()?, &key, &workspaces)
    })
    .await
    .map_err(anyhow::Error::from)??)
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchBody {
    /// A connected host.
    host: String,
    workspace: String,
    /// A project of the workspace (as `/projects` lists them) the session
    /// is placed under; none leaves it workspace-wide. The directory is the
    /// workspace's either way.
    #[serde(default)]
    #[ts(optional)]
    project: Option<String>,
    prompt: String,
    /// A model alias or name; the host's default when absent.
    #[serde(default)]
    #[ts(optional)]
    model: Option<String>,
    /// `plan` or `acceptEdits`; the host's default when absent or `default`.
    #[serde(default)]
    #[ts(optional)]
    permission_mode: Option<String>,
    /// `low`, `medium`, `high`, `xhigh` or `max`; the session's own when
    /// absent.
    #[serde(default)]
    #[ts(optional)]
    effort: Option<String>,
    /// An ended session to continue (on its host, in its directory).
    #[serde(default)]
    #[ts(optional)]
    resume: Option<String>,
    /// `resume` may still be running: a background session is stopped
    /// first, then continued (as a copy, when anything about it changes).
    #[serde(default)]
    #[ts(optional)]
    restart: Option<bool>,
    /// An open issue of the workspace the session is started for: it
    /// claims it as soon as it reports, so the board shows it working.
    #[serde(default)]
    #[ts(optional)]
    issue: Option<String>,
}

/// Starts a background session at the workspace's directory under a known
/// project, or continues an ended one (with `restart`, a running background
/// one, stopped first); `202 { launch }`, followed by
/// `launch` updates on the stream. The directory is the workspace's (the
/// harness and direnv live there), never the client's; the project is the
/// session's label.
async fn launch(
    State(app): State<App>,
    Json(body): Json<LaunchBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let refuse = |message: String| ApiError(StatusCode::UNPROCESSABLE_ENTITY, message);
    let prompt = body.prompt.trim().to_owned();
    if prompt.is_empty() {
        return Err(refuse("the prompt is empty".into()));
    }
    if prompt.chars().count() > MAX_PROMPT {
        return Err(refuse(format!(
            "prompts are at most {MAX_PROMPT} characters"
        )));
    }
    let model = body.model.filter(|m| !m.trim().is_empty());
    if let Some(m) = &model
        && !actions::valid_model(m)
    {
        return Err(refuse(format!("unknown model {m:?}")));
    }
    let permission_mode = body
        .permission_mode
        .filter(|m| !m.is_empty() && m != "default");
    if let Some(m) = &permission_mode
        && !PERMISSION_MODES.contains(&m.as_str())
    {
        return Err(refuse(format!("permission mode {m:?} is not offered")));
    }
    let effort = body.effort.filter(|e| !e.is_empty());
    if let Some(e) = &effort
        && !EFFORTS.contains(&e.as_str())
    {
        return Err(refuse(format!("effort {e:?} is not offered")));
    }
    let ws_dir = app
        .workspaces
        .dir(&body.workspace)
        .ok_or_else(|| refuse(format!("unknown workspace {:?}", body.workspace)))?
        .to_owned();
    // The running session to stop before it is continued.
    let mut restart = None;
    let (cwd, resume) = match &body.resume {
        Some(key) => {
            let live = app.hub.resolve(key).await?;
            if let Some(s) = live.as_ref().filter(|s| s.activity != Activity::Gone) {
                if body.restart != Some(true) {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        "the session is still running".into(),
                    ));
                }
                if s.kind != Kind::Background {
                    return Err(refuse(
                        "only a background session restarts from here: change it in its terminal"
                            .into(),
                    ));
                }
                if s.host != body.host {
                    return Err(refuse(format!(
                        "the session runs on {}: it restarts there",
                        s.host
                    )));
                }
                restart = Some(s.id.clone());
            }
            let s = match live {
                Some(s) => s,
                None => match ended(&app, key).await? {
                    Some(s) => s,
                    None => past_stub(&app, key)
                        .await?
                        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))?,
                },
            };
            if s.workspace.is_none() {
                return Err(refuse(format!(
                    "the session ran outside every workspace ({}): resume it in a terminal there",
                    s.cwd
                )));
            }
            if s.host != body.host && !continues_elsewhere(&app, &s.id, &body.host).await? {
                if !app
                    .hub
                    .sync_hosts()
                    .await?
                    .iter()
                    .any(|h| h.host == body.host && h.connected)
                {
                    return Err(ApiError(
                        StatusCode::CONFLICT,
                        format!("{} is not connected", body.host),
                    ));
                }
                return Err(refuse(format!(
                    "the session ran on {}, and {} does not get its transcript through the hub",
                    s.host, body.host
                )));
            }
            if (s.workspace.as_ref(), &s.project) != (Some(&body.workspace), &body.project) {
                return Err(refuse("the session ran in another project".into()));
            }
            (s.cwd, Some(s.id))
        }
        None => {
            let targets = targets_of(&app).await?;
            let host = targets
                .iter()
                .find(|t| t.host == body.host)
                .ok_or_else(|| {
                    ApiError(
                        StatusCode::CONFLICT,
                        format!("{} is not connected", body.host),
                    )
                })?;
            let ws = host
                .workspaces
                .iter()
                .find(|w| w.name == body.workspace)
                .ok_or_else(|| refuse(format!("{} has no directory {ws_dir}", body.host)))?;
            // NOTE: always the workspace root; the project is metadata, applied when the session reports
            match &body.project {
                Some(project) if !ws.projects.contains(project) => {
                    return Err(refuse(format!(
                        "{project:?} is not a known project of {} on {}",
                        body.workspace, body.host
                    )));
                }
                _ => (ws_dir, None),
            }
        }
    };
    let issue = match &body.issue {
        Some(slug) => Some(open_issue(&app, &body.workspace, slug).await?),
        None => None,
    };
    // NOTE: last, so a refused request leaves the session running
    if let Some(id) = restart {
        tokio::time::timeout(
            STOP_WAIT,
            app.hub.session_cmd(|reply| Cmd::Stop { key: id, reply }),
        )
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "the host did not answer the stop: the session was not restarted".into(),
            )
        })??
        .map_err(|e| {
            ApiError(
                StatusCode::CONFLICT,
                format!("the session was not stopped, so nothing changed: {e}"),
            )
        })?;
    }
    let launch = app
        .hub
        .launch(LaunchReq {
            host: body.host,
            workspace: body.workspace,
            project: body.project,
            cwd,
            prompt,
            model,
            permission_mode,
            effort,
            resume,
            issue,
        })
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "launch": launch }))))
}

/// A session only the catalog knows (its transcript is on the hub, the
/// store never saw it run), as an ended session to continue.
async fn past_stub(app: &App, id: &str) -> Result<Option<Session>, ApiError> {
    let (catalog, workspaces, id) = (app.catalog.clone(), app.workspaces.clone(), id.to_owned());
    let found = tokio::task::spawn_blocking(move || -> Result<_> {
        catalog::find(&catalog.reader()?, &workspaces, &Default::default(), &id)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(found.map(|p| {
        let mut s = Session::stub(&p.host, &p.id, &p.cwd);
        s.workspace = p.workspace;
        s.project = p.project;
        s.title = p.title;
        s.activity = Activity::Gone;
        s.activity_since = p.last_at;
        s.started_at = p.first_at;
        s.last_event_at = p.last_at;
        s
    }))
}

/// Ids of the sessions running now.
async fn open_ids(app: &App) -> Result<std::collections::HashSet<String>, ApiError> {
    Ok(app
        .hub
        .snapshot()
        .await?
        .sessions
        .into_iter()
        .filter(|s| s.activity != Activity::Gone)
        .map(|s| s.id)
        .collect())
}

#[derive(Deserialize)]
struct PastQuery {
    /// Words to find in prompts, replies and titles.
    q: Option<String>,
    /// Machines, comma-separated: sessions any of them wrote to.
    host: Option<String>,
    /// The scope's workspaces, comma-separated.
    workspace: Option<String>,
    /// A facet key: `workspace/project`, `workspace/`, `-` (outside).
    project: Option<String>,
    /// Sessions active since then (ms).
    since: Option<i64>,
    sort: Option<catalog::Sort>,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn list_param(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Every past session, filtered, counted, searched and sorted (`PastPage`).
async fn past(
    State(app): State<App>,
    Query(query): Query<PastQuery>,
) -> Result<Json<catalog::Page>, ApiError> {
    let workspaces = list_param(query.workspace.as_deref());
    if let Some(unknown) = workspaces.iter().find(|w| app.workspaces.dir(w).is_none()) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("unknown workspace {unknown:?}"),
        ));
    }
    let q = catalog::Query {
        text: query.q.filter(|q| !q.trim().is_empty()),
        hosts: list_param(query.host.as_deref()),
        workspaces,
        project: query.project.filter(|p| !p.is_empty()),
        since: query.since,
        sort: query.sort.unwrap_or_default(),
        offset: query.offset.unwrap_or(0),
        limit: query.limit.unwrap_or(50),
    };
    let open = open_ids(&app).await?;
    let (catalog, workspaces) = (app.catalog.clone(), app.workspaces.clone());
    let mut page = tokio::task::spawn_blocking(move || -> Result<_> {
        catalog::list(&catalog.reader()?, &workspaces, &open, &q, now_ms())
    })
    .await
    .map_err(anyhow::Error::from)??;
    page.pending = app.catalog.progress.pending();
    page.ready = app.catalog.progress.ready();
    Ok(Json(page))
}

#[derive(Deserialize)]
struct PastSessionQuery {
    /// The list's search, to show where it matched.
    q: Option<String>,
}

/// One past session's overview (`PastDetail`).
async fn past_session(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<PastSessionQuery>,
) -> Result<Json<catalog::PastDetail>, ApiError> {
    let open = open_ids(&app).await?;
    let (catalog, workspaces) = (app.catalog.clone(), app.workspaces.clone());
    tokio::task::spawn_blocking(move || -> Result<_> {
        catalog::detail(
            &catalog.reader()?,
            &workspaces,
            &open,
            &id,
            query.q.as_deref(),
        )
    })
    .await
    .map_err(anyhow::Error::from)??
    .map(Json)
    .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))
}

#[derive(Deserialize)]
struct PastConversationQuery {
    offset: Option<i64>,
    /// A text's id: the page starts a few texts before it.
    around: Option<i64>,
    limit: Option<i64>,
}

/// A page of a past session's prompts and Claude's text (`PastConversation`).
async fn past_conversation(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<PastConversationQuery>,
) -> Result<Json<catalog::Conversation>, ApiError> {
    let catalog = app.catalog.clone();
    tokio::task::spawn_blocking(move || -> Result<_> {
        catalog::conversation(
            &catalog.reader()?,
            &id,
            query.offset,
            query.around,
            query.limit.unwrap_or(60),
        )
    })
    .await
    .map_err(anyhow::Error::from)??
    .map(Json)
    .ok_or(ApiError(StatusCode::NOT_FOUND, "no such session".into()))
}

// --- drafts -----------------------------------------------------------------

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct DraftBody {
    text: String,
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct EditBody {
    /// The connected machine whose editor opens the draft.
    host: String,
    /// The panel's text at that moment, as the editor gets it.
    text: String,
}

fn draft_error(e: String) -> ApiError {
    let code = if e.contains("is not a draft key") || e.contains("at most") {
        StatusCode::UNPROCESSABLE_ENTITY
    } else {
        StatusCode::CONFLICT
    };
    ApiError(code, e)
}

/// Keeps a draft on the hub (an empty text drops it); it reaches every
/// panel through the stream.
async fn put_draft(
    State(app): State<App>,
    Path(key): Path<String>,
    Json(body): Json<DraftBody>,
) -> Result<Json<Value>, ApiError> {
    let draft = app
        .hub
        .draft(&key, &body.text)
        .await?
        .map_err(draft_error)?;
    Ok(Json(json!({ "draft": draft })))
}

async fn delete_draft(
    State(app): State<App>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let draft = app.hub.draft(&key, "").await?.map_err(draft_error)?;
    Ok(Json(json!({ "draft": draft })))
}

/// Opens the draft in an editor on `host` (its `[edit] command`); the draft
/// shows as editing until the editor closes, then carries its text.
async fn edit_draft(
    State(app): State<App>,
    Path(key): Path<String>,
    Json(body): Json<EditBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let draft = app
        .hub
        .edit_draft(&key, &body.host, &body.text)
        .await?
        .map_err(draft_error)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "draft": draft }))))
}

/// Closes the editor open on the draft; the text stays as it was.
async fn cancel_edit(
    State(app): State<App>,
    Path(key): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let draft = app.hub.cancel_edit(&key).await?.map_err(draft_error)?;
    Ok(Json(json!({ "draft": draft })))
}

/// Where sessions can start: connected hosts, the workspaces whose
/// directory they have, and the projects with notes and a directory there.
async fn launch_targets(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "hosts": targets_of(&app).await? })))
}

#[derive(Deserialize)]
struct SkillsQuery {
    host: String,
    /// The session's workspace; without one only the host's own skills.
    workspace: Option<String>,
}

/// What `/` offers in a composer writing to a session on `host` in
/// `workspace`: the host's user and plugin skills plus that workspace's.
async fn skills(
    State(app): State<App>,
    Query(query): Query<SkillsQuery>,
) -> Result<Json<Value>, ApiError> {
    let all = app.hub.host_skills(&query.host).await?;
    let workspace = query.workspace.as_deref().filter(|w| !w.is_empty());
    Ok(Json(json!({ "skills": skills_for(all, workspace) })))
}

/// The host's skills for one workspace: other workspaces' drop, the rest
/// stays, sorted by command.
fn skills_for(all: Vec<crate::model::Skill>, workspace: Option<&str>) -> Vec<crate::model::Skill> {
    let mut skills: Vec<_> = all
        .into_iter()
        .filter(|s| match s.scope {
            crate::model::SkillScope::Workspace => s.workspace.as_deref() == workspace,
            crate::model::SkillScope::User | crate::model::SkillScope::Plugin => true,
        })
        .collect();
    skills.sort();
    skills
}

async fn targets_of(app: &App) -> Result<Vec<LaunchTarget>, ApiError> {
    let hosts = app.hub.host_dirs().await?;
    let (store, names) = (app.store.clone(), app.workspaces.names());
    let projects = tokio::task::spawn_blocking(move || -> Result<Vec<(String, Vec<String>)>> {
        let conn = store.reader()?;
        let now = now_ms();
        names
            .into_iter()
            .map(|ws| Ok((ws.clone(), projects::names(&conn, &ws, now)?)))
            .collect()
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(launch_targets_from(hosts, &projects))
}

/// Each host's workspaces (in the configured order) with the projects
/// that have notes and a directory on that host.
fn launch_targets_from(
    hosts: Vec<(String, Dirs)>,
    projects: &[(String, Vec<String>)],
) -> Vec<LaunchTarget> {
    hosts
        .into_iter()
        .map(|(host, dirs)| LaunchTarget {
            host,
            workspaces: projects
                .iter()
                .filter_map(|(ws, names)| {
                    let here = dirs.get(ws)?;
                    Some(TargetWorkspace {
                        name: ws.clone(),
                        projects: names.iter().filter(|p| here.contains(p)).cloned().collect(),
                    })
                })
                .collect(),
        })
        .collect()
}

/// A launch while the hub keeps it (15 minutes).
async fn launch_of(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let launch = app
        .hub
        .launch_of(&id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such launch".into()))?;
    Ok(Json(json!({ "launch": launch })))
}

/// Stops a running session: `claude stop` for a background one, SIGTERM
/// for an interactive one; answers once its host has.
async fn stop(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let result = tokio::time::timeout(
        STOP_WAIT,
        app.hub.session_cmd(|reply| Cmd::Stop { key: id, reply }),
    )
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            "the host did not answer".into(),
        )
    })??;
    let session = result.map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// Interrupts the session's turn: an open prompt is denied with a stop at
/// once, otherwise Claude stops at its next tool call.
async fn interrupt(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .session_cmd(|reply| Cmd::Interrupt { key: id, reply })
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// Marks a session resolved: Claude gets the wrap-up prompt through its
/// channel, and the hub closes the session when that turn ends.
async fn resolve(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .session_cmd(|reply| Cmd::MarkResolved { key: id, reply })
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// Takes a session's resolved mark back: it stays open after its wrap-up.
async fn keep_open(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .session_cmd(|reply| Cmd::KeepOpen { key: id, reply })
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

/// Marks a session gone on the hub (a ghost, or one on an offline host); a
/// later report from its host brings it back.
async fn forget(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .session_cmd(|reply| Cmd::Forget { key: id, reply })
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "session": session })))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SuspendBody {
    /// Suspend even while sessions there work or wait on a prompt.
    force: bool,
}

/// Suspends a machine whose collector allows it (`[power] suspend`) and
/// answers with its status once it is on its way to sleep.
async fn suspend_machine(
    State(app): State<App>,
    Path(host): Path<String>,
    Json(body): Json<SuspendBody>,
) -> Result<Json<Value>, ApiError> {
    power(app, host, PowerAction::Suspend, body.force).await
}

/// Locks the graphical session of a machine whose collector allows it
/// (`[power] lock`).
async fn lock_machine(
    State(app): State<App>,
    Path(host): Path<String>,
) -> Result<Json<Value>, ApiError> {
    power(app, host, PowerAction::Lock, false).await
}

async fn power(
    app: App,
    host: String,
    action: PowerAction,
    force: bool,
) -> Result<Json<Value>, ApiError> {
    let (reply, rx) = oneshot::channel();
    app.hub
        .send(Cmd::Power {
            host,
            action,
            force,
            reply,
        })
        .await?;
    let status = tokio::time::timeout(POWER_WAIT, rx)
        .await
        .map_err(|_| {
            ApiError(
                StatusCode::GATEWAY_TIMEOUT,
                "the machine did not answer".into(),
            )
        })?
        .context("hub stopped")?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "host": status })))
}

/// Sends wake-on-LAN magic packets to an offline machine.
async fn wake_machine(
    State(app): State<App>,
    Path(host): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (reply, rx) = oneshot::channel();
    app.hub.send(Cmd::Wake { host, reply }).await?;
    let status = rx
        .await
        .context("hub stopped")?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "host": status })))
}

/// Drops an offline machine from the list the hub keeps; it comes back
/// when its collector connects again.
async fn forget_machine(
    State(app): State<App>,
    Path(host): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (reply, rx) = oneshot::channel();
    app.hub.send(Cmd::ForgetHost { host, reply }).await?;
    rx.await
        .context("hub stopped")?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({})))
}

/// Focuses the session's window on its host, or attaches a background
/// session there in a new terminal.
async fn open(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    let session = app
        .hub
        .session_cmd(|reply| Cmd::Open { key: id, reply })
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

/// A notification button, sent by the panel's service worker: the key in
/// the link is the ask's own secret, valid until the prompt closes.
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

// --- devices ----------------------------------------------------------------

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub(crate) struct Label {
    label: String,
}

impl Label {
    fn clean(&self) -> Result<String, ApiError> {
        tokens::label(&self.label).ok_or(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the label is empty".into(),
        ))
    }
}

/// The devices with a token, the caller's flagged `current`; `bootstrap`
/// when the caller is on the configured token instead.
async fn devices(
    State(app): State<App>,
    Extension(who): Extension<Auth>,
) -> Result<Json<Value>, ApiError> {
    let store = app.store.clone();
    let now = now_ms();
    let mut list = tokio::task::spawn_blocking(move || tokens::list(&store.reader()?, now))
        .await
        .map_err(anyhow::Error::from)??;
    for token in &mut list {
        token.current = who == Auth::Device(token.id);
    }
    Ok(Json(json!({
        "tokens": list,
        "bootstrap": who == Auth::Bootstrap,
    })))
}

/// Mints a token for a new device: the address to open (the token in its
/// fragment, shown this once) and its QR code. Unopened, it dies in an
/// hour.
async fn mint(State(app): State<App>, Json(body): Json<Label>) -> Result<Json<Value>, ApiError> {
    let label = body.clean()?;
    let now = now_ms();
    let (token, secret) = app
        .store
        .exec(move |conn| tokens::mint(conn, &label, now))
        .await??;
    let url = util::web_url(&app.public_url, &secret);
    let svg = util::qr_svg(&url)?;
    Ok(Json(json!({ "token": token, "url": url, "svg": svg })))
}

async fn relabel(
    State(app): State<App>,
    Path(id): Path<i64>,
    Json(body): Json<Label>,
) -> Result<Json<Value>, ApiError> {
    let label = body.clean()?;
    let now = now_ms();
    let token = app
        .store
        .exec(move |conn| tokens::relabel(conn, id, &label, now))
        .await??
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such device".into()))?;
    Ok(Json(json!({ "token": token })))
}

/// Revokes a device's token and drops its push subscriptions; a stream it
/// holds open ends.
async fn revoke(State(app): State<App>, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let now = now_ms();
    let subscriptions = app
        .store
        .exec(move |conn| tokens::revoke(conn, id, now))
        .await??
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such device".into()))?;
    let _ = app.revoked.send(id);
    Ok(Json(
        json!({ "revoked": true, "subscriptions": subscriptions }),
    ))
}

/// The open issue a launch names; anything else is refused before the
/// session starts, which would cost a run.
async fn open_issue(app: &App, workspace: &str, slug: &str) -> Result<LaunchIssue, ApiError> {
    let store = app.store.clone();
    let (ws, wanted) = (workspace.to_owned(), slug.to_owned());
    let note = tokio::task::spawn_blocking(move || knowledge::get(&store.reader()?, &ws, &wanted))
        .await
        .map_err(anyhow::Error::from)??
        .filter(|n| n.kind == NoteKind::Issue)
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no issue {slug}")))?;
    if !matches!(note.status, Status::Active | Status::Stale) {
        return Err(ApiError(
            StatusCode::CONFLICT,
            format!("{slug} is already closed"),
        ));
    }
    Ok(LaunchIssue {
        slug: note.slug,
        title: note.title,
    })
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

/// Each limit's current window, for the Now page's tiles.
async fn limits(State(app): State<App>) -> Result<Json<LimitTracks>, ApiError> {
    let snapshot = app.hub.snapshot().await?;
    let Some(live) = snapshot.rate_limits else {
        return Ok(Json(LimitTracks::default()));
    };
    let store = app.store.clone();
    let now = snapshot.now;
    let tracks =
        tokio::task::spawn_blocking(move || analytics::limit_tracks(&store.reader()?, &live, now))
            .await
            .map_err(anyhow::Error::from)??;
    Ok(Json(tracks))
}

async fn stream(State(app): State<App>, Extension(who): Extension<Auth>) -> impl IntoResponse {
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
    let events = tokio_stream::iter(first)
        .chain(updates)
        .map(Ok::<_, Infallible>);
    let mut revoked = app.revoked.subscribe();
    let signed_out = async move {
        if let Auth::Device(id) = who {
            while let Ok(revoked) = revoked.recv().await.or_else(|e| match e {
                broadcast::error::RecvError::Lagged(_) => Ok(-1),
                broadcast::error::RecvError::Closed => Err(()),
            }) {
                if revoked == id {
                    return;
                }
            }
        }
        std::future::pending().await
    };
    // NOTE: Sse says `no-cache`, which leaves it in the HTTP cache: see `store.svelte.ts`
    (
        [(header::CACHE_CONTROL, "no-store")],
        Sse::new(futures_util::StreamExt::take_until(events, signed_out))
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(20))),
    )
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

/// What the Projects pages show: every workspace's live notes grouped by
/// project, the issues with their derived state (from the live sessions'
/// claims and the recent writes), plus the notes the lifecycle pass
/// flagged.
async fn projects_view(
    State(app): State<App>,
    Query(query): Query<NotesQuery>,
) -> Result<Json<Projects>, ApiError> {
    let names = query.workspaces(&app.workspaces)?;
    let sessions = app.hub.snapshot().await?.sessions;
    let store = app.store.clone();
    let now = now_ms();
    let workspaces = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = store.reader()?;
        names
            .iter()
            .map(|ws| projects::build(&conn, ws, &sessions, now))
            .collect::<Result<Vec<_>>>()
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(Projects { workspaces }))
}

#[derive(Deserialize)]
struct GraphQuery {
    workspace: String,
    /// Comma-separated statuses; default active and stale.
    status: Option<String>,
}

/// The notes of one workspace and the links between them.
async fn graph(
    State(app): State<App>,
    Query(query): Query<GraphQuery>,
) -> Result<Json<Graph>, ApiError> {
    app.workspaces.dir(&query.workspace).ok_or(ApiError(
        StatusCode::BAD_REQUEST,
        format!("unknown workspace {:?}", query.workspace),
    ))?;
    let statuses = parse_statuses(query.status.as_deref())?;
    let store = app.store.clone();
    let graph = tokio::task::spawn_blocking(move || {
        projects::graph(&store.reader()?, &query.workspace, &statuses)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(graph))
}

/// `active,stale`; the default without a list.
fn parse_statuses(list: Option<&str>) -> Result<Vec<Status>, ApiError> {
    match list.filter(|s| !s.is_empty()) {
        None => Ok(vec![Status::Active, Status::Stale]),
        Some(list) => list
            .split(',')
            .map(|s| {
                Status::parse(s).ok_or(ApiError(
                    StatusCode::BAD_REQUEST,
                    format!("unknown status {s:?}"),
                ))
            })
            .collect(),
    }
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
    let statuses = parse_statuses(query.status.as_deref())?;
    let limit = query.limit.unwrap_or(30).clamp(1, 200);
    let store = app.store.clone();
    let hits = tokio::task::spawn_blocking(move || -> Result<Vec<knowledge::Hit>> {
        let conn = store.reader()?;
        if let Some(text) = query.q.as_deref().filter(|q| !q.trim().is_empty()) {
            return knowledge::search_all(
                &conn,
                &names,
                &statuses,
                &knowledge::Query {
                    text: Some(text.to_owned()),
                    kinds,
                    project: query.project.clone(),
                    limit: Some(limit),
                    slug: None,
                },
            );
        }
        let mut hits = Vec::new();
        for ws in &names {
            hits.extend(
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
            );
        }
        hits.sort_by_key(|a| std::cmp::Reverse(a.card.updated_at));
        hits.truncate(limit);
        Ok(hits)
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(json!({ "hits": hits })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportBody {
    notes: Vec<NoteRef>,
}

/// The picked notes as a `.zip` to hand to a colleague.
async fn export_notes(
    State(app): State<App>,
    Json(body): Json<ExportBody>,
) -> Result<Response, ApiError> {
    if body.notes.is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "pick some notes".into()));
    }
    if body.notes.len() > MAX_EXPORT {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_EXPORT} notes at a time"),
        ));
    }
    let store = app.store.clone();
    let reports_dir = app.reports.dir().to_path_buf();
    let zip = tokio::task::spawn_blocking(move || {
        knowledge::export::archive(&store.reader()?, &body.notes, Some(&reports_dir))
    })
    .await
    .map_err(anyhow::Error::from)??
    .ok_or(ApiError(
        StatusCode::NOT_FOUND,
        "one of the notes no longer exists".into(),
    ))?;
    let headers = [
        (header::CONTENT_TYPE, "application/zip"),
        (
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"ccdash-notes.zip\"",
        ),
    ];
    Ok((headers, zip).into_response())
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

/// Stores the browser's `PushSubscription` (idempotent per endpoint),
/// tied to the device's token so revoking the token drops it.
async fn push_subscribe(
    State(app): State<App>,
    Extension(who): Extension<Auth>,
    headers: HeaderMap,
    Json(body): Json<push::Subscribe>,
) -> Result<Json<Value>, ApiError> {
    app.push()?;
    body.validate()
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    let user_agent = user_agent(&headers);
    let now = now_ms();
    let subscription = app
        .store
        .exec(move |conn| push::subscribe(conn, &body, user_agent.as_deref(), who.device(), now))
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

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct PushPrefs {
    endpoint: String,
    /// The kinds the device wants (`Category` names); null means every kind.
    categories: Option<Vec<String>>,
}

/// Sets which kinds of notification a subscribed device gets.
async fn push_prefs(
    State(app): State<App>,
    Json(body): Json<PushPrefs>,
) -> Result<Json<Value>, ApiError> {
    app.push()?;
    if let Some(list) = &body.categories
        && let Some(bad) = list
            .iter()
            .find(|c| crate::notify::Category::parse(c).is_none())
    {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("unknown notification kind {bad:?}"),
        ));
    }
    let found = app
        .store
        .exec(move |conn| push::set_categories(conn, &body.endpoint, body.categories.as_deref()))
        .await??;
    if !found {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "the hub does not know this device's subscription; turn push off and on".into(),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

/// Whether `host` can continue `session` that ran elsewhere: it syncs
/// and the hub holds the transcript (the collector pulls it first).
async fn continues_elsewhere(app: &App, session: &str, host: &str) -> Result<bool> {
    let syncs = app
        .hub
        .sync_hosts()
        .await?
        .iter()
        .any(|h| h.host == host && h.connected && h.status.is_some());
    if !syncs {
        return Ok(false);
    }
    let (store, session) = (app.store.clone(), session.to_owned());
    tokio::task::spawn_blocking(move || sync::hub::holds_transcript(&store.reader()?, &session))
        .await
        .context("transcript lookup")?
}

/// The machines' copies of Claude Code's state and the open conflicts.
async fn sync_view(State(app): State<App>) -> Result<Json<SyncView>, ApiError> {
    let hosts = app.hub.sync_hosts().await?;
    let store = app.store.clone();
    let ((files, bytes), conflicts) = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = store.reader()?;
        Ok((sync::hub::totals(&conn)?, sync::hub::conflicts(&conn)?))
    })
    .await
    .context("sync view")??;
    Ok(Json(SyncView {
        hosts,
        files,
        bytes,
        conflicts,
    }))
}

async fn sync_resync(
    State(app): State<App>,
    Path(host): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.hub
        .sync_resync(&host)
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct Resolve {
    keep: sync::Keep,
}

/// Sends the owner's choice to the machine holding the copy; the conflict
/// shows the resolution as pending until that machine answers.
async fn sync_resolve(
    State(app): State<App>,
    Path(id): Path<i64>,
    Json(body): Json<Resolve>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let conflict = app
        .store
        .exec(move |conn| sync::hub::request(conn, id, body.keep))
        .await??
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    let sent = app
        .hub
        .send_to(
            &conflict.host,
            crate::protocol::ToCollector::SyncResolve {
                id,
                path: conflict.path.clone(),
                copy: conflict.copy.clone(),
                keep: body.keep,
            },
        )
        .await?;
    if let Err(e) = sent {
        let error = e.clone();
        let host = conflict.host.clone();
        app.store
            .exec(move |conn| sync::hub::resolved(conn, &host, id, Some(&error), None, now_ms()))
            .await??;
        return Err(ApiError(StatusCode::CONFLICT, e));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))))
}

/// Sizes and growth of Claude Code's state on the hub.
async fn sync_stats(State(app): State<App>) -> Result<Json<sync::SyncStats>, ApiError> {
    let (store, workspaces) = (app.store.clone(), app.workspaces.clone());
    let stats = tokio::task::spawn_blocking(move || -> Result<_> {
        let conn = store.reader()?;
        let cwds: Vec<String> = conn
            .prepare("SELECT DISTINCT cwd FROM sessions")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let labels: std::collections::HashMap<String, String> = cwds
            .iter()
            .filter_map(|cwd| {
                let (ws, project) = workspaces.locate(cwd)?;
                let label = project.map_or_else(|| ws.clone(), |p| format!("{ws}/{p}"));
                Some((format!("projects/{}", sync::hub::dir_name(cwd)), label))
            })
            .collect();
        let roots: Vec<(String, String)> = workspaces
            .entries()
            .into_iter()
            .map(|w| (sync::hub::dir_name(&w.dir), w.name))
            .collect();
        sync::hub::stats(&conn, &labels, &roots)
    })
    .await
    .context("sync stats")??;
    Ok(Json(stats))
}

async fn sync_dismiss(
    State(app): State<App>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let found = app
        .store
        .exec(move |conn| sync::hub::dismiss(conn, id, now_ms()))
        .await??;
    if !found {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no open conflict {id}"),
        ));
    }
    Ok(Json(json!({ "ok": true })))
}

// ---- the workspace sync ----

impl App {
    fn git(&self) -> Result<&GitState, ApiError> {
        self.git.as_ref().ok_or_else(|| {
            ApiError(
                StatusCode::NOT_FOUND,
                "the workspace sync is off on this hub (workspace.enable)".into(),
            )
        })
    }
}

/// The Workspace page: hosts, units × hosts, new directories, conflicts,
/// held snapshots, the configuration.
async fn workspace_view(State(app): State<App>) -> Result<Json<WorkspaceView>, ApiError> {
    let git = app.git()?.clone();
    let Some(state) = app.hub.workspace_state().await? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "the workspace sync is off".into(),
        ));
    };
    let store = app.store.clone();
    let (rows, refs, conflicts, new_dirs, bytes, known_hosts, growth) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let conn = store.reader()?;
            Ok((
                wshub::units(&conn)?,
                wshub::refs(&conn)?,
                wshub::conflicts(&conn)?,
                wshub::new_dirs(&conn, &state.config)?,
                git.repos.bytes(),
                wshub::hosts(&conn)?,
                wshub::growth(&conn, 30)?,
            ))
        })
        .await
        .context("workspace view")??;
    let Some(mut state) = app.hub.workspace_state().await? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "the workspace sync is off".into(),
        ));
    };
    // NOTE: a host that pushed before the hub restarted has no report yet
    for host in known_hosts {
        if !state.hosts.iter().any(|h| h.host == host) {
            state.hosts.push(ws::WorkspaceHost {
                host,
                connected: false,
                rtt_ms: None,
                status: None,
            });
        }
    }
    state.hosts.sort_by(|a, b| a.host.cmp(&b.host));
    let mut held = Vec::new();
    let mut units: Vec<UnitView> = rows
        .into_iter()
        .filter(|u| u.state == "included")
        .map(|u| {
            let hosts: std::collections::BTreeMap<String, ws::UnitStatus> = state
                .reports
                .iter()
                .filter_map(|(h, units)| units.get(&u.unit).map(|s| (h.clone(), s.clone())))
                .collect();
            for (h, s) in &hosts {
                if let Some(hd) = &s.held {
                    held.push(HeldView {
                        unit: u.unit.clone(),
                        host: h.clone(),
                        held: hd.clone(),
                    });
                }
            }
            UnitView {
                paused: state.config.units.get(&u.unit).is_some_and(|o| o.paused),
                refs: refs.get(&u.unit).cloned().unwrap_or_default(),
                hosts,
                unit: u.unit,
                workspace: u.workspace,
                kind: u.kind,
                state: u.state,
                size: u.size,
                version: u.version,
                at: u.at,
            }
        })
        .collect();
    units.sort_by(|a, b| a.unit.cmp(&b.unit));
    Ok(Json(WorkspaceView {
        enabled: true,
        hosts: state.hosts,
        units,
        new_dirs,
        conflicts,
        held,
        config: state.config,
        config_version: state.version,
        bytes,
        growth,
    }))
}

#[derive(Deserialize)]
struct StatsQuery {
    /// Milliseconds; the last 30 days when missing.
    #[serde(default)]
    since: Option<i64>,
    /// Milliseconds; now when missing.
    #[serde(default)]
    until: Option<i64>,
}

/// Traffic, cost and growth of the workspace sync over a span (the last
/// 30 days by default, a year at most).
async fn workspace_stats(
    State(app): State<App>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<WorkspaceStats>, ApiError> {
    let git = app.git()?.clone();
    let store = app.store.clone();
    let now = now_ms();
    let until = query.until.unwrap_or(now);
    let since = query.since.unwrap_or(until - 30 * 86_400_000);
    if since >= until {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "since must be before until".into(),
        ));
    }
    let stats = tokio::task::spawn_blocking(move || -> Result<_> {
        let repos = git.repos.sizes();
        wshub::stats(&store.reader()?, since, until, repos, now)
    })
    .await
    .context("workspace stats")??;
    Ok(Json(stats))
}

/// What travels and what stays home, per machine and per directory.
async fn workspace_coverage(State(app): State<App>) -> Result<Json<Coverage>, ApiError> {
    let git = app.git()?.clone();
    let Some(state) = app.hub.workspace_state().await? else {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "the workspace sync is off".into(),
        ));
    };
    let store = app.store.clone();
    let coverage = tokio::task::spawn_blocking(move || -> Result<_> {
        let hub_bytes = git.repos.bytes();
        wshub::coverage(&store.reader()?, &state.config, &state.reports, hub_bytes)
    })
    .await
    .context("workspace coverage")??;
    Ok(Json(coverage))
}

/// What a proposed configuration would decide for every directory the hub
/// knows: the panel shows it while the owner edits.
async fn workspace_preview(
    State(app): State<App>,
    Json(body): Json<WorkspaceConfigBody>,
) -> Result<Json<Preview>, ApiError> {
    app.git()?;
    let store = app.store.clone();
    let preview = tokio::task::spawn_blocking(move || -> Result<_> {
        wshub::preview(&store.reader()?, &body.config)
    })
    .await
    .context("workspace preview")??;
    Ok(Json(preview))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceConfigBody {
    config: ws::WorkspaceConfig,
}

/// Replaces the configuration; collectors get the new setup at once.
async fn workspace_config(
    State(app): State<App>,
    Json(body): Json<WorkspaceConfigBody>,
) -> Result<Json<Value>, ApiError> {
    app.git()?;
    if body.config.depth == 0 || body.config.depth > 6 {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "depth must be 1 to 6".into(),
        ));
    }
    let version = app
        .store
        .exec(move |conn| wshub::save_config(conn, &body.config, now_ms()))
        .await??;
    app.hub.send(Cmd::WorkspaceSetupChanged).await?;
    Ok(Json(json!({ "version": version })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceUnitBody {
    /// `include`, `exclude`, `ignore`, `pause`, `resume`, `adopt`, `override`.
    op: String,
    /// For `adopt`: the host holding the copy.
    #[serde(default)]
    host: Option<String>,
    /// For `override`: the unit's overrides, whole (the defaults remove it).
    #[serde(default)]
    value: Option<ws::UnitOverride>,
}

/// Include, exclude, ignore, pause or resume a unit, set its overrides,
/// or make a host adopt its copy.
async fn workspace_unit(
    State(app): State<App>,
    Path(unit): Path<String>,
    Json(body): Json<WorkspaceUnitBody>,
) -> Result<Json<Value>, ApiError> {
    app.git()?;
    if !ws::valid_unit(&unit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("bad unit {unit:?}"),
        ));
    }
    if body.op == "adopt" {
        let host = body.host.clone().ok_or_else(|| {
            ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                "adopt needs a host".into(),
            )
        })?;
        app.hub
            .send_to(
                &host,
                crate::protocol::ToCollector::WorkspaceAdopt { unit: Some(unit) },
            )
            .await?
            .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
        return Ok(Json(json!({ "ok": true })));
    }
    let op = UnitOp {
        op: body.op,
        value: body.value,
    };
    let unit2 = unit.clone();
    let changed = app
        .store
        .exec(move |conn| wshub::unit_op(conn, &unit2, &op, now_ms()))
        .await??
        .map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e))?;
    if changed {
        app.hub.send(Cmd::WorkspaceSetupChanged).await?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// The last snapshots of a unit across hosts.
async fn workspace_history(
    State(app): State<App>,
    Path(unit): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let git = app.git()?.clone();
    if !ws::valid_unit(&unit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("bad unit {unit:?}"),
        ));
    }
    let entries = tokio::task::spawn_blocking(move || git.repos.history(&unit))
        .await
        .context("history")??;
    Ok(Json(json!({ "history": entries })))
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceHeldBody {
    action: HeldAction,
    /// The held commit the panel showed; the host acts only on that one.
    #[serde(default)]
    commit: Option<String>,
}

/// Applies or drops a held snapshot on its host.
async fn workspace_held(
    State(app): State<App>,
    Path((host, unit)): Path<(String, String)>,
    Json(body): Json<WorkspaceHeldBody>,
) -> Result<Json<Value>, ApiError> {
    app.git()?;
    if !ws::valid_unit(&unit) {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("bad unit {unit:?}"),
        ));
    }
    let answer = app
        .hub
        .workspace_held(&host, &unit, body.action, body.commit)
        .await?;
    // NOTE: the collector answers at once (it queues the action between
    // passes); no answer in five seconds is a stalled collector
    match tokio::time::timeout(Duration::from_secs(5), answer).await {
        Ok(Ok(Ok(()))) => Ok(Json(json!({ "ok": true }))),
        Ok(Ok(Err(e))) => Err(ApiError(StatusCode::CONFLICT, e)),
        Ok(Err(_)) => Err(ApiError(
            StatusCode::CONFLICT,
            "the hub dropped the request".into(),
        )),
        Err(_) => Err(ApiError(
            StatusCode::GATEWAY_TIMEOUT,
            format!("{host} has not answered; it may be stalled: check its collector"),
        )),
    }
}

#[derive(Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceConflictBody {
    /// `mine`, `theirs` or `dismiss`.
    action: String,
}

/// Resolves a conflict on its host (one side wins) or dismisses it.
async fn workspace_conflict(
    State(app): State<App>,
    Path(id): Path<i64>,
    Json(body): Json<WorkspaceConflictBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    app.git()?;
    let side = match body.action.as_str() {
        "mine" => Side::Mine,
        "theirs" => Side::Theirs,
        "dismiss" => {
            let found = app
                .store
                .exec(move |conn| wshub::dismiss_conflict(conn, id, now_ms()))
                .await??;
            if !found {
                return Err(ApiError(
                    StatusCode::NOT_FOUND,
                    format!("no open conflict {id}"),
                ));
            }
            return Ok((StatusCode::OK, Json(json!({ "ok": true }))));
        }
        other => {
            return Err(ApiError(
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("unknown action {other:?}"),
            ));
        }
    };
    let conflict = app
        .store
        .exec(move |conn| wshub::request_conflict(conn, id, side))
        .await??
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    let blob = match side {
        Side::Mine => conflict.ours.clone(),
        Side::Theirs => conflict.theirs.clone(),
    };
    let sent = app
        .hub
        .send_to(
            &conflict.host,
            crate::protocol::ToCollector::WorkspaceResolve {
                id,
                unit: conflict.unit.clone(),
                path: conflict.path.clone(),
                blob,
            },
        )
        .await?;
    if let Err(e) = sent {
        let (error, host) = (e.clone(), conflict.host.clone());
        app.store
            .exec(move |conn| wshub::conflict_resolved(conn, &host, id, Some(&error), now_ms()))
            .await??;
        return Err(ApiError(StatusCode::CONFLICT, e));
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))))
}

#[derive(Deserialize)]
struct BlobQuery {
    side: Side,
}

/// One side of a conflict, as the hub's repository has it.
async fn workspace_blob(
    State(app): State<App>,
    Path(id): Path<i64>,
    Query(q): Query<BlobQuery>,
) -> Result<Response, ApiError> {
    let git = app.git()?.clone();
    let store = app.store.clone();
    let conflict = tokio::task::spawn_blocking(move || wshub::conflict(&store.reader()?, id))
        .await
        .context("conflict")??
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, format!("no open conflict {id}")))?;
    let oid = match q.side {
        Side::Mine => conflict.ours,
        Side::Theirs => conflict.theirs,
    };
    if oid.is_empty() {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "that side has no file".into(),
        ));
    }
    let unit = conflict.unit;
    let data = tokio::task::spawn_blocking(move || git.repos.blob(&unit, &oid))
        .await
        .context("blob")?
        .map_err(|e| ApiError(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
}

#[derive(Deserialize, TS, Default)]
#[ts(export_to = "types.ts")]
#[serde(deny_unknown_fields, default)]
pub(crate) struct WorkspaceResyncBody {
    unit: Option<String>,
}

/// Asks a host to fetch and snapshot everything (or one unit) again.
async fn workspace_resync(
    State(app): State<App>,
    Path(host): Path<String>,
    Json(body): Json<WorkspaceResyncBody>,
) -> Result<Json<Value>, ApiError> {
    app.git()?;
    app.hub
        .send_to(
            &host,
            crate::protocol::ToCollector::WorkspaceResync { unit: body.unit },
        )
        .await?
        .map_err(|e| ApiError(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({ "ok": true })))
}

pub(crate) struct ApiError(pub(crate) StatusCode, pub(crate) String);

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            warn!("api {}: {}", self.0, self.1);
        }
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /who` on a router behind `auth`: the status and who it saw.
    async fn who(addr: std::net::SocketAddr, bearer: Option<String>) -> (u16, String) {
        tokio::task::spawn_blocking(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .http_status_as_error(false)
                .build()
                .into();
            let mut req = agent.get(format!("http://{addr}/who"));
            if let Some(bearer) = bearer {
                req = req.header("Authorization", &format!("Bearer {bearer}"));
            }
            let res = req.call().unwrap();
            let status = res.status().as_u16();
            (status, res.into_body().read_to_string().unwrap_or_default())
        })
        .await
        .unwrap()
    }

    #[test]
    fn targets_are_noted_projects_with_a_directory_on_the_host() {
        let hosts = vec![
            (
                "navi".to_owned(),
                Dirs::from([
                    (
                        "tsandrini".to_owned(),
                        vec!["ccdash".into(), "femtimer".into()],
                    ),
                    ("meteopress".to_owned(), Vec::new()),
                ]),
            ),
            ("flat".to_owned(), Dirs::new()),
        ];
        let projects = vec![
            ("meteopress".to_owned(), vec!["radar".to_owned()]),
            (
                "tsandrini".to_owned(),
                vec!["ccdash".to_owned(), "tsandrini".to_owned()],
            ),
            ("PesekMudra".to_owned(), vec!["site".to_owned()]),
        ];
        let targets = launch_targets_from(hosts, &projects);
        assert_eq!(
            targets[0],
            LaunchTarget {
                host: "navi".into(),
                workspaces: vec![
                    TargetWorkspace {
                        name: "meteopress".into(),
                        projects: Vec::new()
                    },
                    TargetWorkspace {
                        name: "tsandrini".into(),
                        projects: vec!["ccdash".into()]
                    },
                ],
            }
        );
        assert!(targets[1].workspaces.is_empty());
    }

    #[tokio::test]
    async fn bearers_are_the_bootstrap_token_or_a_live_device() {
        let dir = std::env::temp_dir().join(format!("ccdash-auth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir.join("t.db"), 30).unwrap();
        let guard = Guard {
            bootstrap: Arc::new("boot".into()),
            store: store.clone(),
        };
        let app = Router::new()
            .route(
                "/who",
                get(|Extension(who): Extension<Auth>| async move { format!("{who:?}") }),
            )
            .layer(middleware::from_fn_with_state(guard, auth));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (device, secret) = store
            .exec(|conn| tokens::mint(conn, "Phone", now_ms()))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(who(addr, None).await.0, 401);
        assert_eq!(who(addr, Some("nope".into())).await.0, 401);
        assert_eq!(
            who(addr, Some("boot".into())).await,
            (200, "Bootstrap".into())
        );
        assert_eq!(
            who(addr, Some(secret.clone())).await,
            (200, format!("Device({})", device.id))
        );
        // the first use claimed the pairing link
        let mut claimed = None;
        for _ in 0..100 {
            let token = tokens::get(&store.reader().unwrap(), device.id)
                .unwrap()
                .unwrap();
            if token.expires_at.is_none() {
                claimed = Some(token);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let claimed = claimed.expect("claimed");
        assert!(claimed.last_seen_at.is_some());
        assert!(claimed.user_agent.is_some());

        // revoked: out at the next request, the bootstrap token unaffected
        let id = device.id;
        store
            .exec(move |conn| tokens::revoke(conn, id, now_ms()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(who(addr, Some(secret)).await.0, 401);
        assert_eq!(who(addr, Some("boot".into())).await.0, 200);

        // an unopened link dies after its hour
        let (late, secret) = store
            .exec(|conn| tokens::mint(conn, "Lost", now_ms() - tokens::PAIRING_MS - 1))
            .await
            .unwrap()
            .unwrap();
        assert!(late.expires_at.is_some());
        assert_eq!(who(addr, Some(secret)).await.0, 401);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod skills_tests {
    use super::skills_for;
    use crate::model::{Skill, SkillScope};

    fn skill(command: &str, scope: SkillScope, workspace: Option<&str>) -> Skill {
        Skill {
            command: command.into(),
            description: String::new(),
            scope,
            workspace: workspace.map(str::to_owned),
        }
    }

    #[test]
    fn a_composer_sees_its_workspace_s_skills_only() {
        let all = vec![
            skill("sys-operativa", SkillScope::Workspace, Some("meteopress")),
            skill("retro", SkillScope::User, None),
            skill("ccdash:wrap-up", SkillScope::Plugin, None),
            skill("nixvim", SkillScope::Workspace, Some("tsandrini")),
        ];
        let names = |ws: Option<&str>| -> Vec<String> {
            skills_for(all.clone(), ws)
                .into_iter()
                .map(|s| s.command)
                .collect()
        };
        assert_eq!(
            names(Some("meteopress")),
            ["ccdash:wrap-up", "retro", "sys-operativa"]
        );
        assert_eq!(names(None), ["ccdash:wrap-up", "retro"]);
    }
}
