//! Claude Code's state on the hub (H7, `meta/plans/ccdash-hub/h7.md`):
//! sessions, rewind checkpoints, plans, task lists, pasted content and the
//! prompt history, kept by the hub and mirrored by every collector. Files
//! travel byte for byte; a file that only grew travels as its new tail.
//!
//! This module holds what both sides share: the index row, the wire types
//! of the `/sync/` routes, path rules and the fingerprint that tells an
//! append from a rewrite.

pub mod collector;
pub mod hub;
mod watch;

use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path},
};

use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Largest piece of a file one request carries.
pub const CHUNK: usize = 8 << 20;
/// Request bodies the `/sync/` routes take: a chunk plus headroom.
pub const BODY_LIMIT: usize = CHUNK + (1 << 20);
/// Bytes before an offset that its fingerprint covers.
const FP_WINDOW: u64 = 64 << 10;
/// The prompt history, merged by line instead of copied.
pub const HISTORY: &str = "history.jsonl";
/// Where a collector sets aside its copy of a file that diverged.
pub const CONFLICTS_DIR: &str = "ccdash-conflicts";

/// One file as the hub indexes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// Relative to the Claude config dir.
    pub path: String,
    pub size: u64,
    /// The hub-wide write counter at this file's last write.
    pub version: i64,
    /// The version of the last write that was not an append: bytes a
    /// reader holds stay valid while `base` is at most what it read.
    pub base: i64,
    /// The host that wrote last.
    pub host: String,
    pub at: i64,
}

/// `GET /sync/changes`: rows written after `since`, oldest first.
#[derive(Debug, Serialize, Deserialize)]
pub struct Changes {
    pub rows: Vec<Row>,
    pub more: bool,
}

/// One file a collector holds without a sync record, for `/sync/compare`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probe {
    pub path: String,
    pub size: u64,
    /// `fp` at `size`.
    pub fp: String,
}

/// The hub's side of a probe: its row, and its fingerprint at the smaller
/// of the two sizes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Probed {
    pub path: String,
    pub row: Option<Row>,
    pub fp: Option<String>,
}

/// `PUT /sync/file`, refused: the hub's row as it stands, and the host
/// holding the file's session when that is why.
#[derive(Debug, Serialize, Deserialize)]
pub struct Refused {
    pub row: Option<Row>,
    #[serde(default)]
    pub held_by: Option<String>,
}

/// `GET /sync/history`: prompt history lines after `since`, oldest first.
#[derive(Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub lines: Vec<(i64, String)>,
    pub more: bool,
}

/// A file a collector moved aside because both sides changed it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictReport {
    pub path: String,
    pub size: u64,
    /// Where the collector keeps its copy, relative to the Claude config dir.
    pub copy: String,
    #[serde(default)]
    pub details: Option<ConflictDetails>,
    /// The copy is the version the hub had before this machine's copy
    /// replaced it ("use this copy").
    #[serde(default)]
    pub former_hub: bool,
}

/// How a set-aside copy compares with the version in place, worked out
/// by the collector that holds both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ConflictDetails {
    /// The version in place (the hub's) and the copy.
    pub current_size: u64,
    pub copy_size: u64,
    /// Bytes both start with.
    pub shared: u64,
    /// Lines each has after the shared part.
    pub current_lines: u64,
    pub copy_lines: u64,
    /// When each was last written.
    pub current_mtime: i64,
    pub copy_mtime: i64,
}

/// Every copy a collector holds under `ccdash-conflicts/`, sent after each
/// full pass and each resolution; the hub's list follows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictState {
    pub copies: Vec<ConflictReport>,
}

/// Which version of a conflicted file stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// The version in place: the copy is deleted.
    Hub,
    /// The copy: it replaces the hub's version on every machine, and the
    /// version it replaced is set aside in its place.
    Copy,
}

/// What a collector reports about its copy (`ToHub::Sync`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SyncStatus {
    /// Files and bytes this machine holds of what travels.
    pub files: u64,
    pub bytes: u64,
    /// Files waiting to go up or come down after the last pass.
    pub pending_up: u64,
    pub pending_down: u64,
    /// Files held back because their session runs here.
    pub held: u64,
    /// Conflicts set aside since the collector started.
    pub conflicts: u64,
    pub last_pass: Option<i64>,
    pub error: Option<String>,
    /// While a first run or a resync compares every file: done and total.
    pub backfill: Option<(u64, u64)>,
    /// Changes are seen through inotify; otherwise the collector scans
    /// everything every `sync.interval_secs`.
    #[serde(default)]
    pub watching: bool,
}

/// One machine in the panel's Sync section.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SyncHost {
    pub host: String,
    pub connected: bool,
    /// Its last report; none from a collector without sync.
    pub status: Option<SyncStatus>,
}

/// A conflict the hub recorded.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SyncConflict {
    pub id: i64,
    pub path: String,
    pub host: String,
    pub size: u64,
    pub copy: String,
    pub at: i64,
    /// The session the file belongs to, and its title when the hub knows it.
    pub session: Option<String>,
    pub title: Option<String>,
    pub details: Option<ConflictDetails>,
    pub former_hub: bool,
    /// A resolution sent to the host and not confirmed yet.
    pub pending: Option<Keep>,
    /// Why the last resolution failed.
    pub error: Option<String>,
}

/// Bytes and files of a part of the state.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct StatSize {
    pub files: u64,
    pub bytes: u64,
}

