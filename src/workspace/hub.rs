//! The hub's side of the workspace sync: one bare repository per unit
//! under `sync.dir/workspace/`, served over git smart HTTP (`/git/…`,
//! `git http-backend` as CGI behind the collector tokens), a pre-receive
//! guard that makes merge pushes compare-and-swap on the other hosts'
//! tips, the tables behind the panel (units, refs, conflicts, events, the
//! configuration, what each host sees), and the nightly `git gc`.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{BufRead, Write as _},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Extension, Router,
    body::{Body, Bytes},
    extract::{Path as AxPath, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::StreamExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, warn};

use super::{
    ConflictReport, Cost, CostRow, Coverage, CoverageHost, CoverageUnit, Decision, GlobMatch,
    HistoryEntry, HomeDir, LeftOutName, NewDir, Preview, PushRow, RepoSize, SYNC_REFS, SeenUnit,
    Side, TrafficRow, Transfer, UnitKind, UnitStatus, WorkspaceConfig, WorkspaceConflict,
    WorkspaceSetup, WorkspaceStats, valid_host, valid_unit,
};
use crate::{
    fsx,
    hub::{collect::Tokens, core::Cmd},
    util::now_ms,
};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS workspace_units (
    unit TEXT PRIMARY KEY,
    workspace TEXT NOT NULL,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    version INTEGER NOT NULL DEFAULT 0,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_refs (
    unit TEXT NOT NULL,
    host TEXT NOT NULL,
    ref TEXT NOT NULL,
    commit_id TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (unit, host, ref)
);
CREATE TABLE IF NOT EXISTS workspace_conflicts (
    id INTEGER PRIMARY KEY,
    unit TEXT NOT NULL,
    host TEXT NOT NULL,
    path TEXT NOT NULL,
    kind TEXT NOT NULL,
    ours TEXT NOT NULL,
    theirs TEXT NOT NULL,
    other_host TEXT NOT NULL,
    at INTEGER NOT NULL,
    resolved_at INTEGER,
    how TEXT,
    pending TEXT,
    error TEXT
);
CREATE INDEX IF NOT EXISTS workspace_conflicts_open ON workspace_conflicts (unit, host)
    WHERE resolved_at IS NULL;
CREATE TABLE IF NOT EXISTS workspace_events (
    id INTEGER PRIMARY KEY,
    unit TEXT NOT NULL,
    host TEXT NOT NULL,
    kind TEXT NOT NULL,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS workspace_events_at ON workspace_events (at);
CREATE TABLE IF NOT EXISTS workspace_config (
    scope TEXT PRIMARY KEY,
    json TEXT NOT NULL,
    version INTEGER NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_seen (
    host TEXT NOT NULL,
    unit TEXT NOT NULL,
    kind TEXT NOT NULL,
    size INTEGER NOT NULL,
    selected INTEGER NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (host, unit)
);
CREATE TABLE IF NOT EXISTS workspace_traffic (
    host TEXT NOT NULL,
    hour INTEGER NOT NULL,
    kind TEXT NOT NULL,
    requests INTEGER NOT NULL DEFAULT 0,
    bytes_in INTEGER NOT NULL DEFAULT 0,
    bytes_out INTEGER NOT NULL DEFAULT 0,
    ms INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host, hour, kind)
);
CREATE TABLE IF NOT EXISTS workspace_cost (
    host TEXT NOT NULL,
    hour INTEGER NOT NULL,
    cpu_ms INTEGER NOT NULL DEFAULT 0,
    git_cpu_ms INTEGER NOT NULL DEFAULT 0,
    git_calls INTEGER NOT NULL DEFAULT 0,
    git_wall_ms INTEGER NOT NULL DEFAULT 0,
    io_read INTEGER NOT NULL DEFAULT 0,
    io_write INTEGER NOT NULL DEFAULT 0,
    passes INTEGER NOT NULL DEFAULT 0,
    snapshots INTEGER NOT NULL DEFAULT 0,
    pushes INTEGER NOT NULL DEFAULT 0,
    fetches INTEGER NOT NULL DEFAULT 0,
    push_ms INTEGER NOT NULL DEFAULT 0,
    fetch_ms INTEGER NOT NULL DEFAULT 0,
    failures INTEGER NOT NULL DEFAULT 0,
    rss_kb INTEGER NOT NULL DEFAULT 0,
    watches INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host, hour)
);
CREATE TABLE IF NOT EXISTS workspace_transfers (
    id INTEGER PRIMARY KEY,
    host TEXT NOT NULL,
    unit TEXT NOT NULL,
    kind TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    ms INTEGER NOT NULL,
    at INTEGER NOT NULL
);
";

/// Columns added after the tables' first release.
pub const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("workspace_seen", "excluded", "TEXT"),
    ("workspace_seen", "unsyncable", "TEXT"),
];

/// The one configuration row.
const SCOPE: &str = "root";
/// A conflict blob the panel may fetch, at most.
const MAX_BLOB: usize = 8 << 20;
/// Snapshots a unit's history lists at most.
const HISTORY_LIMIT: usize = 60;
/// Event rows are kept this long.
const EVENTS_KEEP_MS: i64 = 90 * 86_400_000;
/// Traffic and cost rows are kept this long.
const STATS_KEEP_MS: i64 = 366 * 86_400_000;
/// Large transfers kept for the throughput view.
const TRANSFERS_KEEP: i64 = 300;
/// A transfer this large counts as a measurement of the link.
pub const TRANSFER_MIN: u64 = 64 << 10;
/// How long the repositories' sizes are reused.
const SIZES_TTL: Duration = Duration::from_secs(300);
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

// ---------------------------------------------------------------------------
// repositories

/// The bare repositories.
#[derive(Debug, Clone)]
pub struct Repos {
    pub root: PathBuf,
    hooks: PathBuf,
    /// Two hosts seeding one unit at the same instant both ran `git init`;
    /// one failed with "cannot mkdir". Creation is serialised.
    creating: Arc<std::sync::Mutex<()>>,
    /// Every repository's size, measured at most every `SIZES_TTL` (the
    /// panel polls every few seconds; 4.5 GB of packs is not walked for
    /// each poll).
    sizes: Arc<Mutex<Option<Sizes>>>,
}

/// The repositories' sizes and when they were measured.
type Sizes = (Instant, Vec<RepoSize>);

