//! Messages between a collector and the hub over the `/collect` WebSocket:
//! JSON text frames, one message each. The collector opens with `Hello`,
//! the hub answers with `Welcome`; either side closes on a version it does
//! not speak.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
use crate::state::Window;
use crate::{
    ask::Answer,
    knowledge::{Hit, Note, NoteOp, NoteView, Query, Upsert, Written},
    model::{Claim, Kind, RateLimits, ScopedLimit, Session, Update},
    state::{AgentEntry, NiriEvent, ProcPoll},
    sync::SyncStatus,
    wire::Envelope,
};

/// 6: Claude Code's state on the hub (`Sync` up, `SyncChanged` and
/// `SyncResync` down) and the one-machine guard (`Call::Guard`).
/// 5: `Dirs`, the project directories under each workspace on the
/// collector's machine (where sessions can start).
/// 4: sessions started, stopped and opened from the panel (`Launch`,
/// `Stop`, `Open` down; `Launched`, `Stopped` up), `branch` and `launched`
/// on sessions, `branch` on agents entries and envelopes, `launch` on
/// envelopes.
pub const VERSION: u32 = 6;

/// Collector → hub.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHub {
    /// First message on a link.
    Hello {
        v: u32,
        host: String,
        /// Window `app_id`s that count as terminals on this host.
        terminals: Vec<String>,
    },
    /// A hook or statusline payload, as `ccdash emit` sent it.
    Envelope {
        envelope: Envelope,
    },
    /// One `claude agents --json` poll; `dead` lists the pids of this
    /// host's known sessions that no longer exist.
    Agents {
        list: Vec<AgentEntry>,
        at: i64,
        dead: Vec<u32>,
    },
    Niri {
        event: NiriEvent,
    },
    Procs {
        polls: Vec<ProcPoll>,
        at: i64,
    },
    Usage {
        limits: Vec<ScopedLimit>,
        at: i64,
    },
    Telemetry {
        records: Vec<TelemetryRecord>,
    },
    /// A `ccdash channel` of the Claude process `pid` connected or left.
    ChannelOpen {
        pid: u32,
    },
    ChannelClosed {
        pid: u32,
    },
    /// `ccdash ask` holds a prompt open; the collector picked `id`.
    Ask {
        id: String,
        envelope: Envelope,
    },
    /// The hook holding ask `id` went away.
    AskGone {
        id: String,
    },
    /// A request from the collector's local API, answered by `Result`.
    Call {
        id: u64,
        call: Call,
    },
    /// A write made while the hub was unreachable, replayed from the
    /// outbox; carried out without an answer. `at` is when it was made.
    Queued {
        call: Call,
        at: i64,
    },
    /// The UserPromptSubmit hook of `session` took the context the hub
    /// sent, so the messages delivered in it were read.
    Injected {
        session: String,
    },
    /// What `claude --bg` made of launch `launch`: the id it printed, or
    /// why it failed.
    Launched {
        launch: String,
        #[serde(default)]
        session: Option<String>,
        #[serde(default)]
        error: Option<String>,
    },
    /// Stopping `session` went through (`claude stop` or SIGTERM), or not.
    Stopped {
        session: String,
        #[serde(default)]
        error: Option<String>,
    },
    /// Workspace name → the directories directly under it on this machine;
    /// a workspace whose directory is missing here is left out. Sent after
    /// each welcome and when it changes.
    Dirs {
        dirs: BTreeMap<String, Vec<String>>,
    },
    /// How this machine's copy of Claude Code's state stands.
    Sync {
        status: SyncStatus,
    },
}

impl ToHub {
    /// Kept in the on-disk queue while the hub is unreachable; polls are
    /// not, the next one replaces them.
    pub fn durable(&self) -> bool {
        matches!(
            self,
            Self::Envelope { .. } | Self::Telemetry { .. } | Self::Queued { .. }
        )
    }
}

/// Who makes a knowledge call: enough for the hub to find the session and
/// its workspace. Hooks know the session id and cwd; the channel's MCP
/// server knows its parent pid and the project directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Who {
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub cwd: Option<String>,
}

/// What a hook wants injected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Injection {
    /// Open issues, relevant memories, documents and recent progress.
    SessionStart,
    /// Other sessions active in the same project; with `since`, also what
    /// changed in the workspace since then (a session that started while
    /// the hub was unreachable catching up).
    Prompt {
        #[serde(default)]
        since: Option<i64>,
    },
    /// Who else edited `path` lately.
    Edit { path: String },
}

