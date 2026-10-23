//! The collector's side: a thread that keeps this machine's Claude config
//! dir in step with the hub (blocking HTTP and files, its own index).
//!
//! inotify (`super::watch`) says which files changed, the hub says what it
//! got (`SyncChanged`); a pass settles just those files: changed here → up
//! (the tail when it only grew), changed on the hub → down, changed on
//! both → the hub's copy wins and the local one is set aside under
//! `ccdash-conflicts/`. Everything is scanned at start-up, after an
//! inotify overflow, for a resync or a flush, and every half hour as a
//! safety net; without inotify, every `interval`. Files of a session
//! running here are never written; deletions never travel. The prompt
//! history is merged by line. See `meta/plans/ccdash-hub/h7.md`.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tracing::{debug, info, warn};
use ureq::unversioned::{
    resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver},
    transport::{DefaultConnector, NextTimeout},
};

use super::{
    BODY_LIMIT, CHUNK, CONFLICTS_DIR, Changes, ConflictReport, HISTORY, HistoryPage, Probe, Probed,
    Refused, Row, SyncStatus, fp, fp_of, glob, line_digest, session_of, valid_path, watch,
};
use crate::{
    protocol::ToHub,
    util::{now_ms, pid_alive},
};

/// What travels unless `[sync] include` says otherwise (handoff's list).
pub const DEFAULT_INCLUDE: &[&str] = &[
    "projects/*",
    "file-history/*",
    "plans/*",
    "tasks/*",
    "paste-cache/*",
    HISTORY,
];
/// Scratch sessions under `/tmp` stay home.
pub const DEFAULT_EXCLUDE: &[&str] = &["projects/-tmp-*"];
/// Suffix of a download being written, renamed over the file when whole.
const PART: &str = ".ccdash-part";
/// The hub's changes are read at least this often without a signal
/// (`SyncChanged` covers the rest; a reconnect asks again).
const PULL_EVERY: Duration = Duration::from_secs(300);
/// A burst of file events settles for this long before a pass…
const QUIET: Duration = Duration::from_millis(250);
/// …but waits no longer than this while writes keep coming.
const BURST_MAX: Duration = Duration::from_secs(2);
/// Files that could not settle (held, refused, failed) are tried again
/// this often.
const PENDING_EVERY: Duration = Duration::from_secs(10);
/// With inotify, everything is still scanned this often, in case an
/// event went missing.
const SAFETY_EVERY: Duration = Duration::from_secs(1800);
/// Status goes up at most this often during a long pass.
const REPORT_EVERY: Duration = Duration::from_secs(2);
/// An unchanged status goes up again this often (a restarted hub forgot it).
const RESEND_EVERY: Duration = Duration::from_secs(60);
const PROBE_BATCH: usize = 500;
const HISTORY_BATCH_BYTES: usize = 1 << 20;
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What the collector (and the watcher) ask of the sync thread.
pub enum Cmd {
    /// The hub has new writes.
    Changed,
    /// Forget every sync record and compare everything again.
    Resync,
    /// Scan everything and settle it now; the sender hears when it is done.
    Flush(oneshot::Sender<()>),
    /// Paths under the root that changed (from the watcher).
    Touched(Vec<String>),
    /// The watcher lost events (queue overflow): scan everything.
    Rescan,
    /// The watcher stopped: scan every `interval` from now on.
    Unwatched,
}

/// The collector's handle on the sync thread.
#[derive(Clone)]
pub struct Handle {
    tx: mpsc::Sender<Cmd>,
    /// Sessions whose transcript here is behind the hub's.
    behind: Arc<Mutex<HashSet<String>>>,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    /// Runs a pass and waits for it, at most `limit`; false on timeout.
    pub async fn flush(&self, limit: Duration) -> bool {
        let (tx, rx) = oneshot::channel();
        self.send(Cmd::Flush(tx));
        matches!(tokio::time::timeout(limit, rx).await, Ok(Ok(())))
    }

    /// Whether another machine wrote to `session` since this one last
    /// had its transcript in step.
    pub fn behind(&self, session: &str) -> bool {
        self.behind
            .lock()
            .map(|b| b.contains(session))
            .unwrap_or(false)
    }
}

/// How the thread runs.
pub struct Settings {
    pub host: String,
    /// The Claude config dir.
    pub root: PathBuf,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub interval: Duration,
    /// The hub's base URL (`https://…`).
    pub base_url: String,
    pub token_file: PathBuf,
    /// The thread's own index.
    pub index: PathBuf,
}

/// Starts the thread; its status goes to the hub through `hub`.
///
/// # Errors
///
/// The index cannot be opened.
pub fn spawn(settings: Settings, hub: UnboundedSender<ToHub>) -> Result<Handle> {
    let index = Index::open(&settings.index)?;
    let (tx, rx) = mpsc::channel();
    let behind = Arc::new(Mutex::new(HashSet::new()));
    let filter = Filter {
        include: settings.include.clone(),
        exclude: settings.exclude.clone(),
    };
    let watching = match watch::spawn(settings.root.clone(), filter, tx.clone()) {
        Ok(dirs) => {
            info!("sync: watching {dirs} directories for changes");
            true
        }
        Err(e) => {
            warn!(
                "sync: cannot watch {} ({e:#}); scanning it every {:?}",
                settings.root.display(),
                settings.interval
            );
            false
        }
    };
    let mut syncer = Syncer::new(settings, index, hub, behind.clone())?;
    syncer.status.watching = watching;
    let to_self = tx.clone();
    std::thread::Builder::new()
        .name("sync".into())
        .spawn(move || run(syncer, &rx, &to_self))?;
    Ok(Handle { tx, behind })
}

/// What the next pass has to cover.
#[derive(Default)]
struct Due {
    /// Scan everything.
    full: bool,
    /// Read the hub's changes.
    pull: bool,
    resync: bool,
    /// Paths the watcher reported.
    dirty: BTreeSet<String>,
    flushes: Vec<oneshot::Sender<()>>,
}

impl Due {
    fn take(&mut self, cmd: Cmd, watching: &mut bool) {
        match cmd {
            Cmd::Changed => self.pull = true,
            Cmd::Resync => self.resync = true,
            Cmd::Flush(done) => {
                self.full = true;
                self.pull = true;
                self.flushes.push(done);
            }
            Cmd::Touched(paths) => self.dirty.extend(paths),
            Cmd::Rescan => self.full = true,
            Cmd::Unwatched => {
                warn!("sync: the file watcher stopped; scanning instead");
                *watching = false;
                self.full = true;
            }
        }
    }

    fn any(&self) -> bool {
        self.full || self.pull || self.resync || !self.dirty.is_empty() || !self.flushes.is_empty()
    }
}