impl Repos {
    /// Creates the directory and the shared hooks directory with the
    /// pre-receive guard pointing at this executable.
    ///
    /// # Errors
    ///
    /// They cannot be written.
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        fsx::set_mode(root, 0o700)?;
        let hooks = root.join(".hooks");
        fs::create_dir_all(&hooks)?;
        let exe = std::env::current_exe().context("locating the ccdash executable")?;
        let script = format!(
            "#!/bin/sh\n# ccdash: a merge snapshot must build on the other hosts' current tips\nexec {} pre-receive \"$@\"\n",
            shell_quote(&exe.to_string_lossy())
        );
        let hook = hooks.join("pre-receive");
        if fs::read_to_string(&hook).ok().as_deref() != Some(script.as_str()) {
            fs::write(&hook, script)?;
        }
        fsx::set_mode(&hook, 0o755)?;
        Ok(Self {
            root: root.to_owned(),
            hooks,
            creating: Arc::new(std::sync::Mutex::new(())),
            sizes: Arc::new(Mutex::new(None)),
        })
    }

    pub fn path(&self, unit: &str) -> PathBuf {
        self.root.join(format!("{unit}.git"))
    }

    pub fn exists(&self, unit: &str) -> bool {
        self.path(unit).join("HEAD").is_file()
    }

    /// Creates the unit's repository when it is missing: bare, pushable
    /// over HTTP, shallow pushes allowed, auto gc off, the shared hooks.
    ///
    /// # Errors
    ///
    /// git fails.
    pub fn ensure(&self, unit: &str) -> Result<PathBuf> {
        let path = self.path(unit);
        if self.exists(unit) {
            return Ok(path);
        }
        let _one_at_a_time = self
            .creating
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.exists(unit) {
            return Ok(path);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        git_ok(&["init", "-q", "--bare", &path.to_string_lossy()], None)?;
        for (key, value) in [
            ("http.receivepack", "true"),
            ("receive.shallowUpdate", "true"),
            ("receive.denyDeletes", "false"),
            ("receive.denyNonFastForwards", "false"),
            ("gc.auto", "0"),
            ("core.sharedRepository", "umask"),
            ("core.hooksPath", &self.hooks.to_string_lossy()),
        ] {
            git_ok(&["config", key, value], Some(&path))?;
        }
        info!("workspace: created {}", path.display());
        Ok(path)
    }

    /// `refs/sync/<host>` of a unit: host → commit.
    pub fn sync_refs(&self, unit: &str) -> Result<BTreeMap<String, String>> {
        let out = git_ok(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)",
                &format!("{SYNC_REFS}/"),
            ],
            Some(&self.path(unit)),
        )?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let (name, sha) = l.split_once('\0')?;
                let host = name.strip_prefix(&format!("{SYNC_REFS}/"))?;
                (!host.contains('/')).then(|| (host.to_owned(), sha.to_owned()))
            })
            .collect())
    }

    /// The last snapshots of a unit across hosts, newest first.
    pub fn history(&self, unit: &str) -> Result<Vec<HistoryEntry>> {
        let path = self.path(unit);
        if !self.exists(unit) {
            return Ok(Vec::new());
        }
        let refs: Vec<String> = self.sync_refs(unit)?.into_values().collect();
        if refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec![
            "log".to_owned(),
            format!("-n{HISTORY_LIMIT}"),
            "--format=%H%x1f%P%x1f%ct%x1f%b%x1e".to_owned(),
        ];
        args.extend(refs);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = git_ok(&arg_refs, Some(&path))?;
        let mut entries = Vec::new();
        for record in out.split('\x1e') {
            let fields: Vec<&str> = record.trim_start_matches('\n').splitn(4, '\x1f').collect();
            let [commit, parents, ct, body] = fields[..] else {
                continue;
            };
            if commit.is_empty() {
                continue;
            }
            let parents: Vec<&str> = parents.split_whitespace().collect();
            let host = Repos::host_of_body(body, &path, commit);
            let branch = body
                .lines()
                .find_map(|l| l.strip_prefix("branch "))
                .filter(|b| *b != "detached")
                .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
            let files = match parents.first() {
                Some(parent) => git_ok(
                    &[
                        "diff-tree",
                        "-r",
                        "--name-only",
                        "--no-commit-id",
                        parent,
                        commit,
                        "--",
                        "work",
                    ],
                    Some(&path),
                )
                .map(|o| o.lines().filter(|l| !l.is_empty()).count() as u64)
                .unwrap_or(0),
                None => 0,
            };
            entries.push(HistoryEntry {
                commit: commit.to_owned(),
                host,
                at: ct.trim().parse::<i64>().unwrap_or(0) * 1000,
                merge: parents.len() > 1,
                files,
                branch,
            });
        }
        Ok(entries)
    }

    fn host_of_body(body: &str, path: &Path, commit: &str) -> String {
        if let Some(host) = super::snapshot::Meta::host_of(
            &git_ok(&["log", "-1", "--format=%s", commit], Some(path)).unwrap_or_default(),
        ) {
            return host.to_owned();
        }
        let _ = body;
        String::new()
    }

    /// A blob of a unit's repository, for the panel.
    pub fn blob(&self, unit: &str, oid: &str) -> Result<Vec<u8>> {
        if oid.len() < 40 || !oid.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("bad object id");
        }
        let out = std::process::Command::new("git")
            .args(["cat-file", "blob", oid])
            .current_dir(self.path(unit))
            .stdin(Stdio::null())
            .output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        if out.stdout.len() > MAX_BLOB {
            bail!("the file is larger than {} MB", MAX_BLOB >> 20);
        }
        Ok(out.stdout)
    }

    /// Bytes the repositories take.
    pub fn bytes(&self) -> u64 {
        self.sizes().iter().map(|r| r.bytes).sum()
    }

    /// Every repository with its size, largest first; measured at most
    /// every `SIZES_TTL`.
    pub fn sizes(&self) -> Vec<RepoSize> {
        if let Ok(cache) = self.sizes.lock()
            && let Some((at, sizes)) = cache.as_ref()
            && at.elapsed() < SIZES_TTL
        {
            return sizes.clone();
        }
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if !path.is_dir() || entry.file_name() == ".hooks" {
                    continue;
                }
                if path.extension().is_some_and(|e| e == "git") && path.join("HEAD").is_file() {
                    let unit = path
                        .strip_prefix(&self.root)
                        .map(|p| p.to_string_lossy().trim_end_matches(".git").to_owned())
                        .unwrap_or_default();
                    out.push(RepoSize {
                        unit,
                        bytes: super::units::size_of(&path, &[]).1,
                    });
                } else {
                    stack.push(path);
                }
            }
        }
        out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.unit.cmp(&b.unit)));
        if let Ok(mut cache) = self.sizes.lock() {
            *cache = Some((Instant::now(), out.clone()));
        }
        out
    }

    /// The next `sizes()` measures again (a push landed).
    pub fn sizes_stale(&self) {
        if let Ok(mut cache) = self.sizes.lock() {
            *cache = None;
        }
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Runs git on the hub (no token, no excludes needed) and returns stdout.
fn git_ok(args: &[&str], dir: Option<&Path>) -> Result<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args)
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    let out = cmd.output().context("starting git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `ccdash gc`: `git gc --quiet` over every repository under `root`.
///
/// # Errors
///
/// A repository's gc fails (the others still ran).
pub fn gc(root: &Path) -> Result<()> {
    let mut failed = Vec::new();
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.extension().is_some_and(|e| e == "git") && path.join("HEAD").is_file() {
                count += 1;
                if let Err(e) = git_ok(&["gc", "--quiet"], Some(&path)) {
                    failed.push(format!("{}: {e:#}", path.display()));
                }
                prune_incoming(&path.join("objects"));
            } else {
                stack.push(path);
            }
        }
    }
    println!("{count} repositories collected");
    if !failed.is_empty() {
        bail!("{} failed:\n{}", failed.len(), failed.join("\n"));
    }
    Ok(())
}