/// A local request that only the hub can carry out.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum Call {
    Rename {
        session: String,
        title: String,
    },
    Reply {
        session: String,
        text: String,
    },
    Answer {
        session: String,
        ask: String,
        answer: Answer,
    },
    /// A knowledge write, scoped to the caller's workspace.
    Note {
        who: Who,
        op: NoteOp,
    },
    /// A knowledge read.
    Recall {
        who: Who,
        query: Query,
    },
    /// Context for a hook.
    Inject {
        who: Who,
        what: Injection,
    },
    /// A message to another session of the caller's workspace, handed to
    /// it at its next prompt.
    Tell {
        who: Who,
        to: String,
        text: String,
    },
    /// What the caller is working on; paths are relative to its cwd,
    /// `until` is minutes from now and `issue` the slug of the issue.
    Claim {
        who: Who,
        what: String,
        #[serde(default)]
        paths: Vec<String>,
        #[serde(default)]
        until: Option<i64>,
        #[serde(default)]
        issue: Option<String>,
    },
    /// Drops the caller's claim.
    Release {
        who: Who,
    },
    /// `ccdash import`: notes for the workspace holding `dir`.
    Import {
        dir: String,
        notes: Vec<Upsert>,
    },
    /// Whether the caller's session is open on another machine.
    Guard {
        who: Who,
    },
}

/// What a `Call` returns.
// NOTE: adjacently tagged: internally tagged enums cannot hold lists or plain values
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", content = "data", rename_all = "snake_case")]
pub enum Reply {
    Session(Box<Session>),
    Written(Written),
    Hits(Vec<Hit>),
    Note(Box<NoteView>),
    /// None when there is nothing to inject.
    Context(Option<String>),
    /// The caller's claim after `Claim` or `Release`.
    Claimed(Option<Claim>),
    Imported {
        added: usize,
        updated: usize,
        unchanged: usize,
    },
    Sent,
    /// Why the session must not go on here; none when it may.
    Guard(Option<String>),
}

/// A configured workspace, so collectors can scope offline reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDir {
    pub name: String,
    pub dir: String,
}

/// One OpenTelemetry metric point or event a collector received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryRecord {
    pub ts: i64,
    pub session: String,
    pub name: String,
    pub data: Value,
}