/// One part of the state (a kind of file, or a project's sessions) with
/// what it grew by.
#[derive(Debug, Clone, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct StatGroup {
    /// `projects/<dir>`, or the top-level directory (`file-history`, …).
    pub key: String,
    /// `workspace/project` for a project's sessions when the hub knows the
    /// directory, else the key.
    pub label: String,
    pub size: StatSize,
    /// Sessions in it (projects only).
    pub sessions: u64,
    /// Growth over the last day, week and 30 days, in bytes.
    pub day: i64,
    pub week: i64,
    pub month: i64,
}

/// A session's files (transcript, subagents, tool results, rewind
/// checkpoints, tasks).
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct StatSession {
    pub session: String,
    pub title: Option<String>,
    pub label: Option<String>,
    pub host: Option<String>,
    pub size: StatSize,
    pub last_write: i64,
}

/// `GET /api/v1/sync/stats`.
#[derive(Debug, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SyncStats {
    pub total: StatSize,
    /// By top-level directory.
    pub kinds: Vec<StatGroup>,
    /// By project directory, largest first.
    pub projects: Vec<StatGroup>,
    /// The largest sessions.
    pub sessions: Vec<StatSession>,
    /// Bytes added per day (local dates), oldest first, the last 30 days.
    pub days: Vec<(String, i64)>,
    /// The first day growth was recorded.
    pub since: Option<String>,
    /// Prompt history lines and bytes.
    pub history: StatSize,
}

/// `GET /api/v1/sync`.
#[derive(Debug, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct SyncView {
    pub hosts: Vec<SyncHost>,
    /// What the hub holds.
    pub files: u64,
    pub bytes: u64,
    pub conflicts: Vec<SyncConflict>,
}

/// SHA-256 (hex) of the `FP_WINDOW` bytes of `file` before `n`, the whole
/// prefix when shorter. Equal fingerprints at equal offsets mean the same
/// bytes there; after a growth they tell an append from a rewrite.
///
/// # Errors
///
/// Reading the file fails, or it is shorter than `n`.
pub fn fp(file: &mut File, n: u64) -> io::Result<String> {
    let start = n.saturating_sub(FP_WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut window = Vec::with_capacity((n - start) as usize);
    file.take(n - start).read_to_end(&mut window)?;
    if window.len() as u64 != n - start {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the file is shorter than the offset",
        ));
    }
    Ok(hex(digest(&SHA256, &window).as_ref()))
}

/// `fp` of the file at `path`.
///
/// # Errors
///
/// As `fp`, or opening the file fails.
pub fn fp_of(path: &Path, n: u64) -> io::Result<String> {
    fp(&mut File::open(path)?, n)
}

/// SHA-256 of a prompt history line, its identity across machines.
pub fn line_digest(line: &str) -> Vec<u8> {
    digest(&SHA256, line.as_bytes()).as_ref().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A relative path made of plain segments only (no `..`, no root, no
/// empty or hidden-dot tricks), short enough to store.
pub fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\0')
        && !path.ends_with('/')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        && !path
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

/// The session a path belongs to: the first segment (or `.jsonl` file
/// stem) that is a session id. Transcripts, their subagents and tool
/// results, rewind checkpoints and task lists all carry it.
pub fn session_of(path: &str) -> Option<&str> {
    path.split('/')
        .map(|seg| seg.strip_suffix(".jsonl").unwrap_or(seg))
        .find(|seg| is_uuid(seg))
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Shell-style match: `*` spans any characters, `/` included, `?` one.
pub fn glob(pattern: &str, path: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), path.chars().collect());
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, ss + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_sessions() {
        let id = "1ee78b00-59c7-4bf5-b462-ef39161f7d8d";
        assert!(valid_path("projects/-home-x/a.jsonl"));
        for bad in ["", "/etc/passwd", "a/../b", "a//b", "./a", "a/", "a/./b"] {
            assert!(!valid_path(bad), "{bad}");
        }
        assert_eq!(
            session_of(&format!("projects/-home-x/{id}.jsonl")),
            Some(id)
        );
        assert_eq!(
            session_of(&format!("projects/-home-x/{id}/subagents/agent-1.jsonl")),
            Some(id)
        );
        assert_eq!(session_of(&format!("file-history/{id}/abc@v2")), Some(id));
        assert_eq!(session_of("plans/a-plan.md"), None);
        assert_eq!(session_of(HISTORY), None);
    }

    #[test]
    fn globs_span_directories() {
        assert!(glob("projects/*", "projects/-home-x/a.jsonl"));
        assert!(glob("history.jsonl", "history.jsonl"));
        assert!(glob("projects/-tmp-*", "projects/-tmp-abc/s.jsonl"));
        assert!(!glob("projects/-tmp-*", "projects/-home-x/s.jsonl"));
        assert!(!glob("plans/*", "projects/x"));
        assert!(glob("a?c", "abc"));
    }

    #[test]
    fn fingerprints_tell_appends_from_rewrites() {
        let dir = std::env::temp_dir().join(format!("ccdash-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        let base: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &base).unwrap();
        let before = fp_of(&path, base.len() as u64).unwrap();
        let mut grown = base.clone();
        grown.extend_from_slice(b"more");
        std::fs::write(&path, &grown).unwrap();
        assert_eq!(fp_of(&path, base.len() as u64).unwrap(), before);
        let mut rewritten = base.clone();
        rewritten[99_990] ^= 1;
        std::fs::write(&path, &rewritten).unwrap();
        assert_ne!(fp_of(&path, base.len() as u64).unwrap(), before);
        assert!(fp_of(&path, 1 << 30).is_err());
        assert_eq!(fp_of(&path, 0).unwrap(), hex(digest(&SHA256, b"").as_ref()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