/// Quarantine directories of pushes that never finished (`incoming-*`,
/// a day old) are removed; `git gc` leaves them alone.
fn prune_incoming(objects: &Path) {
    let day = std::time::Duration::from_secs(86_400);
    for entry in fs::read_dir(objects).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > day);
        if name.starts_with("incoming-") && old {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// The pre-receive guard: on `refs/sync/<host>`, a commit with more than
/// one parent is accepted only if every extra parent is the current tip
/// of another host's ref (the compare-and-swap that stops three hosts
/// merging the same pair at once from ping-ponging), or if the commit is
/// already on the hub as another host's tip or behind it (a snapshot
/// taken over as is). Refused pushes get the tips on stderr as
/// `ccdash-tip <host> <sha>` lines.
pub fn pre_receive() -> i32 {
    let stdin = std::io::stdin();
    let mut updates: Vec<(String, String, String)> = Vec::new();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if let [old, new, name] = parts[..] {
            updates.push((old.to_owned(), new.to_owned(), name.to_owned()));
        }
    }
    let tips = match git_ok(
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)",
            &format!("{SYNC_REFS}/"),
        ],
        None,
    ) {
        Ok(out) => out,
        Err(e) => {
            eprintln!("ccdash-refused: {e:#}");
            return 1;
        }
    };
    let current: HashMap<String, String> = tips
        .lines()
        .filter_map(|l| {
            let (name, sha) = l.split_once('\0')?;
            Some((name.to_owned(), sha.to_owned()))
        })
        .collect();
    let mut stderr = std::io::stderr();
    for (old, new, name) in &updates {
        let Some(host) = name.strip_prefix(&format!("{SYNC_REFS}/")) else {
            continue;
        };
        if host.contains('/') || new.chars().all(|c| c == '0') {
            continue;
        }
        let parents = match git_ok(&["rev-list", "--parents", "-n", "1", new], None) {
            Ok(out) => out
                .split_whitespace()
                .skip(1)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            Err(e) => {
                let _ = writeln!(stderr, "ccdash-refused: {e:#}");
                return 1;
            }
        };
        if parents.len() < 2 {
            continue;
        }
        let others: HashMap<&str, &str> = current
            .iter()
            .filter(|(n, _)| n.as_str() != name)
            .filter_map(|(n, sha)| {
                let h = n.strip_prefix(&format!("{SYNC_REFS}/"))?;
                (!h.contains('/')).then_some((h, sha.as_str()))
            })
            .collect();
        // NOTE: another host's merge commit, taken over as this host's
        // snapshot, is on the hub already (it or a descendant is that host's tip)
        let adopted = others.values().any(|tip| {
            *tip == new
                || std::process::Command::new("git")
                    .args(["merge-base", "--is-ancestor", new, tip])
                    .stdin(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
        });
        if adopted {
            continue;
        }
        for parent in &parents[1..] {
            // NOTE: a parent on the pusher's own old chain is fine (a re-merge)
            if parent == old || others.values().any(|tip| *tip == parent) {
                continue;
            }
            let _ = writeln!(
                stderr,
                "ccdash-refused: {name} merges {} which is not another host's current tip",
                &parent[..8.min(parent.len())]
            );
            for (h, tip) in &others {
                let _ = writeln!(stderr, "ccdash-tip {h} {tip}");
            }
            return 1;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// smart HTTP

/// The host a collector token names, on every `/git/` request.
#[derive(Clone)]
struct Host(String);

/// What the routes share.
#[derive(Clone)]
pub struct GitState {
    pub repos: Arc<Repos>,
    /// Tells the core about a receive-pack that ran.
    pub cmds: mpsc::Sender<Cmd>,
}

/// The `/git/` routes: `/{unit}.git/info/refs`, `/{unit}.git/git-upload-pack`,
/// `/{unit}.git/git-receive-pack`, each behind a collector token.
pub fn routes<S>(state: GitState, tokens: Tokens) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/{*rest}", any(git_route))
        .layer(middleware::from_fn_with_state(tokens, collector_auth))
        .with_state(state)
}

async fn collector_auth(
    State(tokens): State<Tokens>,
    mut request: Request,
    next: Next,
) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let Some(host) = given.and_then(|t| tokens.host_of(&t)) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    request.extensions_mut().insert(Host(host));
    next.run(request).await
}

fn text(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

async fn git_route(
    State(state): State<GitState>,
    Extension(Host(host)): Extension<Host>,
    AxPath(rest): AxPath<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some((unit, tail)) = rest.split_once(".git/") else {
        return text(StatusCode::NOT_FOUND, "not a repository path");
    };
    if !valid_unit(unit) || !valid_host(&host) {
        return text(StatusCode::BAD_REQUEST, "bad unit or host");
    }
    let query = uri.query().unwrap_or_default();
    let service = match (method.as_str(), tail) {
        ("GET", "info/refs") => query
            .split('&')
            .find_map(|kv| kv.strip_prefix("service="))
            .unwrap_or_default()
            .to_owned(),
        ("POST", "git-upload-pack") => "git-upload-pack".into(),
        ("POST", "git-receive-pack") => "git-receive-pack".into(),
        ("GET", "HEAD") => "git-upload-pack".into(),
        _ => return text(StatusCode::NOT_FOUND, "not a smart HTTP endpoint"),
    };
    if !matches!(service.as_str(), "git-upload-pack" | "git-receive-pack") {
        return text(StatusCode::FORBIDDEN, "dumb HTTP is not served");
    }
    let receiving = service == "git-receive-pack";
    let repos = state.repos.clone();
    let unit_owned = unit.to_owned();
    let ready = tokio::task::spawn_blocking(move || {
        if receiving {
            repos.ensure(&unit_owned).map(|_| true)
        } else {
            Ok(repos.exists(&unit_owned))
        }
    })
    .await;
    match ready {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return text(StatusCode::NOT_FOUND, "the hub has no such unit yet"),
        Ok(Err(e)) => {
            warn!("workspace: preparing {unit}: {e:#}");
            return text(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        }
        Err(_) => return text(StatusCode::INTERNAL_SERVER_ERROR, "join"),
    }

    let started = Instant::now();
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &state.repos.root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", format!("/{unit}.git/{tail}"))
        .env("QUERY_STRING", query)
        .env("REQUEST_METHOD", method.as_str())
        .env("REMOTE_USER", &host)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("GATEWAY_INTERFACE", "CGI/1.1")
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .current_dir(&state.repos.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, env) in [
        (header::CONTENT_TYPE, "CONTENT_TYPE"),
        (header::CONTENT_LENGTH, "CONTENT_LENGTH"),
        (header::CONTENT_ENCODING, "HTTP_CONTENT_ENCODING"),
    ] {
        if let Some(v) = headers.get(&name).and_then(|v| v.to_str().ok()) {
            cmd.env(env, v);
        }
    }
    if let Some(v) = headers.get("git-protocol").and_then(|v| v.to_str().ok()) {
        cmd.env("HTTP_GIT_PROTOCOL", v);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            warn!("workspace: starting git http-backend: {e}");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "git is not available");
        }
    };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // the request body streams into the backend
    let feeder = tokio::spawn(async move {
        let mut bytes: u64 = 0;
        let mut stream = body.into_data_stream();
        if let Some(mut stdin) = stdin.take() {
            while let Some(chunk) = stream.next().await {
                let Ok(chunk) = chunk else { break };
                bytes += chunk.len() as u64;
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        }
        bytes
    });
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_string(&mut text).await;
        }
        text
    });

    // NOTE: http-backend prints its headers before the service reads the
    // request; a proxy that sees response headers while it is still sending
    // the body (nginx, HTTP/2) resets the stream. The whole body goes in
    // first; receive-pack and upload-pack write nothing before they have it.
    let bytes = feeder.await.unwrap_or(0);

    // the CGI headers, then the body as it comes
    let Some(stdout) = stdout else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "no stdout");
    };
    let mut reader = BufReader::new(stdout);
    let mut status = StatusCode::OK;
    let mut response_headers = HeaderMap::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                warn!("workspace: reading git http-backend: {e}");
                return text(StatusCode::BAD_GATEWAY, "git http-backend failed");
            }
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("Status") {
            if let Some(code) = value.split(' ').next().and_then(|c| c.parse::<u16>().ok()) {
                status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
            }
        } else if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.trim().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            response_headers.insert(name, value);
        }
    }
    let first = Bytes::copy_from_slice(reader.buffer());
    let mut stdout = reader.into_inner();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let cmds = state.cmds.clone();
    let (unit_owned, host_owned) = (unit.to_owned(), host.clone());
    let failed_status = !status.is_success();
    tokio::spawn(async move {
        let mut bytes_out = first.len() as u64;
        if !first.is_empty() && tx.send(Ok(first)).await.is_err() {
            return;
        }
        let mut buf = vec![0u8; 64 << 10];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    bytes_out += n as u64;
                    if tx
                        .send(Ok(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
        }
        drop(tx);
        let exit = child.wait().await;
        let err_text = errors.await.unwrap_or_default();
        let ok = exit.as_ref().is_ok_and(|s| s.success()) && !failed_status;
        if !ok || !err_text.trim().is_empty() {
            debug!(
                "workspace: git http-backend for {unit_owned} ({service}) exited {exit:?}: {}",
                err_text.trim()
            );
        }
        let _ = cmds
            .send(Cmd::WorkspaceTransfer {
                host: host_owned.clone(),
                unit: unit_owned.clone(),
                kind: if receiving { "push" } else { "fetch" }.into(),
                bytes_in: bytes,
                bytes_out,
                ms: started.elapsed().as_millis() as u64,
                ok,
            })
            .await;
        if receiving && ok && method == Method::POST {
            let _ = cmds
                .send(Cmd::WorkspacePushed {
                    unit: unit_owned,
                    host: host_owned,
                    bytes,
                })
                .await;
        }
    });
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

// ---------------------------------------------------------------------------
// the tables

/// A row of `workspace_units`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    pub unit: String,
    pub workspace: String,
    pub kind: UnitKind,
    /// `included`, `waiting` (over the threshold) or `ignored` (the owner).
    pub state: String,
    pub size: u64,
    pub version: i64,
    pub at: i64,
}

fn kind_name(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Git => "git",
        UnitKind::Plain => "plain",
    }
}

fn kind_of(name: &str) -> UnitKind {
    if name == "plain" {
        UnitKind::Plain
    } else {
        UnitKind::Git
    }
}

/// The configuration and its version; the defaults (version 0) before
/// the first write.
pub fn load_config(conn: &Connection) -> Result<(WorkspaceConfig, i64)> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT json, version FROM workspace_config WHERE scope = ?1",
            [SCOPE],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match row {
        Some((json, version)) => Ok((serde_json::from_str(&json)?, version)),
        None => Ok((WorkspaceConfig::default(), 0)),
    }
}

/// Stores the configuration; the new version.
pub fn save_config(conn: &Connection, config: &WorkspaceConfig, now: i64) -> Result<i64> {
    let (_, version) = load_config(conn)?;
    let version = version + 1;
    conn.execute(
        "INSERT INTO workspace_config (scope, json, version, at) VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT (scope) DO UPDATE SET json = ?2, version = ?3, at = ?4",
        params![SCOPE, serde_json::to_string(config)?, version, now],
    )?;
    Ok(version)
}