/// Hub → collector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToCollector {
    Welcome {
        v: u32,
        host: String,
        /// Where the web UI lives, for links opened on the collector's host.
        web_url: String,
        workspaces: Vec<WorkspaceDir>,
    },
    /// This host's sessions as the hub sees them; replaces the mirror.
    Snapshot {
        sessions: Vec<Session>,
        rate_limits: Option<RateLimits>,
    },
    Update {
        update: Box<Update>,
    },
    /// A message for the session run by `pid`, through its channel.
    Say {
        pid: u32,
        text: String,
    },
    /// The PermissionRequest decision for ask `id`.
    Decision {
        id: String,
        decision: Value,
    },
    /// Ask `id` closed elsewhere; let the hook go.
    AskRelease {
        id: String,
    },
    /// Queue `title` for the session's next prompt.
    Title {
        session: String,
        title: String,
    },
    Result {
        id: u64,
        result: Result<Reply, String>,
    },
    /// The live notes of one workspace, replacing the collector's cache
    /// of it; sent after the welcome and after the nightly pass.
    Notes {
        workspace: String,
        notes: Vec<Note>,
    },
    /// A note changed; one no longer live leaves the cache.
    NoteChanged {
        note: Box<Note>,
    },
    /// Starts a background session in `cwd` with `prompt` (`claude --bg`),
    /// or continues session `resume`; answered by `Launched`. The hooks of
    /// the new session carry `launch` in their envelopes.
    Launch {
        launch: String,
        cwd: String,
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        permission_mode: Option<String>,
        #[serde(default)]
        resume: Option<String>,
    },
    /// Stops a running session: `claude stop` for a background one,
    /// SIGTERM to `pid` for an interactive one; answered by `Stopped`.
    Stop {
        session: String,
        pid: Option<u32>,
        kind: Kind,
    },
    /// Focuses the session's window, or attaches to a background session
    /// (what `ccdash open` does on that host).
    Open {
        session: String,
    },
    /// The hub's copy of Claude Code's state changed; `seq` counts writes.
    SyncChanged {
        seq: u64,
    },
    /// Forget the sync records and compare everything again.
    SyncResync,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_round_trip_through_json() {
        let hello = ToHub::Hello {
            v: VERSION,
            host: "navi".into(),
            terminals: vec!["ghostty".into()],
        };
        let text = serde_json::to_string(&hello).unwrap();
        assert!(text.starts_with(r#"{"type":"hello""#), "{text}");
        assert!(matches!(
            serde_json::from_str(&text).unwrap(),
            ToHub::Hello { v: VERSION, .. }
        ));
        let call = ToHub::Call {
            id: 3,
            call: Call::Answer {
                session: "s".into(),
                ask: "a".into(),
                answer: Answer::Deny {
                    message: Some("no".into()),
                },
            },
        };
        let back: ToHub = serde_json::from_str(&serde_json::to_string(&call).unwrap()).unwrap();
        assert!(matches!(
            back,
            ToHub::Call {
                id: 3,
                call: Call::Answer { .. }
            }
        ));
        assert!(!hello.durable());
        assert!(
            ToHub::Telemetry {
                records: vec![TelemetryRecord {
                    ts: 1,
                    session: "s".into(),
                    name: "api_request".into(),
                    data: json!({}),
                }],
            }
            .durable()
        );
        let windows = ToHub::Niri {
            event: NiriEvent::Windows(vec![Window {
                id: 1,
                app_id: "ghostty".into(),
                title: "✳ x".into(),
                focused: true,
            }]),
        };
        let back: ToHub = serde_json::from_str(&serde_json::to_string(&windows).unwrap()).unwrap();
        assert!(matches!(back, ToHub::Niri { event: NiriEvent::Windows(w) } if w.len() == 1));
        let result = ToCollector::Result {
            id: 3,
            result: Err("gone".into()),
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({"type": "result", "id": 3, "result": {"Err": "gone"}})
        );
        let sent = ToCollector::Result {
            id: 4,
            result: Ok(Reply::Sent),
        };
        assert_eq!(
            serde_json::to_value(&sent).unwrap(),
            json!({"type": "result", "id": 4, "result": {"Ok": {"reply": "sent"}}})
        );
        let queued = ToHub::Queued {
            call: Call::Note {
                who: Who {
                    session: None,
                    pid: Some(7),
                    cwd: Some("/w".into()),
                },
                op: NoteOp::Verify { slug: "a".into() },
            },
            at: 9,
        };
        assert!(queued.durable());
        let text = serde_json::to_string(&queued).unwrap();
        assert!(
            text.contains(r#""call":"note""#) && text.contains(r#""op":"verify""#),
            "{text}"
        );
        let back: ToHub = serde_json::from_str(&text).unwrap();
        assert!(matches!(back, ToHub::Queued { at: 9, .. }));
        let claim = ToHub::Queued {
            call: Call::Claim {
                who: Who {
                    session: Some("s".into()),
                    pid: None,
                    cwd: None,
                },
                what: "w".into(),
                paths: vec!["src".into()],
                until: None,
                issue: Some("i".into()),
            },
            at: 1,
        };
        assert!(claim.durable());
        let back: ToHub = serde_json::from_str(&serde_json::to_string(&claim).unwrap()).unwrap();
        assert!(
            matches!(back, ToHub::Queued { call: Call::Claim { ref paths, .. }, .. } if paths == &["src"])
        );
        let launched: ToHub =
            serde_json::from_str(r#"{"type":"launched","launch":"l","session":"9c4719e5"}"#)
                .unwrap();
        assert!(
            matches!(launched, ToHub::Launched { ref session, error: None, .. } if session.as_deref() == Some("9c4719e5"))
        );
        assert!(!launched.durable());
        let launch = ToCollector::Launch {
            launch: "l".into(),
            cwd: "/w/p".into(),
            prompt: "hi".into(),
            model: None,
            permission_mode: Some("plan".into()),
            resume: None,
        };
        let text = serde_json::to_string(&launch).unwrap();
        assert!(text.starts_with(r#"{"type":"launch""#), "{text}");
        let stop: ToCollector =
            serde_json::from_str(r#"{"type":"stop","session":"s","pid":7,"kind":"background"}"#)
                .unwrap();
        assert!(matches!(
            stop,
            ToCollector::Stop {
                pid: Some(7),
                kind: Kind::Background,
                ..
            }
        ));
        let dirs = ToHub::Dirs {
            dirs: BTreeMap::from([("w".to_owned(), vec!["ccdash".to_owned()])]),
        };
        let text = serde_json::to_string(&dirs).unwrap();
        assert_eq!(text, r#"{"type":"dirs","dirs":{"w":["ccdash"]}}"#);
        assert!(!dirs.durable());
        let sync = ToHub::Sync {
            status: SyncStatus::default(),
        };
        assert!(!sync.durable());
        assert!(
            serde_json::to_string(&sync)
                .unwrap()
                .starts_with(r#"{"type":"sync""#)
        );
        let changed: ToCollector =
            serde_json::from_str(r#"{"type":"sync_changed","seq":3}"#).unwrap();
        assert!(matches!(changed, ToCollector::SyncChanged { seq: 3 }));
        let injected: ToHub = serde_json::from_str(r#"{"type":"injected","session":"s"}"#).unwrap();
        assert!(matches!(injected, ToHub::Injected { ref session } if session == "s"));
        assert!(!injected.durable());
        for reply in [
            Reply::Hits(Vec::new()),
            Reply::Context(None),
            Reply::Context(Some("x".into())),
            Reply::Claimed(None),
            Reply::Claimed(Some(Claim {
                what: "w".into(),
                paths: vec!["/a".into()],
                issue: None,
                since: 1,
                until: Some(2),
            })),
            Reply::Imported {
                added: 1,
                updated: 0,
                unchanged: 2,
            },
            Reply::Guard(None),
            Reply::Guard(Some("open on navi".into())),
        ] {
            let text = serde_json::to_string(&reply).unwrap();
            let back: Reply = serde_json::from_str(&text).unwrap();
            assert_eq!(back, reply, "{text}");
        }
    }
}
