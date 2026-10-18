//! Messages between a collector and the hub over the `/collect` WebSocket:
//! JSON text frames, one message each. The collector opens with `Hello`,
//! the hub answers with `Welcome`; either side closes on a version it does
//! not speak.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
use crate::state::Window;
use crate::{
    ask::Answer,
    model::{RateLimits, ScopedLimit, Session, Update},
    state::{AgentEntry, NiriEvent, ProcPoll},
    wire::Envelope,
};

pub const VERSION: u32 = 1;

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
}

impl ToHub {
    /// Kept in the on-disk queue while the hub is unreachable; polls are
    /// not, the next one replaces them.
    pub fn durable(&self) -> bool {
        matches!(self, Self::Envelope { .. } | Self::Telemetry { .. })
    }
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
        workspaces: Vec<String>,
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
        result: Result<Box<Session>, String>,
    },
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
    }
}