/// Every unit row.
pub fn units(conn: &Connection) -> Result<Vec<UnitRow>> {
    Ok(conn
        .prepare_cached(
            "SELECT unit, workspace, kind, state, size, version, at FROM workspace_units ORDER BY unit",
        )?
        .query_map([], |r| {
            Ok(UnitRow {
                unit: r.get(0)?,
                workspace: r.get(1)?,
                kind: kind_of(&r.get::<_, String>(2)?),
                state: r.get(3)?,
                size: r.get::<_, i64>(4)?.try_into().unwrap_or(0),
                version: r.get(5)?,
                at: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// Adds or updates a unit row (the state only when given).
pub fn upsert_unit(
    conn: &Connection,
    unit: &str,
    kind: UnitKind,
    state: Option<&str>,
    size: Option<u64>,
    now: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_units (unit, workspace, kind, state, size, version, at)
        VALUES (?1, ?2, ?3, COALESCE(?4, 'included'), COALESCE(?5, 0), 0, ?6)
        ON CONFLICT (unit) DO UPDATE SET
            kind = ?3,
            state = COALESCE(?4, state),
            size = COALESCE(?5, size),
            at = ?6",
        params![
            unit,
            WorkspaceConfig::bundle_of(unit),
            kind_name(kind),
            state,
            size.map(|s| i64::try_from(s).unwrap_or(0)),
            now
        ],
    )?;
    Ok(())
}

/// The setup collectors run on: the configuration and the units that
/// travel (rows in `included` the globs still select).
pub fn setup(conn: &Connection) -> Result<WorkspaceSetup> {
    let (config, version) = load_config(conn)?;
    let units: BTreeMap<String, UnitKind> = units(conn)?
        .into_iter()
        .filter(|u| u.state == "included" && config.selects(&u.unit))
        .map(|u| (u.unit, u.kind))
        .collect();
    // NOTE: the unit set changes with rows, not only with config writes
    let rows: i64 = conn.query_row(
        "SELECT COALESCE(SUM(version), 0) + COUNT(*) FROM workspace_units",
        [],
        |r| r.get(0),
    )?;
    Ok(WorkspaceSetup {
        version: version * 1_000_000 + rows,
        config,
        units,
    })
}

/// Records what `host` sees at unit depth and decides about the new
/// directories: a selected one under the bundle's threshold is included,
/// a bigger one waits. Returns whether the included set changed.
pub fn seen(conn: &mut Connection, host: &str, list: &[SeenUnit], now: i64) -> Result<bool> {
    let (config, _) = load_config(conn)?;
    let tx = conn.transaction()?;
    let mut changed = false;
    tx.execute("DELETE FROM workspace_seen WHERE host = ?1", [host])?;
    for s in list {
        if !valid_unit(&s.unit) {
            continue;
        }
        tx.execute(
            "INSERT INTO workspace_seen (host, unit, kind, size, selected, at, excluded, unsyncable)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                host,
                s.unit,
                kind_name(s.kind),
                i64::try_from(s.size).unwrap_or(0),
                s.selected,
                now,
                (!s.excluded.is_empty()).then(|| serde_json::to_string(&s.excluded).unwrap_or_default()),
                s.unsyncable,
            ],
        )?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM workspace_units WHERE unit = ?1",
                [&s.unit],
                |r| r.get(0),
            )
            .optional()?;
        if s.unsyncable.is_some() {
            // NOTE: never a unit; one that travelled leaves the setup
            if state.as_deref() != Some("unsyncable") {
                upsert_unit(&tx, &s.unit, s.kind, Some("unsyncable"), Some(s.size), now)?;
                changed = state.as_deref() == Some("included");
            }
            continue;
        }
        if state.is_some() || !config.selects(&s.unit) {
            continue;
        }
        let state = if s.size <= config.auto_include_bytes(WorkspaceConfig::bundle_of(&s.unit)) {
            changed = true;
            "included"
        } else {
            "waiting"
        };
        tx.execute(
            "INSERT INTO workspace_units (unit, workspace, kind, state, size, version, at)
            VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![
                s.unit,
                WorkspaceConfig::bundle_of(&s.unit),
                kind_name(s.kind),
                state,
                i64::try_from(s.size).unwrap_or(0),
                now
            ],
        )?;
    }
    tx.commit()?;
    Ok(changed)
}

/// Directories at unit depth the hub has not included: waiting ones,
/// ignored ones, and the ones the globs leave out.
pub fn new_dirs(conn: &Connection, config: &WorkspaceConfig) -> Result<Vec<NewDir>> {
    let mut by_unit: BTreeMap<String, NewDir> = BTreeMap::new();
    let mut stmt = conn.prepare_cached(
        "SELECT s.host, s.unit, s.kind, s.size, s.at, u.state, s.unsyncable FROM workspace_seen s
        LEFT JOIN workspace_units u ON u.unit = s.unit ORDER BY s.unit, s.host",
    )?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let host: String = r.get(0)?;
        let unit: String = r.get(1)?;
        let state: Option<String> = r.get(5)?;
        let reason: Option<String> = r.get(6)?;
        let state = match state.as_deref() {
            Some("included") if config.selects(&unit) => continue,
            Some("included") => "excluded".to_owned(),
            Some(other) => other.to_owned(),
            None if reason.is_some() => "unsyncable".to_owned(),
            None if config.selects(&unit) => "waiting".to_owned(),
            None => "excluded".to_owned(),
        };
        let entry = by_unit.entry(unit.clone()).or_insert_with(|| NewDir {
            unit,
            kind: kind_of(&r.get::<_, String>(2).unwrap_or_default()),
            size: 0,
            hosts: Vec::new(),
            state,
            at: r.get(4).unwrap_or(0),
            reason: None,
        });
        if entry.reason.is_none() {
            entry.reason = reason;
        }
        entry.size = entry.size.max(r.get::<_, i64>(3)?.try_into().unwrap_or(0));
        entry.hosts.push(host);
    }
    Ok(by_unit.into_values().collect())
}

/// Sets a unit's state (`included`, `waiting`, `ignored`); a unit the hub
/// never saw gets a row.
pub fn set_unit_state(conn: &Connection, unit: &str, state: &str, now: i64) -> Result<()> {
    let kind: Option<String> = conn
        .query_row(
            "SELECT kind FROM workspace_seen WHERE unit = ?1 LIMIT 1",
            [unit],
            |r| r.get(0),
        )
        .optional()?;
    upsert_unit(
        conn,
        unit,
        kind_of(kind.as_deref().unwrap_or("git")),
        Some(state),
        None,
        now,
    )
}

/// Records the refs of a unit after a push: `host` → commit for every
/// `refs/sync/<host>`; the hosts whose ref moved, and the unit's version.
pub fn set_refs(
    conn: &mut Connection,
    unit: &str,
    refs: &BTreeMap<String, String>,
    pusher: &str,
    bytes: u64,
    now: i64,
) -> Result<(Vec<String>, i64)> {
    let tx = conn.transaction()?;
    let before: BTreeMap<String, String> = tx
        .prepare_cached(
            "SELECT host, commit_id FROM workspace_refs WHERE unit = ?1 AND ref = 'sync'",
        )?
        .query_map([unit], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut moved = Vec::new();
    for (host, sha) in refs {
        if before.get(host) != Some(sha) {
            moved.push(host.clone());
            tx.execute(
                "INSERT INTO workspace_refs (unit, host, ref, commit_id, at) VALUES (?1, ?2, 'sync', ?3, ?4)
                ON CONFLICT (unit, host, ref) DO UPDATE SET commit_id = ?3, at = ?4",
                params![unit, host, sha, now],
            )?;
        }
    }
    for host in before.keys() {
        if !refs.contains_key(host) {
            tx.execute(
                "DELETE FROM workspace_refs WHERE unit = ?1 AND host = ?2 AND ref = 'sync'",
                params![unit, host],
            )?;
        }
    }
    let kind: String = tx
        .query_row(
            "SELECT kind FROM workspace_seen WHERE unit = ?1 LIMIT 1",
            [unit],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "git".into());
    tx.execute(
        "INSERT INTO workspace_units (unit, workspace, kind, state, size, version, at)
        VALUES (?1, ?2, ?3, 'included', 0, 1, ?4)
        ON CONFLICT (unit) DO UPDATE SET version = version + 1, at = ?4",
        params![unit, WorkspaceConfig::bundle_of(unit), kind, now],
    )?;
    let version: i64 = tx.query_row(
        "SELECT version FROM workspace_units WHERE unit = ?1",
        [unit],
        |r| r.get(0),
    )?;
    if !moved.is_empty() {
        let merge = moved.len() > 1;
        tx.execute(
            "INSERT INTO workspace_events (unit, host, kind, files, bytes, at) VALUES (?1, ?2, ?3, 0, ?4, ?5)",
            params![
                unit,
                pusher,
                if merge { "merge" } else { "snapshot" },
                i64::try_from(bytes).unwrap_or(0),
                now
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM workspace_events WHERE at < ?1",
        [now - EVENTS_KEEP_MS],
    )?;
    tx.commit()?;
    Ok((moved, version))
}

/// Every unit's `refs/sync/<host>` as the hub last recorded them.
pub fn refs(conn: &Connection) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut stmt =
        conn.prepare_cached("SELECT unit, host, commit_id FROM workspace_refs WHERE ref = 'sync'")?;
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        out.entry(r.get(0)?)
            .or_default()
            .insert(r.get(1)?, r.get(2)?);
    }
    Ok(out)
}

/// Follows what `host` reports as open conflicts in `unit`: new ones get
/// a row, ones no longer reported are resolved (the markers went).
pub fn reconcile_conflicts(
    conn: &mut Connection,
    host: &str,
    unit: &str,
    reported: &[ConflictReport],
    now: i64,
) -> Result<()> {
    let tx = conn.transaction()?;
    let open: Vec<(i64, String, Option<String>)> = tx
        .prepare_cached(
            "SELECT id, path, pending FROM workspace_conflicts
            WHERE host = ?1 AND unit = ?2 AND resolved_at IS NULL",
        )?
        .query_map(params![host, unit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (id, path, pending) in &open {
        if !reported.iter().any(|c| &c.path == path) {
            tx.execute(
                "UPDATE workspace_conflicts SET resolved_at = ?2, pending = NULL,
                    how = COALESCE(how, CASE WHEN ?3 IS NULL THEN 'fixed in the tree' ELSE ?3 END)
                WHERE id = ?1",
                params![id, now, pending],
            )?;
        }
    }
    for c in reported {
        if open.iter().any(|(_, path, _)| path == &c.path) {
            continue;
        }
        tx.execute(
            "INSERT INTO workspace_conflicts (unit, host, path, kind, ours, theirs, other_host, at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                unit,
                host,
                c.path,
                c.kind,
                c.ours,
                c.theirs,
                c.other_host,
                c.at
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

const CONFLICT_COLUMNS: &str =
    "id, unit, host, path, kind, ours, theirs, other_host, at, pending, error";

fn conflict_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceConflict> {
    Ok(WorkspaceConflict {
        id: r.get(0)?,
        unit: r.get(1)?,
        host: r.get(2)?,
        path: r.get(3)?,
        kind: r.get(4)?,
        ours: r.get(5)?,
        theirs: r.get(6)?,
        other_host: r.get(7)?,
        at: r.get(8)?,
        pending: r
            .get::<_, Option<String>>(9)?
            .and_then(|s| serde_json::from_value(serde_json::Value::String(s)).ok()),
        error: r.get(10)?,
    })
}

/// Open conflicts, newest first.
pub fn conflicts(conn: &Connection) -> Result<Vec<WorkspaceConflict>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {CONFLICT_COLUMNS} FROM workspace_conflicts
            WHERE resolved_at IS NULL ORDER BY at DESC LIMIT 500"
        ))?
        .query_map([], conflict_row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// One open conflict.
pub fn conflict(conn: &Connection, id: i64) -> Result<Option<WorkspaceConflict>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {CONFLICT_COLUMNS} FROM workspace_conflicts WHERE id = ?1 AND resolved_at IS NULL"
        ))?
        .query_row([id], conflict_row)
        .optional()?)
}

/// Marks a resolution as on its way to the host.
pub fn request_conflict(
    conn: &Connection,
    id: i64,
    side: Side,
) -> Result<Result<WorkspaceConflict, String>> {
    let Some(c) = conflict(conn, id)? else {
        return Ok(Err(format!("no open conflict {id}")));
    };
    if c.pending.is_some() {
        return Ok(Err("a resolution is already on its way".into()));
    }
    let side = serde_json::to_value(side)?;
    conn.execute(
        "UPDATE workspace_conflicts SET pending = ?2, error = NULL WHERE id = ?1",
        params![id, side.as_str()],
    )?;
    Ok(Ok(c))
}

/// What the host made of a resolution.
pub fn conflict_resolved(
    conn: &Connection,
    host: &str,
    id: i64,
    error: Option<&str>,
    now: i64,
) -> Result<()> {
    match error {
        Some(e) => conn.execute(
            "UPDATE workspace_conflicts SET pending = NULL, error = ?3 WHERE id = ?1 AND host = ?2",
            params![id, host, e],
        )?,
        None => conn.execute(
            "UPDATE workspace_conflicts SET resolved_at = ?3,
                how = CASE pending WHEN 'mine' THEN 'kept this host''s version'
                    WHEN 'theirs' THEN 'took the other version' ELSE how END,
                pending = NULL, error = NULL
            WHERE id = ?1 AND host = ?2",
            params![id, host, now],
        )?,
    };
    Ok(())
}

/// Hides a conflict (the markers stay for the owner to fix in the tree).
pub fn dismiss_conflict(conn: &Connection, id: i64, now: i64) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE workspace_conflicts SET resolved_at = ?2, how = 'dismissed', pending = NULL
        WHERE id = ?1 AND resolved_at IS NULL",
        params![id, now],
    )? > 0)
}

