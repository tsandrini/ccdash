//! Types shared by the daemon state and every API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Started, no prompt yet.
    Idle,
    Working,
    /// Blocked on a permission prompt.
    Permission,
    /// Turn finished; waiting for the next prompt.
    YourTurn,
    Gone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Interactive,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamped {
    pub at: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRun {
    pub name: String,
    pub summary: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRef {
    pub id: u64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    pub used_pct: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimits {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub host: String,
    pub id: String,
    /// Background session id as shown by `claude agents`.
    pub short_id: Option<String>,
    pub kind: Kind,
    pub pid: Option<u32>,
    pub cwd: String,
    /// Name from `claude agents`.
    pub name: Option<String>,
    /// Conversation title from the statusline (also the window title).
    pub title: Option<String>,
    pub activity: Activity,
    pub activity_since: i64,
    pub waiting_for: Option<String>,
    pub tool: Option<ToolRun>,
    pub prompt: Option<Stamped>,
    pub reply: Option<Stamped>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    pub context_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub remote_url: Option<String>,
    pub window: Option<WindowRef>,
    pub started_at: i64,
    pub last_event_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub host: String,
    pub now: i64,
    pub sessions: Vec<Session>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Update {
    Session { session: Box<Session> },
    Removed { id: String },
    RateLimits { rate_limits: RateLimits },
}
