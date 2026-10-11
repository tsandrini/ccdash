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

/// Progress of the current or last turn. Counts cover the main thread;
/// `subagent_tools` counts tool calls made by subagents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub tools: u32,
    pub failures: u32,
    pub subagent_tools: u32,
    /// Edited files, relative to the session cwd, in first-edit order.
    pub files: Vec<String>,
}

impl Turn {
    const MAX_FILES: usize = 50;

    /// Counts a finished main-thread tool call; successful edits add their file.
    pub fn note_tool(&mut self, name: &str, summary: &str, ok: bool) {
        self.tools += 1;
        self.failures += u32::from(!ok);
        if ok
            && matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
            && !summary.is_empty()
            && self.files.len() < Self::MAX_FILES
            && !self.files.iter().any(|f| f == summary)
        {
            self.files.push(summary.to_owned());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// An entry of Claude's task list (TaskCreate/TaskUpdate or TodoWrite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    /// Present-tense label shown while the task is in progress.
    pub active_form: Option<String>,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    pub id: String,
    pub agent_type: String,
    /// The `description` the main thread gave the Agent tool.
    pub description: Option<String>,
    pub started_at: i64,
    pub tools: u32,
    pub tool: Option<ToolRun>,
}

/// CPU and memory of a session's process tree (Claude, its MCP servers and
/// running commands).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resources {
    /// Percent of one core since the previous poll.
    pub cpu_pct: f64,
    /// Proportional set size of the tree: shared pages are split between the
    /// processes that map them, so sessions add up.
    pub mem_bytes: u64,
    pub procs: u32,
    /// Busiest processes of the tree, Claude itself included.
    pub top: Vec<ProcInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    /// Command line, shortened and with obvious secrets masked.
    pub cmd: String,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
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
    /// Start of the last prompt; the full text is in the conversation.
    pub prompt: Option<Stamped>,
    /// Start of Claude's last final reply.
    pub reply: Option<Stamped>,
    /// End of what Claude is saying in the current turn, as it streams.
    pub message: Option<Stamped>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub context_pct: Option<f64>,
    pub cost_usd: Option<f64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub remote_url: Option<String>,
    pub window: Option<WindowRef>,
    /// Hooks or the statusline have reported; false for sessions started
    /// before ccdash was installed, which only the agents poll sees.
    pub hooked: bool,
    pub resources: Option<Resources>,
    pub turn: Option<Turn>,
    pub tasks: Vec<Task>,
    pub subagents: Vec<Subagent>,
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