/// `workspace_events` of the last `days` days as (day, bytes) pairs.
pub fn growth(conn: &Connection, days: i64) -> Result<Vec<(String, i64)>> {
    Ok(conn
        .prepare_cached(
            "SELECT date(at / 1000, 'unixepoch', 'localtime') AS day, SUM(bytes)
            FROM workspace_events WHERE at > ?1 GROUP BY day ORDER BY day",
        )?
        .query_map([now_ms() - days * 86_400_000], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// What a host reported last, kept by the core.
#[derive(Debug, Clone, Default)]
pub struct Reported {
    pub status: super::WorkspaceStatus,
    pub units: HashMap<String, UnitStatus>,
    pub at: i64,
}

/// The panel's unit op body.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitOp {
    /// `include`, `exclude`, `ignore`, `pause`, `resume`, `override`.
    pub op: String,
    /// For `override`: the unit's overrides, whole.
    #[serde(default)]
    pub value: Option<super::UnitOverride>,
}

/// Applies a unit op to the configuration and the unit table; whether the
/// setup changed.
pub fn unit_op(
    conn: &Connection,
    unit: &str,
    op: &UnitOp,
    now: i64,
) -> Result<Result<bool, String>> {
    if !valid_unit(unit) {
        return Ok(Err(format!("bad unit {unit:?}")));
    }
    let (mut config, _) = load_config(conn)?;
    match op.op.as_str() {
        "include" => {
            config.units.entry(unit.to_owned()).or_default().include = Some(true);
            set_unit_state(conn, unit, "included", now)?;
        }
        "exclude" => {
            config.units.entry(unit.to_owned()).or_default().include = Some(false);
        }
        "ignore" => {
            set_unit_state(conn, unit, "ignored", now)?;
        }
        "pause" | "resume" => {
            config.units.entry(unit.to_owned()).or_default().paused = op.op == "pause";
        }
        "override" => {
            let Some(value) = &op.value else {
                return Ok(Err("override needs a value".into()));
            };
            if *value == super::UnitOverride::default() {
                config.units.remove(unit);
            } else {
                config.units.insert(unit.to_owned(), value.clone());
            }
        }
        "adopt" => return Ok(Ok(false)),
        other => return Ok(Err(format!("unknown op {other:?}"))),
    }
    save_config(conn, &config, now)?;
    Ok(Ok(true))
}

/// A host's traffic in one direction since the last flush.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficDelta {
    pub requests: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub ms: u64,
    pub failures: u64,
}

/// Adds a host's traffic to its row of the hour.
pub fn add_traffic(
    conn: &Connection,
    host: &str,
    hour: i64,
    kind: &str,
    d: &TrafficDelta,
) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_traffic (host, hour, kind, requests, bytes_in, bytes_out, ms, failures)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ON CONFLICT (host, hour, kind) DO UPDATE SET
            requests = requests + ?4, bytes_in = bytes_in + ?5, bytes_out = bytes_out + ?6,
            ms = ms + ?7, failures = failures + ?8",
        params![
            host,
            hour,
            kind,
            d.requests as i64,
            d.bytes_in as i64,
            d.bytes_out as i64,
            d.ms as i64,
            d.failures as i64
        ],
    )?;
    conn.execute(
        "DELETE FROM workspace_traffic WHERE hour < ?1",
        [hour - STATS_KEEP_MS],
    )?;
    Ok(())
}

/// Adds what a host's sync cost (the deltas of its counters, the gauges
/// as reported) to its row of the hour.
pub fn add_cost(conn: &Connection, host: &str, hour: i64, d: &Cost) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_cost (host, hour, cpu_ms, git_cpu_ms, git_calls, git_wall_ms, io_read,
            io_write, passes, snapshots, pushes, fetches, push_ms, fetch_ms, failures, rss_kb, watches)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
        ON CONFLICT (host, hour) DO UPDATE SET
            cpu_ms = cpu_ms + ?3, git_cpu_ms = git_cpu_ms + ?4, git_calls = git_calls + ?5,
            git_wall_ms = git_wall_ms + ?6, io_read = io_read + ?7, io_write = io_write + ?8,
            passes = passes + ?9, snapshots = snapshots + ?10, pushes = pushes + ?11,
            fetches = fetches + ?12, push_ms = push_ms + ?13, fetch_ms = fetch_ms + ?14,
            failures = failures + ?15, rss_kb = MAX(rss_kb, ?16), watches = ?17",
        params![
            host,
            hour,
            d.cpu_ms as i64,
            d.git_cpu_ms as i64,
            d.git_calls as i64,
            d.git_wall_ms as i64,
            d.io_read as i64,
            d.io_write as i64,
            d.passes as i64,
            d.snapshots as i64,
            d.pushes as i64,
            d.fetches as i64,
            d.push_ms as i64,
            d.fetch_ms as i64,
            d.failures as i64,
            d.rss_kb as i64,
            d.watches as i64
        ],
    )?;
    conn.execute(
        "DELETE FROM workspace_cost WHERE hour < ?1",
        [hour - STATS_KEEP_MS],
    )?;
    Ok(())
}

/// Records a large transfer; the last `TRANSFERS_KEEP` are kept.
pub fn add_transfer(conn: &Connection, t: &Transfer) -> Result<()> {
    conn.execute(
        "INSERT INTO workspace_transfers (host, unit, kind, bytes, ms, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![t.host, t.unit, t.kind, t.bytes as i64, t.ms as i64, t.at],
    )?;
    conn.execute(
        "DELETE FROM workspace_transfers WHERE id <= (SELECT MAX(id) FROM workspace_transfers) - ?1",
        [TRANSFERS_KEEP],
    )?;
    Ok(())
}

/// The SQL that turns a millisecond stamp in `col` into its bucket: the
/// hour as it is, or local midnight for daily buckets.
fn bucket_sql(col: &str, daily: bool) -> String {
    if daily {
        format!(
            "CAST(strftime('%s', date({col} / 1000, 'unixepoch', 'localtime'), 'utc') AS INTEGER) * 1000"
        )
    } else {
        format!("({col} / {HOUR_MS}) * {HOUR_MS}")
    }
}