/// The thread: waits for changes, timers and the collector, runs passes.
/// `to_self` lets a watcher started later (the root did not exist yet)
/// report here.
fn run(mut s: Syncer, rx: &mpsc::Receiver<Cmd>, to_self: &mpsc::Sender<Cmd>) {
    let mut due = Due {
        full: true,
        pull: true,
        ..Due::default()
    };
    let mut watching = s.status.watching;
    let (mut last_full, mut last_pull, mut last_pending) =
        (Instant::now(), Instant::now(), Instant::now());
    // after a failed pass: when to try again, and the backoff that led there
    let mut retry: Option<(Instant, Duration)> = None;
    loop {
        let rescan_every = if watching { SAFETY_EVERY } else { s.interval };
        let pending_due = !s.pending.is_empty() && last_pending.elapsed() >= PENDING_EVERY;
        if retry.is_some_and(|(at, _)| Instant::now() >= at) && !due.any() && !pending_due {
            retry = None;
        }
        let wait = if !due.flushes.is_empty() {
            Duration::ZERO
        } else if let Some((at, _)) = retry {
            at.saturating_duration_since(Instant::now())
        } else if due.any() || pending_due {
            Duration::ZERO
        } else {
            let mut wait = PULL_EVERY
                .saturating_sub(last_pull.elapsed())
                .min(rescan_every.saturating_sub(last_full.elapsed()));
            if !s.pending.is_empty() {
                wait = wait.min(PENDING_EVERY.saturating_sub(last_pending.elapsed()));
            }
            wait
        };
        if !wait.is_zero() {
            match rx.recv_timeout(wait) {
                Ok(cmd) => {
                    let burst = matches!(cmd, Cmd::Touched(_));
                    due.take(cmd, &mut watching);
                    // NOTE: Claude Code writes a transcript line by line; one pass per burst
                    if burst && !settle(rx, &mut due, &mut watching) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        while let Ok(cmd) = rx.try_recv() {
            due.take(cmd, &mut watching);
        }
        if retry.is_some_and(|(at, _)| Instant::now() < at) && due.flushes.is_empty() {
            continue;
        }
        if last_pull.elapsed() >= PULL_EVERY {
            due.pull = true;
        }
        if last_full.elapsed() >= rescan_every {
            due.full = true;
        }
        let pending_due = !s.pending.is_empty() && last_pending.elapsed() >= PENDING_EVERY;
        if !due.any() && !pending_due {
            continue;
        }
        let work = std::mem::take(&mut due);
        if work.resync
            && let Err(e) = s.reset()
        {
            warn!("sync: resetting the index: {e:#}");
        }
        let (full, pull) = (work.full || work.resync, work.pull || work.resync);
        s.status.watching = watching;
        match s.pass(pull, full, &work.dirty) {
            Ok(()) => {
                s.status.error = None;
                retry = None;
                let now = Instant::now();
                last_pending = now;
                if full {
                    last_full = now;
                }
                if full && !watching {
                    watching = s.watch(to_self);
                    s.status.watching = watching;
                }
                if pull {
                    last_pull = now;
                }
            }
            Err(e) => {
                let message = format!("{e:#}");
                if s.status.error.as_ref() != Some(&message) {
                    warn!("sync: {message}");
                }
                s.status.error = Some(message);
                // NOTE: what this pass did not get to waits for the next one
                due.full |= full;
                due.pull |= pull;
                due.dirty.extend(work.dirty);
                let backoff =
                    retry.map_or(Duration::from_secs(1), |(_, b)| (b * 2).min(MAX_BACKOFF));
                retry = Some((Instant::now() + backoff, backoff));
            }
        }
        s.report(true);
        for done in work.flushes {
            let _ = done.send(());
        }
    }
}

/// Takes what arrives until `QUIET` passes without anything or
/// `BURST_MAX` is up; false when the collector went away.
fn settle(rx: &mpsc::Receiver<Cmd>, due: &mut Due, watching: &mut bool) -> bool {
    let end = Instant::now() + BURST_MAX;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(QUIET.min(left)) {
            Ok(cmd) => due.take(cmd, watching),
            Err(RecvTimeoutError::Timeout) => return true,
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// A file as the scan saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stat {
    size: u64,
    /// Nanoseconds.
    mtime: i64,
}

impl Stat {
    fn of(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            mtime: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
        }
    }
}

/// A regular file's `Stat`; none for anything else or nothing.
fn stat(path: &Path) -> Option<Stat> {
    let meta = path.symlink_metadata().ok()?;
    meta.is_file().then(|| Stat::of(&meta))
}

/// What travels: `[sync] include` minus `exclude`, relative to the root.
#[derive(Debug, Clone)]
pub(super) struct Filter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Filter {
    /// A directory some included path may lie under.
    pub fn could_hold(&self, dir: &str) -> bool {
        let top = dir.split('/').next().unwrap_or(dir);
        top != CONFLICTS_DIR
            && self.include.iter().any(|p| {
                let first = p.split('/').next().unwrap_or(p);
                glob(first, top)
            })
            && !self.exclude.iter().any(|p| glob(p, dir))
    }

    /// A file that is copied as it is (the prompt history is merged instead).
    pub fn travels(&self, path: &str) -> bool {
        path != HISTORY
            && !path.ends_with(PART)
            && self.include.iter().any(|p| glob(p, path))
            && !self.exclude.iter().any(|p| glob(p, path))
    }

    /// Whether the prompt history is merged.
    pub fn history(&self) -> bool {
        self.include.iter().any(|p| glob(p, HISTORY))
    }
}

/// What this machine last had in step with the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Synced {
    size: u64,
    mtime: i64,
    version: i64,
    /// `fp` at `size`.
    fp: String,
    /// Deleted here after it was synced: never fetched again (until a resync).
    deleted: bool,
}

/// How a file without a sync record compares with the hub's copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Same,
    /// The hub's copy is longer and starts with ours.
    Behind,
    /// Ours is longer and starts with the hub's.
    Ahead,
    Diverged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Nothing,
    Upload {
        from: u64,
        expect: i64,
    },
    Download {
        from: u64,
    },
    /// The same bytes on both sides: record it.
    Adopt {
        version: i64,
    },
    Conflict,
    /// Its session runs here: wait.
    Hold,
    /// Deleted here: remember, never fetch it again.
    Forget,
    /// Needs `/sync/compare` first.
    Probe,
}

/// What to do with one file.
fn decide(
    local: Option<Stat>,
    synced: Option<&Synced>,
    remote: Option<&Row>,
    verdict: Option<Verdict>,
    live: bool,
) -> Action {
    let wait_or = |action: Action| if live { Action::Hold } else { action };
    match (local, synced, remote) {
        (None, None, None) => Action::Nothing,
        (Some(_), None, None) => Action::Upload { from: 0, expect: 0 },
        (Some(l), None, Some(r)) => match verdict {
            None => Action::Probe,
            Some(Verdict::Same) => Action::Adopt { version: r.version },
            Some(Verdict::Behind) => wait_or(Action::Download { from: l.size }),
            Some(Verdict::Ahead) => Action::Upload {
                from: r.size,
                expect: r.version,
            },
            Some(Verdict::Diverged) => wait_or(Action::Conflict),
        },
        (None, None, Some(_)) => wait_or(Action::Download { from: 0 }),
        (None, Some(s), _) => {
            if s.deleted {
                Action::Nothing
            } else {
                Action::Forget
            }
        }
        (Some(l), Some(s), remote) => {
            let here = l.size != s.size || l.mtime != s.mtime || s.deleted;
            let there = remote.map_or(0, |r| r.version) != s.version;
            match (here, there, remote) {
                (false, false, _) => Action::Nothing,
                // NOTE: the hub lost it (a new hub database): ours is the copy
                (_, true, None) => Action::Upload { from: 0, expect: 0 },
                (true, false, _) => Action::Upload {
                    from: if l.size >= s.size && !s.deleted {
                        s.size
                    } else {
                        0
                    },
                    expect: s.version,
                },
                (false, true, Some(r)) => wait_or(Action::Download {
                    from: if r.base <= s.version && r.size >= s.size {
                        s.size
                    } else {
                        0
                    },
                }),
                (true, true, Some(_)) => wait_or(Action::Conflict),
            }
        }
    }
}

/// The thread's state.
struct Syncer {
    host: String,
    root: PathBuf,
    filter: Filter,
    /// Between full scans without inotify.
    interval: Duration,
    client: Client,
    index: Index,
    files: HashMap<String, Synced>,
    remote: HashMap<String, Row>,
    cursor: i64,
    history_offset: u64,
    history_fp: Option<String>,
    history_cursor: i64,
    hub: UnboundedSender<ToHub>,
    behind: Arc<Mutex<HashSet<String>>>,
    status: SyncStatus,
    reported: Option<(SyncStatus, Instant)>,
    /// What travels here, as the last scan and the watcher's reports since
    /// left it.
    local: HashMap<String, Stat>,
    /// Files that did not settle in their pass (held, refused, failed,
    /// still to compare); every pass tries them again.
    pending: BTreeSet<String>,
    /// Uploads the hub refused in this pass because another machine
    /// holds their session (see `sync::hub`).
    refused_held: HashSet<String>,
}

impl Syncer {
    fn new(
        settings: Settings,
        index: Index,
        hub: UnboundedSender<ToHub>,
        behind: Arc<Mutex<HashSet<String>>>,
    ) -> Result<Self> {
        let (files, remote) = index.load()?;
        let cursor = index.meta("cursor")?.unwrap_or(0);
        let history_offset = index.meta("history_offset")?.unwrap_or(0);
        let history_cursor = index.meta("history_cursor")?.unwrap_or(0);
        let history_fp = index.text("history_fp")?;
        Ok(Self {
            host: settings.host,
            root: settings.root,
            filter: Filter {
                include: settings.include,
                exclude: settings.exclude,
            },
            interval: settings.interval.max(Duration::from_secs(1)),
            client: Client::new(settings.base_url, settings.token_file),
            index,
            files,
            remote,
            cursor,
            history_offset: history_offset.try_into().unwrap_or(0),
            history_fp,
            history_cursor,
            hub,
            behind,
            status: SyncStatus::default(),
            reported: None,
            local: HashMap::new(),
            pending: BTreeSet::new(),
            refused_held: HashSet::new(),
        })
    }

    /// Forgets every sync record: the next pass compares everything and
    /// fetches what this machine lacks, deleted files included.
    fn reset(&mut self) -> Result<()> {
        info!("sync: resync requested, comparing everything again");
        self.index.reset()?;
        self.files.clear();
        self.remote.clear();
        self.cursor = 0;
        self.history_offset = 0;
        self.history_fp = None;
        self.history_cursor = 0;
        self.pending.clear();
        Ok(())
    }

    /// Sends the status to the hub when it changed (the pass time aside),
    /// at most every `REPORT_EVERY` unless `now`; unchanged, once a minute.
    fn report(&mut self, now: bool) {
        let due = self.reported.as_ref().is_none_or(|(last, at)| {
            let changed = SyncStatus {
                last_pass: self.status.last_pass,
                ..last.clone()
            } != self.status;
            (changed && (now || at.elapsed() >= REPORT_EVERY)) || at.elapsed() >= RESEND_EVERY
        });
        if due {
            let _ = self.hub.send(ToHub::Sync {
                status: self.status.clone(),
            });
            self.reported = Some((self.status.clone(), Instant::now()));
        }
    }

    /// Settles `dirty` (or, `full`, everything there is) with what the hub
    /// has; `pull` reads the hub's changes first.
    fn pass(&mut self, pull: bool, full: bool, dirty: &BTreeSet<String>) -> Result<()> {
        let started = Instant::now();
        let mut candidates = self.pending.clone();
        if pull {
            candidates.extend(self.pull_changes()?);
        }
        self.refused_held.clear();
        let mut live = Live::read(&self.root);
        let mut history = full;
        if full {
            self.local = self.scan();
            candidates.extend(self.local.keys().cloned());
            candidates.extend(self.remote.keys().cloned());
        } else {
            for path in dirty {
                if path == HISTORY {
                    history = true;
                } else if self.filter.travels(path) {
                    self.stat_again(path);
                    candidates.insert(path.clone());
                }
            }
        }
        let probes: Vec<String> = candidates
            .iter()
            .filter(|p| {
                self.local.contains_key(*p)
                    && !self.files.contains_key(*p)
                    && self.remote.contains_key(*p)
            })
            .cloned()
            .collect();
        let verdicts = self.compare(&probes)?;
        let plan: Vec<(String, Action)> = candidates
            .into_iter()
            .map(|path| {
                let action = self.decide(&path, verdicts.get(&path).copied(), &live);
                (path, action)
            })
            .collect();
        let work = plan
            .iter()
            .filter(|(_, a)| !matches!(a, Action::Nothing | Action::Hold | Action::Probe))
            .count() as u64;
        let (mut up, mut down, mut held, mut done) = (0, 0, 0, 0);
        for (path, action) in &plan {
            let writes_here = matches!(action, Action::Download { .. } | Action::Conflict);
            // NOTE: a long pass outlives its plan; a session may have started since
            let action = if writes_here && live.refresh(&self.root).runs(path) {
                &Action::Hold
            } else {
                action
            };
            let l = self.local.get(path).copied();
            if let Err(e) = self.act(path, l, action) {
                self.file_error(path, &e)?;
            }
            if writes_here {
                self.stat_again(path);
            }
            let after = self.decide(path, verdicts.get(path).copied(), &live);
            match after {
                Action::Nothing => {
                    self.pending.remove(path);
                }
                _ => {
                    self.pending.insert(path.clone());
                }
            }
            match after {
                Action::Hold => held += 1,
                Action::Upload { .. } if self.refused_held.contains(path) => held += 1,
                Action::Upload { .. } => up += 1,
                Action::Download { .. } | Action::Conflict => down += 1,
                _ => {}
            }
            if !matches!(action, Action::Nothing | Action::Hold | Action::Probe) {
                done += 1;
                if work > 50 {
                    self.status.backfill = Some((done, work));
                }
                self.report(false);
            }
        }
        if history {
            self.history_up()?;
        }
        if pull {
            self.history_down()?;
        }
        self.status.files = self.local.len() as u64;
        self.status.bytes = self.local.values().map(|s| s.size).sum();
        self.status.pending_up = up;
        self.status.pending_down = down;
        self.status.held = held;
        self.status.backfill = None;
        self.status.last_pass = Some(now_ms());
        self.update_behind();
        debug!(
            "sync pass ({}): {} files settled in {:?}",
            if full { "full" } else { "changes" },
            plan.len(),
            started.elapsed()
        );
        Ok(())
    }

    /// Tries to start the watcher again (after scans without one).
    fn watch(&self, tx: &mpsc::Sender<Cmd>) -> bool {
        let filter = self.filter.clone();
        match watch::spawn(self.root.clone(), filter, tx.clone()) {
            Ok(dirs) => {
                info!("sync: watching {dirs} directories for changes now");
                true
            }
            Err(e) => {
                debug!("sync: still cannot watch {}: {e}", self.root.display());
                false
            }
        }
    }

    fn decide(&self, path: &str, verdict: Option<Verdict>, live: &Live) -> Action {
        decide(
            self.local.get(path).copied(),
            self.files.get(path),
            self.remote.get(path),
            verdict,
            live.runs(path),
        )
    }

    /// Reads one file's size and time again (after a report or a write).
    fn stat_again(&mut self, path: &str) {
        match stat(&self.root.join(path)) {
            Some(st) => self.local.insert(path.to_owned(), st),
            None => self.local.remove(path),
        };
    }

    /// A transport failure stops the pass; a file's own failure is logged
    /// and the pass goes on.
    fn file_error(&self, path: &str, e: &anyhow::Error) -> Result<()> {
        if e.downcast_ref::<HubError>().is_some() {
            return Err(anyhow::anyhow!("{e:#}"));
        }
        debug!("sync {path}: {e:#}");
        Ok(())
    }

    fn act(&mut self, path: &str, local: Option<Stat>, action: &Action) -> Result<()> {
        match action {
            Action::Nothing | Action::Hold | Action::Probe => Ok(()),
            Action::Forget => {
                if let Some(s) = self.files.get_mut(path) {
                    s.deleted = true;
                    self.index.put_file(path, s)?;
                }
                Ok(())
            }
            Action::Adopt { version } => {
                let l = local.context("adopting a missing file")?;
                let synced = Synced {
                    size: l.size,
                    mtime: l.mtime,
                    version: *version,
                    fp: fp_of(&self.root.join(path), l.size)?,
                    deleted: false,
                };
                self.record(path, synced)
            }
            Action::Upload { from, expect } => {
                let l = local.context("uploading a missing file")?;
                self.upload(path, l, *from, *expect)
            }
            Action::Download { from } => self.download(path, *from),
            Action::Conflict => {
                let l = local.context("a conflict on a missing file")?;
                self.conflict(path, l)
            }
        }
    }

    fn record(&mut self, path: &str, synced: Synced) -> Result<()> {
        self.index.put_file(path, &synced)?;
        self.files.insert(path.to_owned(), synced);
        Ok(())
    }

    fn remember_remote(&mut self, row: Row) -> Result<()> {
        self.index.put_remote(&row)?;
        self.remote.insert(row.path.clone(), row);
        Ok(())
    }

    /// Reads the hub's changes since the cursor; the paths they touch.
    fn pull_changes(&mut self) -> Result<Vec<String>> {
        let mut changed = Vec::new();
        loop {
            let page: Changes = self
                .client
                .get_json("/sync/changes", &[("since", self.cursor.to_string())])?;
            for row in page.rows {
                self.cursor = self.cursor.max(row.version);
                // NOTE: only what travels here is ever written under the root
                if valid_path(&row.path) && self.filter.travels(&row.path) {
                    changed.push(row.path.clone());
                    self.remember_remote(row)?;
                }
            }
            self.index.set_meta("cursor", self.cursor)?;
            if !page.more {
                return Ok(changed);
            }
        }
    }

    /// Everything that travels, by path relative to the root.
    fn scan(&self) -> HashMap<String, Stat> {
        let mut out = HashMap::new();
        let mut stack = vec![(self.root.clone(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(name) = entry.file_name().into_string() else {
                    continue;
                };
                let path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let Ok(meta) = entry.path().symlink_metadata() else {
                    continue;
                };
                if meta.is_dir() {
                    if self.filter.could_hold(&path) {
                        stack.push((entry.path(), path));
                    }
                } else if meta.is_file() && self.filter.travels(&path) {
                    out.insert(path, Stat::of(&meta));
                }
            }
        }
        out
    }

    /// Settles files both sides hold without a sync record.
    fn compare(&mut self, paths: &[String]) -> Result<HashMap<String, Verdict>> {
        let mut verdicts = HashMap::new();
        if paths.is_empty() {
            return Ok(verdicts);
        }
        info!("sync: comparing {} files with the hub", paths.len());
        let total = paths.len() as u64;
        for (n, batch) in paths.chunks(PROBE_BATCH).enumerate() {
            let probes: Vec<Probe> = batch
                .iter()
                .filter_map(|path| {
                    let size = self.local.get(path)?.size;
                    let fp = fp_of(&self.root.join(path), size).ok()?;
                    Some(Probe {
                        path: path.clone(),
                        size,
                        fp,
                    })
                })
                .collect();
            let answers: Vec<Probed> = self.client.post_json("/sync/compare", &probes)?;
            for (probe, answer) in probes.iter().zip(answers) {
                let (Some(row), Some(hub_fp)) = (answer.row, answer.fp) else {
                    continue;
                };
                let verdict = match probe.size.cmp(&row.size) {
                    std::cmp::Ordering::Equal if hub_fp == probe.fp => Verdict::Same,
                    std::cmp::Ordering::Less if hub_fp == probe.fp => Verdict::Behind,
                    std::cmp::Ordering::Greater
                        if fp_of(&self.root.join(&probe.path), row.size).ok().as_ref()
                            == Some(&hub_fp) =>
                    {
                        Verdict::Ahead
                    }
                    _ => Verdict::Diverged,
                };
                verdicts.insert(probe.path.clone(), verdict);
                self.remember_remote(row)?;
            }
            let done = ((n + 1) * PROBE_BATCH).min(paths.len()) as u64;
            self.status.backfill = Some((done, total));
            self.report(false);
        }
        Ok(verdicts)
    }

    fn upload(&mut self, path: &str, l: Stat, from: u64, expect: i64) -> Result<()> {
        let mut f = File::open(self.root.join(path))?;
        let synced = self.files.get(path).cloned();
        let appends = from > 0
            && synced.as_ref().is_some_and(|s| s.size == from)
            && fp(&mut f, from).ok() == synced.as_ref().map(|s| s.fp.clone());
        let ahead = from > 0 && synced.is_none();
        let from = if appends || ahead { from } else { 0 };
        let end = if path.ends_with(".jsonl") {
            line_end(&mut f, from, l.size)?
        } else {
            l.size
        };
        if end == from && from > 0 {
            if let Some(mut s) = synced.filter(|s| s.size == end) {
                // NOTE: touched, not changed
                s.mtime = l.mtime;
                s.deleted = false;
                self.record(path, s)?;
            }
            return Ok(());
        }
        let (mut offset, mut expect) = (from, expect);
        let mut buf = vec![0; CHUNK.min((end - from) as usize)];
        let row = loop {
            let len = (end - offset).min(CHUNK as u64) as usize;
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut buf[..len])?;
            match self.client.put(path, offset, expect, &buf[..len])? {
                Ok(row) => {
                    expect = row.version;
                    offset += len as u64;
                    if offset >= end {
                        break row;
                    }
                }
                Err(refused) => {
                    match &refused.held_by {
                        Some(holder) => {
                            debug!("sync {path}: its session is open on {holder}, not sending");
                            self.refused_held.insert(path.to_owned());
                        }
                        None => debug!("sync {path}: the hub moved on, settling next pass"),
                    }
                    if let Some(row) = refused.row {
                        self.remember_remote(row)?;
                    }
                    return Ok(());
                }
            }
        };
        let synced = Synced {
            size: end,
            mtime: if end == l.size { l.mtime } else { -1 },
            version: row.version,
            fp: fp(&mut f, end)?,
            deleted: false,
        };
        self.record(path, synced)?;
        self.remember_remote(row)
    }

    fn download(&mut self, path: &str, from: u64) -> Result<()> {
        let target = self.root.join(path);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        let (row, size) = if from > 0 {
            let version = self.files.get(path).map_or(0, |s| s.version);
            let mut f = OpenOptions::new().append(true).open(&target)?;
            if f.metadata()?.len() != from {
                return Ok(());
            }
            let mut offset = from;
            loop {
                let Some((row, data)) = self.client.get(path, offset)? else {
                    return Ok(());
                };
                if row.base > version && self.files.contains_key(path) {
                    // rewritten on the hub meanwhile: fetched whole next pass
                    return self.remember_remote(row);
                }
                f.write_all(&data)?;
                offset += data.len() as u64;
                if offset >= row.size || data.is_empty() {
                    break (row, offset);
                }
            }
        } else {
            let part = PathBuf::from(format!("{}{PART}", target.display()));
            let mut f = File::create(&part)?;
            let (mut offset, mut base) = (0, None);
            let done = loop {
                let Some((row, data)) = self.client.get(path, offset)? else {
                    break None;
                };
                if base.is_some_and(|b| b != row.base) {
                    break None;
                }
                base = Some(row.base);
                f.write_all(&data)?;
                offset += data.len() as u64;
                if offset >= row.size || data.is_empty() {
                    break Some((row, offset));
                }
            };
            let Some(done) = done else {
                let _ = fs::remove_file(&part);
                return Ok(());
            };
            f.sync_all()?;
            fs::rename(&part, &target)?;
            done
        };
        let meta = fs::metadata(&target)?;
        let synced = Synced {
            size,
            mtime: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            version: row.version,
            fp: fp_of(&target, size)?,
            deleted: false,
        };
        self.record(path, synced)?;
        self.remember_remote(row)
    }

    /// Sets the local copy aside, tells the hub, takes the hub's copy.
    fn conflict(&mut self, path: &str, l: Stat) -> Result<()> {
        let copy = format!("{CONFLICTS_DIR}/{}/{path}", now_ms());
        let target = self.root.join(&copy);
        if let Some(dir) = target.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::rename(self.root.join(path), &target)?;
        warn!("sync: {path} changed here and on the hub; kept this copy as {copy}");
        self.index.del_file(path)?;
        self.files.remove(path);
        self.status.conflicts += 1;
        let report = ConflictReport {
            path: path.to_owned(),
            size: l.size,
            copy,
        };
        let _: serde_json::Value = self.client.post_json("/sync/conflicts", &report)?;
        self.download(path, 0)
    }

    /// New complete lines of the local prompt history go up.
    fn history_up(&mut self) -> Result<()> {
        if !self.filter.history() {
            return Ok(());
        }
        let path = self.root.join(HISTORY);
        let Ok(mut f) = File::open(&path) else {
            return Ok(());
        };
        let len = f.metadata()?.len();
        let moved = self.history_offset > len
            || (self.history_offset > 0 && fp(&mut f, self.history_offset).ok() != self.history_fp);
        if moved {
            debug!("sync: the prompt history changed below what was read; reading it again");
            self.history_offset = 0;
        }
        if len > self.history_offset {
            f.seek(SeekFrom::Start(self.history_offset))?;
            let mut buf = Vec::new();
            (&mut f)
                .take(len - self.history_offset)
                .read_to_end(&mut buf)?;
            let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let lines: Vec<&str> = std::str::from_utf8(&buf[..end])
                .unwrap_or_default()
                .split('\n')
                .filter(|l| !l.trim().is_empty())
                .collect();
            self.index.add_lines(&lines, true)?;
            self.history_offset += end as u64;
            self.history_fp = Some(fp(&mut f, self.history_offset)?);
            self.index
                .set_meta("history_offset", self.history_offset.try_into()?)?;
            self.index
                .set_text("history_fp", self.history_fp.as_deref().unwrap_or(""))?;
        }
        loop {
            let pending = self.index.pending_lines(HISTORY_BATCH_BYTES)?;
            if pending.is_empty() {
                return Ok(());
            }
            let lines: Vec<&String> = pending.iter().map(|(_, l)| l).collect();
            let _: serde_json::Value = self
                .client
                .post_json("/sync/history", &serde_json::json!({ "lines": lines }))?;
            self.index.sent_lines(
                &pending
                    .iter()
                    .map(|(d, _)| d.as_slice())
                    .collect::<Vec<_>>(),
            )?;
        }
    }

    /// Lines other machines added that this one lacks, appended in one
    /// write: Claude Code appends to the same file, so it is never rewritten.
    fn history_down(&mut self) -> Result<()> {
        if !self.filter.history() {
            return Ok(());
        }
        let path = self.root.join(HISTORY);
        loop {
            let page: HistoryPage = self.client.get_json(
                "/sync/history",
                &[("since", self.history_cursor.to_string())],
            )?;
            let fresh: Vec<&str> = page
                .lines
                .iter()
                .map(|(_, l)| l.as_str())
                .filter(|l| !self.index.knows_line(l).unwrap_or(true))
                .collect();
            if !fresh.is_empty() {
                let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
                let ends_open = {
                    let mut r = File::open(&path)?;
                    let len = r.metadata()?.len();
                    let mut last = [0u8];
                    len > 0 && {
                        r.seek(SeekFrom::Start(len - 1))?;
                        r.read_exact(&mut last)?;
                        last[0] != b'\n'
                    }
                };
                let mut text = String::new();
                if ends_open {
                    text.push('\n');
                }
                for line in &fresh {
                    text.push_str(line);
                    text.push('\n');
                }
                f.write_all(text.as_bytes())?;
                self.index.add_lines(&fresh, false)?;
                debug!(
                    "sync: {} prompt history lines from other machines",
                    fresh.len()
                );
            }
            if let Some((id, _)) = page.lines.last() {
                self.history_cursor = *id;
                self.index.set_meta("history_cursor", *id)?;
            }
            if !page.more {
                return Ok(());
            }
        }
    }

    /// Sessions whose transcript the hub has newer than this machine.
    fn update_behind(&self) {
        let behind: HashSet<String> = self
            .remote
            .values()
            .filter(|r| is_transcript(&r.path))
            .filter(|r| {
                self.files
                    .get(&r.path)
                    .is_none_or(|s| s.version < r.version)
            })
            .filter(|r| r.host != self.host || !self.files.contains_key(&r.path))
            .filter_map(|r| session_of(&r.path).map(str::to_owned))
            .collect();
        if let Ok(mut b) = self.behind.lock() {
            *b = behind;
        }
    }
}

/// `projects/<dir>/<session>.jsonl`, a session's main transcript.
fn is_transcript(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() == 3 && parts[0] == "projects" && parts[2].ends_with(".jsonl")
}

/// The end of the last complete line between `from` and `size`, or `from`.
fn line_end(f: &mut File, from: u64, size: u64) -> io::Result<u64> {
    const BLOCK: u64 = 64 << 10;
    let mut end = size;
    let mut buf = vec![0; BLOCK as usize];
    while end > from {
        let start = end.saturating_sub(BLOCK).max(from);
        let len = (end - start) as usize;
        f.seek(SeekFrom::Start(start))?;
        f.read_exact(&mut buf[..len])?;
        if let Some(i) = buf[..len].iter().rposition(|b| *b == b'\n') {
            return Ok(start + i as u64 + 1);
        }
        end = start;
    }
    Ok(from)
}

/// Sessions running here, as the registry (`sessions/<pid>.json`) shows
/// them, read again at most every `REPORT_EVERY`.
struct Live {
    sessions: HashSet<String>,
    at: Instant,
}

impl Live {
    fn read(root: &Path) -> Self {
        let sessions = fs::read_dir(root.join("sessions"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                    .filter_map(|e| {
                        let v: serde_json::Value =
                            serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
                        let pid = u32::try_from(v.get("pid")?.as_u64()?).ok()?;
                        pid_alive(Some(pid))
                            .then(|| v.get("sessionId")?.as_str().map(str::to_owned))?
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            sessions,
            at: Instant::now(),
        }
    }

    fn refresh(&mut self, root: &Path) -> &Self {
        if self.at.elapsed() >= REPORT_EVERY {
            *self = Self::read(root);
        }
        self
    }

    /// Whether `path` belongs to a session running here.
    fn runs(&self, path: &str) -> bool {
        session_of(path).is_some_and(|s| self.sessions.contains(s))
    }
}

/// A failure to reach the hub or a refusal from it: stops the pass.
#[derive(Debug)]
struct HubError(String);

impl std::fmt::Display for HubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HubError {}

/// How long a name lookup of the hub is reused.
const DNS_TTL: Duration = Duration::from_secs(300);

/// Name lookups kept for `DNS_TTL`, and the last answer used when a lookup
/// fails. ureq resolves the host for every request, before it looks for a
/// pooled connection, so a backfill's thousands of requests went to the
/// DNS server one by one: Pi-hole's per-client limit (1000 a minute) then
/// refused every lookup from the whole machine.
#[derive(Debug, Default)]
struct CachedResolver {
    inner: DefaultResolver,
    /// `scheme://authority` → the addresses and when they were looked up.
    known: Mutex<HashMap<String, (ResolvedSocketAddrs, Instant)>>,
}

impl Resolver for CachedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let key = format!(
            "{}://{}",
            uri.scheme_str().unwrap_or_default(),
            uri.authority().map_or("", |a| a.as_str())
        );
        let cached = self.known.lock().ok().and_then(|k| k.get(&key).cloned());
        if let Some((addrs, at)) = &cached
            && at.elapsed() < DNS_TTL
        {
            return Ok(addrs.clone());
        }
        match self.inner.resolve(uri, config, timeout) {
            Ok(addrs) => {
                if let Ok(mut known) = self.known.lock() {
                    known.insert(key, (addrs.clone(), Instant::now()));
                }
                Ok(addrs)
            }
            Err(e) => match cached {
                Some((addrs, _)) => {
                    debug!("sync: looking up {key} failed ({e}); using the last answer");
                    Ok(addrs)
                }
                None => Err(e),
            },
        }
    }
}

/// The `/sync/` routes over HTTP, with this machine's collector token.
struct Client {
    agent: ureq::Agent,
    base: String,
    token_file: PathBuf,
}

impl Client {
    fn new(base: String, token_file: PathBuf) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .build();
        let agent =
            ureq::Agent::with_parts(config, DefaultConnector::new(), CachedResolver::default());
        Self {
            agent,
            base: base.trim_end_matches('/').to_owned(),
            token_file,
        }
    }

    fn bearer(&self) -> Result<String> {
        let token = fs::read_to_string(&self.token_file)
            .with_context(|| format!("reading {}", self.token_file.display()))?;
        Ok(format!("Bearer {}", token.trim()))
    }

    fn hub_err(&self, what: &str, e: impl std::fmt::Display) -> anyhow::Error {
        HubError(format!("{what} on {}: {e}", self.base)).into()
    }

    fn body(&self, what: &str, response: ureq::http::Response<ureq::Body>) -> Result<Vec<u8>> {
        let status = response.status();
        let body = response
            .into_body()
            .with_config()
            .limit(BODY_LIMIT as u64)
            .read_to_vec()
            .map_err(|e| self.hub_err(what, e))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&body);
            return Err(self.hub_err(what, format!("{status}: {}", text.trim())));
        }
        Ok(body)
    }

    fn get_json<T: DeserializeOwned>(&self, route: &str, query: &[(&str, String)]) -> Result<T> {
        let mut req = self
            .agent
            .get(format!("{}{route}", self.base))
            .header("Authorization", &self.bearer()?);
        for (k, v) in query {
            req = req.query(*k, v);
        }
        let response = req.call().map_err(|e| self.hub_err(route, e))?;
        Ok(serde_json::from_slice(&self.body(route, response)?)?)
    }

    fn post_json<T: DeserializeOwned>(&self, route: &str, body: &impl Serialize) -> Result<T> {
        let response = self
            .agent
            .post(format!("{}{route}", self.base))
            .header("Authorization", &self.bearer()?)
            .header("Content-Type", "application/json")
            .send(&serde_json::to_vec(body)?[..])
            .map_err(|e| self.hub_err(route, e))?;
        Ok(serde_json::from_slice(&self.body(route, response)?)?)
    }

    /// Up to a chunk of the hub's copy from `from`, with its row.
    fn get(&self, path: &str, from: u64) -> Result<Option<(Row, Vec<u8>)>> {
        let response = self
            .agent
            .get(format!("{}/sync/file", self.base))
            .header("Authorization", &self.bearer()?)
            .query("path", path)
            .query("from", from.to_string())
            .call()
            .map_err(|e| self.hub_err("/sync/file", e))?;
        if response.status() == 404 {
            return Ok(None);
        }
        let headers = response.headers().clone();
        let header = |name: &str| -> Result<String> {
            Ok(headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .with_context(|| format!("the hub sent no {name}"))?
                .to_owned())
        };
        let row = Row {
            path: path.to_owned(),
            size: header("x-sync-size")?.parse()?,
            version: header("x-sync-version")?.parse()?,
            base: header("x-sync-base")?.parse()?,
            host: header("x-sync-host")?,
            at: header("x-sync-at")?.parse()?,
        };
        let data = self.body("/sync/file", response)?;
        Ok(Some((row, data)))
    }

    /// Writes at `offset` expecting `expect`; the hub's row either way.
    fn put(
        &self,
        path: &str,
        offset: u64,
        expect: i64,
        data: &[u8],
    ) -> Result<Result<Row, Refused>> {
        let response = self
            .agent
            .put(format!("{}/sync/file", self.base))
            .header("Authorization", &self.bearer()?)
            .header("Content-Type", "application/octet-stream")
            .query("path", path)
            .query("offset", offset.to_string())
            .query("expect", expect.to_string())
            .send(data)
            .map_err(|e| self.hub_err("/sync/file", e))?;
        if response.status() == 409 {
            let body = response
                .into_body()
                .read_to_vec()
                .map_err(|e| self.hub_err("/sync/file", e))?;
            return Ok(Err(serde_json::from_slice(&body)?));
        }
        Ok(Ok(serde_json::from_slice(
            &self.body("/sync/file", response)?,
        )?))
    }
}