/// Traffic, cost, transfers, pushes and conflicts between `since` and
/// `until` (clamped to a year, hourly buckets up to three days).
pub fn stats(
    conn: &Connection,
    since: i64,
    until: i64,
    repos: Vec<RepoSize>,
    now: i64,
) -> Result<WorkspaceStats> {
    let until = until.min(now);
    let since = since.clamp(until - 366 * DAY_MS, until - HOUR_MS);
    let daily = until - since > 3 * DAY_MS;
    let bucket = bucket_sql("hour", daily);
    let traffic = conn
        .prepare(&format!(
            "SELECT host, {bucket} AS b, kind, SUM(requests), SUM(bytes_in), SUM(bytes_out), SUM(ms), SUM(failures)
            FROM workspace_traffic WHERE hour >= ?1 AND hour < ?2 GROUP BY host, b, kind ORDER BY b, host, kind"
        ))?
        .query_map([since, until], |r| {
            Ok(TrafficRow {
                host: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                requests: r.get::<_, i64>(3)? as u64,
                bytes_in: r.get::<_, i64>(4)? as u64,
                bytes_out: r.get::<_, i64>(5)? as u64,
                ms: r.get::<_, i64>(6)? as u64,
                failures: r.get::<_, i64>(7)? as u64,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let cost = conn
        .prepare(&format!(
            "SELECT host, {bucket} AS b, SUM(cpu_ms), SUM(git_cpu_ms), SUM(git_calls), SUM(git_wall_ms),
                SUM(io_read), SUM(io_write), SUM(passes), SUM(snapshots), SUM(pushes), SUM(fetches),
                SUM(push_ms), SUM(fetch_ms), SUM(failures), MAX(rss_kb), MAX(watches)
            FROM workspace_cost WHERE hour >= ?1 AND hour < ?2 GROUP BY host, b ORDER BY b, host"
        ))?
        .query_map([since, until], |r| {
            let g = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, i64>(i)? as u64) };
            Ok(CostRow {
                host: r.get(0)?,
                at: r.get(1)?,
                cpu_ms: g(2)?,
                git_cpu_ms: g(3)?,
                git_calls: g(4)?,
                git_wall_ms: g(5)?,
                io_read: g(6)?,
                io_write: g(7)?,
                passes: g(8)?,
                snapshots: g(9)?,
                pushes: g(10)?,
                fetches: g(11)?,
                push_ms: g(12)?,
                fetch_ms: g(13)?,
                failures: g(14)?,
                rss_kb: g(15)?,
                watches: g(16)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let transfers = conn
        .prepare("SELECT host, unit, kind, bytes, ms, at FROM workspace_transfers WHERE at >= ?1 AND at < ?2 ORDER BY at DESC LIMIT 100")?
        .query_map([since, until], |r| {
            Ok(Transfer {
                host: r.get(0)?,
                unit: r.get(1)?,
                kind: r.get(2)?,
                bytes: r.get::<_, i64>(3)? as u64,
                ms: r.get::<_, i64>(4)? as u64,
                at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let event_bucket = bucket_sql("at", daily);
    let pushes = conn
        .prepare(&format!(
            "SELECT host, {event_bucket} AS b, COUNT(*), SUM(bytes) FROM workspace_events
            WHERE at >= ?1 AND at < ?2 GROUP BY host, b ORDER BY b, host"
        ))?
        .query_map([since, until], |r| {
            Ok(PushRow {
                host: r.get(0)?,
                at: r.get(1)?,
                pushes: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let conflicts = conn
        .prepare(&format!(
            "SELECT {event_bucket} AS b, COUNT(*) FROM workspace_conflicts WHERE at >= ?1 AND at < ?2 GROUP BY b ORDER BY b"
        ))?
        .query_map([since, until], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(WorkspaceStats {
        since,
        until,
        bucket_ms: if daily { DAY_MS } else { HOUR_MS },
        traffic,
        cost,
        transfers,
        pushes,
        conflicts,
        hub_bytes: repos.iter().map(|r| r.bytes).sum(),
        repos,
    })
}

/// One row of `workspace_seen`, with what the host measured.
struct SeenRow {
    host: String,
    unit: String,
    kind: UnitKind,
    size: u64,
    excluded: BTreeMap<String, u64>,
    unsyncable: Option<String>,
    state: Option<String>,
}

fn seen_rows(conn: &Connection) -> Result<Vec<SeenRow>> {
    Ok(conn
        .prepare_cached(
            "SELECT s.host, s.unit, s.kind, s.size, s.excluded, s.unsyncable, u.state
            FROM workspace_seen s LEFT JOIN workspace_units u ON u.unit = s.unit
            ORDER BY s.unit, s.host",
        )?
        .query_map([], |r| {
            Ok(SeenRow {
                host: r.get(0)?,
                unit: r.get(1)?,
                kind: kind_of(&r.get::<_, String>(2)?),
                size: r.get::<_, i64>(3)?.try_into().unwrap_or(0),
                excluded: r
                    .get::<_, Option<String>>(4)?
                    .and_then(|j| serde_json::from_str(&j).ok())
                    .unwrap_or_default(),
                unsyncable: r.get(5)?,
                state: r.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// What `config` decides for every directory the hub knows, folded over
/// the hosts that see it (the largest copy's sizes).
fn decide(rows: &[SeenRow], config: &WorkspaceConfig) -> Vec<CoverageUnit> {
    let mut by_unit: BTreeMap<String, CoverageUnit> = BTreeMap::new();
    for r in rows {
        let decision = config.explain(&r.unit);
        let selected = matches!(decision, Decision::Selected { .. } | Decision::ForcedIn);
        let status = match (r.unsyncable.as_deref(), r.state.as_deref()) {
            (Some(_), _) => "unsyncable",
            (None, Some("unsyncable")) => "unsyncable",
            (None, Some("included")) if selected => "travels",
            (None, Some("waiting")) if selected => "waiting",
            (None, Some("ignored")) => "ignored",
            _ => "left_out",
        };
        let entry = by_unit
            .entry(r.unit.clone())
            .or_insert_with(|| CoverageUnit {
                unit: r.unit.clone(),
                kind: r.kind,
                hosts: Vec::new(),
                size: 0,
                excluded: BTreeMap::new(),
                status: status.to_owned(),
                decision: decision.clone(),
                reason: r.unsyncable.clone(),
            });
        entry.hosts.push(r.host.clone());
        entry.size = entry.size.max(r.size);
        for (name, bytes) in &r.excluded {
            let slot = entry.excluded.entry(name.clone()).or_insert(0);
            *slot = (*slot).max(*bytes);
        }
        if entry.reason.is_none() {
            entry.reason = r.unsyncable.clone();
        }
        if status == "unsyncable" {
            entry.status = status.to_owned();
        }
    }
    by_unit.into_values().collect()
}

/// What travels and what stays home, from what every host sees and what
/// the travelling units' collectors report as ignored directories.
pub fn coverage(
    conn: &Connection,
    config: &WorkspaceConfig,
    reports: &HashMap<String, HashMap<String, UnitStatus>>,
    hub_bytes: u64,
) -> Result<Coverage> {
    let rows = seen_rows(conn)?;
    let units = decide(&rows, config);
    let travels: BTreeSet<&str> = units
        .iter()
        .filter(|u| u.status == "travels")
        .map(|u| u.unit.as_str())
        .collect();
    let mut hosts: BTreeMap<String, CoverageHost> = BTreeMap::new();
    for r in &rows {
        let h = hosts.entry(r.host.clone()).or_insert_with(|| CoverageHost {
            host: r.host.clone(),
            units: 0,
            bytes: 0,
            travels: 0,
        });
        h.units += 1;
        h.bytes += r.size + r.excluded.values().sum::<u64>();
        if travels.contains(r.unit.as_str()) {
            h.travels += r.size;
        }
    }
    let mut names: BTreeMap<String, LeftOutName> = BTreeMap::new();
    for u in &units {
        for (name, bytes) in &u.excluded {
            let n = names.entry(name.clone()).or_insert_with(|| LeftOutName {
                name: name.clone(),
                bytes: 0,
                units: 0,
            });
            n.bytes += bytes;
            n.units += 1;
        }
    }
    let mut names: Vec<LeftOutName> = names.into_values().collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.bytes));
    // gitignored directories inside travelling units, as reported live
    let mut home: BTreeMap<(String, String), HomeDir> = BTreeMap::new();
    for (host, units) in reports {
        for (unit, status) in units {
            for d in &status.ignored {
                let e = home
                    .entry((unit.clone(), d.path.clone()))
                    .or_insert_with(|| HomeDir {
                        unit: unit.clone(),
                        path: d.path.clone(),
                        bytes: 0,
                        files: 0,
                        hosts: Vec::new(),
                        carried: d.carried,
                    });
                e.bytes = e.bytes.max(d.bytes);
                e.files = e.files.max(d.files);
                if !e.hosts.contains(host) {
                    e.hosts.push(host.clone());
                }
                e.carried = e.carried || d.carried;
            }
        }
    }
    let mut home: Vec<HomeDir> = home.into_values().collect();
    home.sort_by_key(|h| std::cmp::Reverse(h.bytes));
    Ok(Coverage {
        hosts: hosts.into_values().collect(),
        units,
        names,
        home,
        hub_bytes,
    })
}

/// What a proposed configuration would decide for every directory the
/// hub knows, and how many units each glob matches.
pub fn preview(conn: &Connection, config: &WorkspaceConfig) -> Result<Preview> {
    let rows = seen_rows(conn)?;
    let units = decide(&rows, config);
    let ids: BTreeSet<&str> = units.iter().map(|u| u.unit.as_str()).collect();
    let count = |pattern: &str| -> u64 {
        ids.iter()
            .filter(|id| {
                crate::sync::glob(pattern, id)
                    || (!id.contains('/') && crate::sync::glob(pattern, &format!("{id}/")))
            })
            .count() as u64
    };
    let globs = config
        .include
        .iter()
        .map(|p| GlobMatch {
            pattern: p.clone(),
            exclude: false,
            matches: count(p),
        })
        .chain(config.exclude.iter().map(|p| GlobMatch {
            pattern: p.clone(),
            exclude: true,
            matches: count(p),
        }))
        .collect();
    Ok(Preview { units, globs })
}

/// Hosts that appear in `workspace_seen` or `workspace_refs`, for the grid.
pub fn hosts(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for table in ["workspace_seen", "workspace_refs"] {
        let mut stmt = conn.prepare(&format!("SELECT DISTINCT host FROM {table}"))?;
        for host in stmt.query_map([], |r| r.get::<_, String>(0))? {
            out.insert(host?);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::Ignored;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for (table, column, kind) in ADDED_COLUMNS {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))
                .unwrap();
        }
        conn
    }

    fn seen_unit(unit: &str, size: u64, selected: bool) -> SeenUnit {
        SeenUnit {
            unit: unit.into(),
            kind: UnitKind::Git,
            size,
            selected,
            excluded: BTreeMap::new(),
            unsyncable: None,
        }
    }

    #[test]
    fn config_units_and_setup() {
        let mut c = conn();
        let (cfg, v) = load_config(&c).unwrap();
        assert_eq!((v, cfg.depth), (0, 2));
        let cfg = WorkspaceConfig {
            include: vec!["ts/*".into()],
            exclude: vec!["ts/nixpkgs".into()],
            auto_include_mb: 1,
            ..WorkspaceConfig::default()
        };
        assert_eq!(save_config(&c, &cfg, 1).unwrap(), 1);
        assert_eq!(load_config(&c).unwrap().1, 1);

        // what a host sees: small selected → included, big → waiting, unselected → nothing
        let changed = seen(
            &mut c,
            "navi",
            &[
                seen_unit("ts/small", 100, true),
                seen_unit("ts/big", 5 << 20, true),
                seen_unit("ts/nixpkgs", 100, false),
                seen_unit("other/x", 100, false),
            ],
            2,
        )
        .unwrap();
        assert!(changed);
        let rows = units(&c).unwrap();
        let states: Vec<(&str, &str)> = rows
            .iter()
            .map(|u| (u.unit.as_str(), u.state.as_str()))
            .collect();
        assert_eq!(states, [("ts/big", "waiting"), ("ts/small", "included")]);
        let s1 = setup(&c).unwrap();
        assert_eq!(s1.units.keys().collect::<Vec<_>>(), ["ts/small"]);
        let new = new_dirs(&c, &s1.config).unwrap();
        let listed: Vec<(&str, &str)> = new
            .iter()
            .map(|n| (n.unit.as_str(), n.state.as_str()))
            .collect();
        assert_eq!(
            listed,
            [
                ("other/x", "excluded"),
                ("ts/big", "waiting"),
                ("ts/nixpkgs", "excluded")
            ]
        );
        // the same report again changes nothing
        assert!(!seen(&mut c, "navi", &[seen_unit("ts/small", 100, true)], 3).unwrap());

        // the owner includes the big one and ignores the excluded one
        assert_eq!(
            unit_op(
                &c,
                "ts/big",
                &UnitOp {
                    op: "include".into(),
                    value: None
                },
                4
            )
            .unwrap(),
            Ok(true)
        );
        assert_eq!(
            unit_op(
                &c,
                "other/x",
                &UnitOp {
                    op: "include".into(),
                    value: None
                },
                4
            )
            .unwrap(),
            Ok(true)
        );
        assert_eq!(
            unit_op(
                &c,
                "ts/nixpkgs",
                &UnitOp {
                    op: "ignore".into(),
                    value: None
                },
                4
            )
            .unwrap(),
            Ok(true)
        );
        let setup2 = setup(&c).unwrap();
        assert!(setup2.version > s1.version);
        assert_eq!(
            setup2.units.keys().collect::<Vec<_>>(),
            ["other/x", "ts/big", "ts/small"],
            "an override wins over the globs"
        );
        assert!(
            unit_op(
                &c,
                "ts/small",
                &UnitOp {
                    op: "pause".into(),
                    value: None
                },
                5
            )
            .unwrap()
            .is_ok()
        );
        assert!(setup(&c).unwrap().config.units["ts/small"].paused);
        let over = super::super::UnitOverride {
            sync_ignored: vec![Ignored {
                pattern: "node_modules".into(),
                quiescent: false,
            }],
            ..Default::default()
        };
        assert!(
            unit_op(
                &c,
                "ts/small",
                &UnitOp {
                    op: "override".into(),
                    value: Some(over.clone())
                },
                6
            )
            .unwrap()
            .is_ok()
        );
        assert_eq!(setup(&c).unwrap().config.units["ts/small"], over);
        assert!(
            unit_op(
                &c,
                "../x",
                &UnitOp {
                    op: "include".into(),
                    value: None
                },
                7
            )
            .unwrap()
            .is_err()
        );

        // refs after pushes: moved hosts, versions, events
        let mut refs1 = BTreeMap::new();
        refs1.insert("navi".to_owned(), "a".repeat(40));
        let now = now_ms();
        let (moved, v1) = set_refs(&mut c, "ts/small", &refs1, "navi", 1000, now).unwrap();
        assert_eq!((moved, v1), (vec!["navi".to_owned()], 1));
        refs1.insert("flat".to_owned(), "b".repeat(40));
        let (moved, v2) = set_refs(&mut c, "ts/small", &refs1, "flat", 10, now + 1).unwrap();
        assert_eq!((moved, v2), (vec!["flat".to_owned()], 2));
        let (moved, v3) = set_refs(&mut c, "ts/small", &refs1, "flat", 10, now + 2).unwrap();
        assert!(moved.is_empty() && v3 == 3);
        assert_eq!(refs(&c).unwrap()["ts/small"]["flat"], "b".repeat(40));
        assert_eq!(growth(&c, 1).unwrap().len(), 1);
        assert!(hosts(&c).unwrap().contains("flat"));

        // conflicts follow the host's reports and the panel's choices
        let report = ConflictReport {
            path: "a.txt".into(),
            kind: "text".into(),
            ours: "1".repeat(40),
            theirs: "2".repeat(40),
            other_host: "flat".into(),
            at: 11,
        };
        reconcile_conflicts(
            &mut c,
            "navi",
            "ts/small",
            std::slice::from_ref(&report),
            11,
        )
        .unwrap();
        let open = conflicts(&c).unwrap();
        assert_eq!(open.len(), 1);
        let id = open[0].id;
        reconcile_conflicts(
            &mut c,
            "navi",
            "ts/small",
            std::slice::from_ref(&report),
            12,
        )
        .unwrap();
        assert_eq!(conflicts(&c).unwrap().len(), 1, "no duplicate");
        assert!(request_conflict(&c, id, Side::Theirs).unwrap().is_ok());
        assert!(request_conflict(&c, id, Side::Mine).unwrap().is_err());
        conflict_resolved(&c, "navi", id, Some("gone"), 13).unwrap();
        assert_eq!(
            conflict(&c, id).unwrap().unwrap().error.as_deref(),
            Some("gone")
        );
        assert!(request_conflict(&c, id, Side::Theirs).unwrap().is_ok());
        conflict_resolved(&c, "navi", id, None, 14).unwrap();
        assert!(conflict(&c, id).unwrap().is_none());
        reconcile_conflicts(&mut c, "navi", "ts/small", &[report], 15).unwrap();
        let again = conflicts(&c).unwrap();
        assert_eq!(again.len(), 1);
        reconcile_conflicts(&mut c, "navi", "ts/small", &[], 16).unwrap();
        assert!(conflicts(&c).unwrap().is_empty(), "the markers went");
        let dismissed = {
            reconcile_conflicts(
                &mut c,
                "navi",
                "ts/small",
                &[ConflictReport {
                    path: "b".into(),
                    kind: "binary".into(),
                    ours: String::new(),
                    theirs: String::new(),
                    other_host: "flat".into(),
                    at: 17,
                }],
                17,
            )
            .unwrap();
            conflicts(&c).unwrap()[0].id
        };
        assert!(dismiss_conflict(&c, dismissed, 18).unwrap());
        assert!(!dismiss_conflict(&c, dismissed, 19).unwrap());
    }

    #[test]
    fn unsyncable_directories_never_travel_and_leave_the_setup() {
        let mut c = conn();
        save_config(&c, &WorkspaceConfig::default(), 1).unwrap();
        // included first (a plain repository), then found to be a linked worktree
        assert!(seen(&mut c, "navi", &[seen_unit("ts/wt", 100, true)], 2).unwrap());
        assert_eq!(
            setup(&c).unwrap().units.keys().collect::<Vec<_>>(),
            ["ts/wt"]
        );
        let mut wt = seen_unit("ts/wt", 100, false);
        wt.unsyncable = Some("a linked worktree of /r/ts/main".into());
        assert!(
            seen(&mut c, "navi", &[wt.clone()], 3).unwrap(),
            "the setup changed"
        );
        assert!(setup(&c).unwrap().units.is_empty());
        assert!(!seen(&mut c, "navi", &[wt.clone()], 4).unwrap());
        let new = new_dirs(&c, &WorkspaceConfig::default()).unwrap();
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].state, "unsyncable");
        assert_eq!(
            new[0].reason.as_deref(),
            Some("a linked worktree of /r/ts/main")
        );
        // a brand-new one on another host is never included
        let mut other = seen_unit("ts/wt2", 100, false);
        other.unsyncable = Some("a linked worktree of /r/ts/main".into());
        assert!(!seen(&mut c, "flat", &[other], 5).unwrap());
        assert!(units(&c).unwrap().iter().all(|u| u.state == "unsyncable"));
        let cov = coverage(&c, &WorkspaceConfig::default(), &HashMap::new(), 0).unwrap();
        assert!(cov.units.iter().all(|u| u.status == "unsyncable"));
    }

    #[test]
    fn traffic_cost_and_transfers_add_up_into_stats() {
        let c = conn();
        let hour = 1_700_000_000_000 - (1_700_000_000_000 % HOUR_MS);
        let d = TrafficDelta {
            requests: 3,
            bytes_in: 1000,
            bytes_out: 20,
            ms: 90,
            failures: 1,
        };
        add_traffic(&c, "navi", hour, "push", &d).unwrap();
        add_traffic(&c, "navi", hour, "push", &d).unwrap();
        add_traffic(&c, "flat", hour, "fetch", &d).unwrap();
        let cost = Cost {
            cpu_ms: 10,
            git_cpu_ms: 20,
            git_calls: 5,
            passes: 2,
            rss_kb: 30_000,
            watches: 7,
            ..Cost::default()
        };
        add_cost(&c, "navi", hour, &cost).unwrap();
        add_cost(
            &c,
            "navi",
            hour,
            &Cost {
                rss_kb: 25_000,
                watches: 8,
                ..cost.clone()
            },
        )
        .unwrap();
        add_transfer(
            &c,
            &Transfer {
                host: "navi".into(),
                unit: "ts/u".into(),
                kind: "push".into(),
                bytes: 5 << 20,
                ms: 500,
                at: hour + 1,
            },
        )
        .unwrap();
        let now = hour + 2 * HOUR_MS;
        let s = stats(
            &c,
            now - DAY_MS,
            now,
            vec![RepoSize {
                unit: "ts/u".into(),
                bytes: 42,
            }],
            now,
        )
        .unwrap();
        assert_eq!((s.bucket_ms, s.hub_bytes, s.repos.len()), (HOUR_MS, 42, 1));
        let navi: Vec<&TrafficRow> = s.traffic.iter().filter(|t| t.host == "navi").collect();
        assert_eq!(navi.len(), 1);
        assert_eq!(
            (
                navi[0].requests,
                navi[0].bytes_in,
                navi[0].ms,
                navi[0].failures,
                navi[0].at
            ),
            (6, 2000, 180, 2, hour)
        );
        assert_eq!(s.cost.len(), 1);
        let cr = &s.cost[0];
        assert_eq!(
            (
                cr.cpu_ms,
                cr.git_cpu_ms,
                cr.git_calls,
                cr.passes,
                cr.rss_kb,
                cr.watches
            ),
            (20, 40, 10, 4, 30_000, 8),
            "counters add, the resident set keeps its peak, the watches their last value"
        );
        assert_eq!(s.transfers.len(), 1);
        assert_eq!((s.transfers[0].bytes, s.transfers[0].ms), (5 << 20, 500));
        // daily buckets beyond three days land on a local midnight
        let week = stats(&c, now - 7 * DAY_MS, now, Vec::new(), now).unwrap();
        assert_eq!(week.bucket_ms, DAY_MS);
        assert_eq!(week.traffic.len(), 2);
        assert!(
            week.traffic
                .iter()
                .all(|t| t.at <= hour && hour - t.at < DAY_MS)
        );
        // old rows go
        add_traffic(&c, "navi", hour + STATS_KEEP_MS + HOUR_MS, "push", &d).unwrap();
        let later_now = hour + STATS_KEEP_MS + 2 * HOUR_MS;
        let later = stats(&c, later_now - DAY_MS, later_now, Vec::new(), later_now).unwrap();
        assert_eq!(later.traffic.len(), 1);
        let old: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM workspace_traffic WHERE hour = ?1",
                [hour],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);
    }

    #[test]
    fn coverage_and_preview_explain_every_directory() {
        let mut c = conn();
        let cfg = WorkspaceConfig {
            include: vec!["ts/*".into()],
            exclude: vec!["*_bkp".into()],
            auto_include_mb: 1,
            ..WorkspaceConfig::default()
        };
        save_config(&c, &cfg, 1).unwrap();
        let mut small = seen_unit("ts/small", 100, true);
        small.excluded.insert("node_modules".into(), 5000);
        let big = seen_unit("ts/big", 5 << 20, true);
        let bkp = seen_unit("ts/old_bkp", 300, false);
        let other = seen_unit("other/x", 700, false);
        assert!(seen(&mut c, "navi", &[small.clone(), big, bkp, other], 2).unwrap());
        let mut small_flat = small.clone();
        small_flat.size = 120;
        small_flat.excluded.insert("node_modules".into(), 9000);
        small_flat.excluded.insert("target".into(), 100);
        seen(&mut c, "flat", &[small_flat], 3).unwrap();
        let mut reports: HashMap<String, HashMap<String, UnitStatus>> = HashMap::new();
        reports.entry("navi".into()).or_default().insert(
            "ts/small".into(),
            UnitStatus {
                unit: "ts/small".into(),
                kind: UnitKind::Git,
                state: super::super::UnitState::InStep,
                detail: None,
                snapshot: None,
                files: 3,
                last_pass: None,
                failing_since: None,
                conflicts: Vec::new(),
                held: None,
                diverged: Vec::new(),
                ignored: vec![super::super::IgnoredDir {
                    path: "dist".into(),
                    files: 2,
                    bytes: 400,
                    carried: false,
                    quiescent: false,
                }],
            },
        );
        let cov = coverage(&c, &cfg, &reports, 77).unwrap();
        assert_eq!(cov.hub_bytes, 77);
        let by: BTreeMap<&str, &CoverageUnit> =
            cov.units.iter().map(|u| (u.unit.as_str(), u)).collect();
        assert_eq!(by["ts/small"].status, "travels");
        assert_eq!(
            (by["ts/small"].size, by["ts/small"].excluded["node_modules"]),
            (120, 9000)
        );
        assert_eq!(by["ts/small"].hosts, ["flat", "navi"]);
        assert_eq!(by["ts/big"].status, "waiting");
        assert_eq!(by["ts/old_bkp"].status, "left_out");
        assert_eq!(
            by["ts/old_bkp"].decision,
            Decision::Excluded {
                rule: "*_bkp".into()
            }
        );
        assert_eq!(by["other/x"].decision, Decision::Unmatched);
        let navi = cov.hosts.iter().find(|h| h.host == "navi").unwrap();
        assert_eq!(
            (navi.units, navi.bytes, navi.travels),
            (4, 100 + 5000 + (5 << 20) + 300 + 700, 100)
        );
        assert_eq!(cov.names.len(), 2);
        assert_eq!(
            (
                cov.names[0].name.as_str(),
                cov.names[0].bytes,
                cov.names[0].units
            ),
            ("node_modules", 9000, 1)
        );
        assert_eq!(cov.home.len(), 1);
        assert_eq!(
            (cov.home[0].path.as_str(), cov.home[0].bytes),
            ("dist", 400)
        );

        // a draft that drops the exclude and includes `other`
        let draft = WorkspaceConfig {
            include: vec!["ts/*".into(), "other/*".into()],
            exclude: Vec::new(),
            ..cfg.clone()
        };
        let p = preview(&c, &draft).unwrap();
        let by: BTreeMap<&str, &CoverageUnit> =
            p.units.iter().map(|u| (u.unit.as_str(), u)).collect();
        assert_eq!(
            by["ts/old_bkp"].decision,
            Decision::Selected {
                rule: Some("ts/*".into())
            }
        );
        assert_eq!(
            by["other/x"].decision,
            Decision::Selected {
                rule: Some("other/*".into())
            }
        );
        let counts: Vec<(&str, bool, u64)> = p
            .globs
            .iter()
            .map(|g| (g.pattern.as_str(), g.exclude, g.matches))
            .collect();
        assert_eq!(counts, [("ts/*", false, 3), ("other/*", false, 1)]);
        let p2 = preview(&c, &cfg).unwrap();
        assert!(
            p2.globs
                .iter()
                .any(|g| g.pattern == "*_bkp" && g.exclude && g.matches == 1)
        );
    }

    #[test]
    fn repositories_are_created_bare_with_the_guard() {
        let dir = std::env::temp_dir().join(format!("ccdash-wsrepos-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let repos = Repos::open(&dir.join("workspace")).unwrap();
        assert!(dir.join("workspace/.hooks/pre-receive").is_file());
        assert!(!repos.exists("ts/u"));
        let path = repos.ensure("ts/u").unwrap();
        assert!(repos.exists("ts/u") && path.ends_with("ts/u.git"));
        assert_eq!(
            git_ok(&["config", "receive.shallowUpdate"], Some(&path))
                .unwrap()
                .trim(),
            "true"
        );
        assert_eq!(
            git_ok(&["config", "core.hooksPath"], Some(&path))
                .unwrap()
                .trim(),
            dir.join("workspace").join(".hooks").to_string_lossy()
        );
        assert!(repos.sync_refs("ts/u").unwrap().is_empty());
        assert!(repos.history("ts/u").unwrap().is_empty());
        assert!(repos.blob("ts/u", "zz").is_err());
        gc(&dir.join("workspace")).unwrap();
        fs::remove_dir_all(&dir).unwrap();
    }
}