/// The thread's records, in `$XDG_STATE_HOME/ccdash/sync.db`.
struct Index {
    conn: Connection,
}

const INDEX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    version INTEGER NOT NULL,
    fp TEXT NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS remote (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    version INTEGER NOT NULL,
    base INTEGER NOT NULL,
    host TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value);
CREATE TABLE IF NOT EXISTS lines (
    digest BLOB PRIMARY KEY,
    pending TEXT
);
CREATE INDEX IF NOT EXISTS lines_pending ON lines (digest) WHERE pending IS NOT NULL;
";

impl Index {
    fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(INDEX_SCHEMA)?;
        Ok(Self { conn })
    }

    fn load(&self) -> Result<(HashMap<String, Synced>, HashMap<String, Row>)> {
        let files = self
            .conn
            .prepare("SELECT path, size, mtime, version, fp, deleted FROM files")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Synced {
                        size: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                        mtime: r.get(2)?,
                        version: r.get(3)?,
                        fp: r.get(4)?,
                        deleted: r.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let remote = self
            .conn
            .prepare("SELECT path, size, version, base, host, at FROM remote")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Row {
                        path: r.get(0)?,
                        size: r.get::<_, i64>(1)?.try_into().unwrap_or(0),
                        version: r.get(2)?,
                        base: r.get(3)?,
                        host: r.get(4)?,
                        at: r.get(5)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok((files, remote))
    }

    fn put_file(&self, path: &str, s: &Synced) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO files (path, size, mtime, version, fp, deleted)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                path,
                i64::try_from(s.size)?,
                s.mtime,
                s.version,
                s.fp,
                s.deleted
            ])?;
        Ok(())
    }

    fn del_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", [path])?;
        Ok(())
    }

    fn put_remote(&self, r: &Row) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO remote (path, size, version, base, host, at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                r.path,
                i64::try_from(r.size)?,
                r.version,
                r.base,
                r.host,
                r.at
            ])?;
        Ok(())
    }

    fn meta(&self, key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    fn text(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?
            .filter(|s: &String| !s.is_empty()))
    }

    fn set_meta(&self, key: &str, value: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    fn set_text(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    fn reset(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM files; DELETE FROM remote; DELETE FROM meta; DELETE FROM lines;",
        )?;
        Ok(())
    }

    /// Records prompt history lines this machine holds; `pending` ones
    /// still have to reach the hub.
    fn add_lines(&self, lines: &[&str], pending: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut insert =
                tx.prepare_cached("INSERT OR IGNORE INTO lines (digest, pending) VALUES (?1, ?2)")?;
            for line in lines {
                insert.execute(params![line_digest(line), pending.then_some(*line)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn knows_line(&self, line: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT 1 FROM lines WHERE digest = ?1")?
            .exists([line_digest(line)])?)
    }

    /// Lines still to send, up to about `max_bytes`.
    fn pending_lines(&self, max_bytes: usize) -> Result<Vec<(Vec<u8>, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT digest, pending FROM lines WHERE pending IS NOT NULL")?;
        let mut rows = stmt.query([])?;
        let (mut out, mut bytes) = (Vec::new(), 0);
        while let Some(r) = rows.next()? {
            let line: String = r.get(1)?;
            bytes += line.len();
            out.push((r.get(0)?, line));
            if bytes >= max_bytes {
                break;
            }
        }
        Ok(out)
    }

    fn sent_lines(&self, digests: &[&[u8]]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut update =
                tx.prepare_cached("UPDATE lines SET pending = NULL WHERE digest = ?1")?;
            for d in digests {
                update.execute([d])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

/// The hub's base URL from the collector's `/collect` WebSocket URL.
pub fn base_url(collect_url: &str) -> String {
    let url = collect_url
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    url.trim_end_matches('/')
        .trim_end_matches("/collect")
        .to_owned()
}

/// The Claude config dir: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn default_root() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(size: u64, mtime: i64) -> Option<Stat> {
        Some(Stat { size, mtime })
    }

    fn synced(size: u64, mtime: i64, version: i64) -> Synced {
        Synced {
            size,
            mtime,
            version,
            fp: String::new(),
            deleted: false,
        }
    }

    fn row(size: u64, version: i64, base: i64) -> Row {
        Row {
            path: "p".into(),
            size,
            version,
            base,
            host: "other".into(),
            at: 0,
        }
    }

    #[test]
    fn decisions() {
        let s = synced(10, 1, 5);
        // new here, new there, unknown on both
        assert_eq!(
            decide(stat(3, 1), None, None, None, false),
            Action::Upload { from: 0, expect: 0 }
        );
        assert_eq!(
            decide(None, None, Some(&row(3, 1, 1)), None, false),
            Action::Download { from: 0 }
        );
        assert_eq!(
            decide(None, None, Some(&row(3, 1, 1)), None, true),
            Action::Hold
        );
        assert_eq!(
            decide(stat(3, 1), None, Some(&row(3, 1, 1)), None, false),
            Action::Probe
        );
        // compared
        let r = row(20, 9, 1);
        assert_eq!(
            decide(stat(3, 1), None, Some(&r), Some(Verdict::Same), false),
            Action::Adopt { version: 9 }
        );
        assert_eq!(
            decide(stat(3, 1), None, Some(&r), Some(Verdict::Behind), false),
            Action::Download { from: 3 }
        );
        assert_eq!(
            decide(stat(30, 1), None, Some(&r), Some(Verdict::Ahead), true),
            Action::Upload {
                from: 20,
                expect: 9
            }
        );
        assert_eq!(
            decide(stat(3, 1), None, Some(&r), Some(Verdict::Diverged), false),
            Action::Conflict
        );
        // in step, grown here, grown there, both
        assert_eq!(
            decide(stat(10, 1), Some(&s), Some(&row(10, 5, 1)), None, false),
            Action::Nothing
        );
        assert_eq!(
            decide(stat(15, 2), Some(&s), Some(&row(10, 5, 1)), None, true),
            Action::Upload {
                from: 10,
                expect: 5
            }
        );
        assert_eq!(
            decide(stat(4, 2), Some(&s), Some(&row(10, 5, 1)), None, false),
            Action::Upload { from: 0, expect: 5 }
        );
        assert_eq!(
            decide(stat(10, 1), Some(&s), Some(&row(18, 7, 1)), None, false),
            Action::Download { from: 10 }
        );
        assert_eq!(
            decide(stat(10, 1), Some(&s), Some(&row(18, 7, 6)), None, false),
            Action::Download { from: 0 }
        );
        assert_eq!(
            decide(stat(10, 1), Some(&s), Some(&row(18, 7, 1)), None, true),
            Action::Hold
        );
        assert_eq!(
            decide(stat(12, 2), Some(&s), Some(&row(18, 7, 1)), None, false),
            Action::Conflict
        );
        // deleted here: remembered, never fetched again
        assert_eq!(
            decide(None, Some(&s), Some(&row(18, 7, 1)), None, false),
            Action::Forget
        );
        let gone = Synced {
            deleted: true,
            ..s.clone()
        };
        assert_eq!(
            decide(None, Some(&gone), Some(&row(18, 7, 1)), None, false),
            Action::Nothing
        );
        // the hub lost it
        assert_eq!(
            decide(stat(10, 1), Some(&s), None, None, false),
            Action::Upload { from: 0, expect: 0 }
        );
    }

    #[test]
    fn lookups_are_reused_and_survive_failures() {
        let resolver = CachedResolver::default();
        let config = ureq::Agent::config_builder().build();
        let timeout = || NextTimeout {
            after: ureq::unversioned::transport::time::Duration::NotHappening,
            reason: ureq::Timeout::Resolve,
        };
        let uri: ureq::http::Uri = "http://localhost:7837/sync/changes".parse().unwrap();
        let first = resolver.resolve(&uri, &config, timeout()).unwrap();
        assert!(!first.is_empty());
        // the same host again comes from the cache, even with the answer swapped
        let key = "http://localhost:7837".to_owned();
        let mut planted = first.clone();
        planted.truncate(1);
        planted[0] = "192.0.2.7:7837".parse().unwrap();
        resolver
            .known
            .lock()
            .unwrap()
            .insert(key.clone(), (planted.clone(), Instant::now()));
        assert!(resolver.resolve(&uri, &config, timeout()).unwrap() == planted);
        // a failing lookup falls back to the last answer, however old
        let gone: ureq::http::Uri = "http://no-such-host.invalid/x".parse().unwrap();
        assert!(resolver.resolve(&gone, &config, timeout()).is_err());
        resolver.known.lock().unwrap().insert(
            "http://no-such-host.invalid".into(),
            (planted.clone(), Instant::now() - DNS_TTL * 2),
        );
        assert!(resolver.resolve(&gone, &config, timeout()).unwrap() == planted);
    }

    #[test]
    fn urls_paths_and_line_ends() {
        assert_eq!(
            base_url("wss://ccdash.home.example/collect"),
            "https://ccdash.home.example"
        );
        assert_eq!(
            base_url("ws://127.0.0.1:7837/collect/"),
            "http://127.0.0.1:7837"
        );
        assert!(is_transcript(
            "projects/-w/1ee78b00-59c7-4bf5-b462-ef39161f7d8d.jsonl"
        ));
        assert!(!is_transcript(
            "projects/-w/1ee78b00-59c7-4bf5-b462-ef39161f7d8d/subagents/a.jsonl"
        ));
        let dir = std::env::temp_dir().join(format!("ccdash-lines-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        fs::write(&path, b"{\"a\":1}\n{\"b\":2}\n{\"c\":").unwrap();
        let mut f = File::open(&path).unwrap();
        assert_eq!(line_end(&mut f, 0, 21).unwrap(), 16);
        assert_eq!(line_end(&mut f, 16, 21).unwrap(), 16);
        assert_eq!(line_end(&mut f, 0, 8).unwrap(), 8);
        fs::remove_dir_all(dir).unwrap();
    }
}
