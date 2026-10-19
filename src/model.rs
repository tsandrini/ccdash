//! Types shared by the daemon state and every API.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum Activity {
    /// Started, no prompt yet.
    Idle,
    Working,
    /// Blocked on a permission prompt.
    Permission,
    /// Turn finished; waiting for the next prompt.
    YourTurn,
    /// Your turn for over `PARKED_AFTER_MS`: most likely done, not counted as
    /// waiting on you.
    Parked,
    Gone,
}

/// How long an interactive session stays "your turn" before it is parked.
pub const PARKED_AFTER_MS: i64 = 60 * 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Interactive,
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Stamped {
    pub at: i64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ToolRun {
    pub name: String,
    pub summary: String,
    pub started_at: i64,
}

/// Progress of the current or last turn. Counts cover the main thread;
/// `subagent_tools` counts tool calls made by subagents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

/// An entry of Claude's task list (TaskCreate/TaskUpdate or TodoWrite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Task {
    pub id: String,
    pub subject: String,
    /// Present-tense label shown while the task is in progress.
    pub active_form: Option<String>,
    pub status: TaskStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    /// Command line, shortened and with obvious secrets masked.
    pub cmd: String,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub started_at: i64,
}

/// A prompt Claude Code holds open, relayed by `ccdash ask` (the
/// PermissionRequest hook). The terminal dialog stays open alongside it;
/// whichever answer comes first wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Ask {
    pub id: String,
    /// The subagent that asked; None for the main thread.
    pub agent_id: Option<String>,
    pub at: i64,
    #[serde(flatten)]
    pub kind: AskKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AskKind {
    /// A tool call waiting for approval.
    Tool {
        tool: String,
        /// What the call does, in Claude's words, when it said.
        description: Option<String>,
        /// The call's arguments as `(field, text)`, shortened.
        input: Vec<(String, String)>,
        /// What "always allow" would allow, from Claude Code's suggestions.
        always: Option<String>,
    },
    /// `AskUserQuestion`: one to four multiple-choice questions.
    Question { questions: Vec<Question> },
    /// `ExitPlanMode`: a plan to approve, as markdown.
    Plan { plan: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<Choice>,
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Choice {
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct WindowRef {
    pub id: u64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Limit {
    pub used_pct: f64,
    /// Unix seconds.
    pub resets_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct RateLimits {
    pub five_hour: Option<Limit>,
    pub seven_day: Option<Limit>,
    /// When the statusline last reported the two above.
    pub updated_at: i64,
    /// Weekly limits of one model or surface, such as Fable's, from `/usage`.
    #[serde(default)]
    pub scoped: Vec<ScopedLimit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct ScopedLimit {
    /// The server's label for the model or surface, e.g. `Fable`.
    pub label: String,
    pub used_pct: f64,
    /// Unix seconds; 0 when unknown.
    pub resets_at: i64,
    /// The server's rating: `normal`, `warning` or `critical`.
    pub severity: String,
    /// When `/usage` was read, in ms.
    pub read_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Session {
    pub host: String,
    pub id: String,
    /// Background session id as shown by `claude agents`.
    pub short_id: Option<String>,
    pub kind: Kind,
    /// Session that started this one (a fork, or `claude --bg` run by Claude).
    pub parent: Option<String>,
    pub pid: Option<u32>,
    pub cwd: String,
    /// Configured workspace holding `cwd`.
    pub workspace: Option<String>,
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
    /// Renamed in ccdash; Claude Code adopts the title at the next prompt.
    pub renaming: bool,
    /// The session runs the ccdash channel, so it takes replies.
    pub channel: bool,
    /// Open prompts ccdash can answer, oldest first.
    pub asks: Vec<Ask>,
    /// Replies sent through the channel that Claude has not picked up yet.
    pub outbox: Vec<Stamped>,
    /// Messages waiting for the session's next prompt (text shortened).
    #[serde(default)]
    pub inbox: Vec<Message>,
    /// What the session says it is working on.
    #[serde(default)]
    pub claim: Option<Claim>,
    pub resources: Option<Resources>,
    pub turn: Option<Turn>,
    pub tasks: Vec<Task>,
    pub subagents: Vec<Subagent>,
    pub started_at: i64,
    pub last_event_at: i64,
}

/// A message from one session, or from the owner in the panel, to a
/// session: handed to the recipient as context at its next prompt, never
/// as a turn of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Message {
    pub id: i64,
    pub workspace: String,
    /// None when the owner sent it from the panel.
    pub from_session: Option<String>,
    pub from_host: Option<String>,
    /// The sender's title or id prefix when it was sent; `the owner` from
    /// the panel.
    pub from_label: String,
    pub to_session: String,
    pub to_host: Option<String>,
    pub text: String,
    pub sent_at: i64,
    /// Injected into the recipient's prompt.
    pub delivered_at: Option<i64>,
    /// The recipient's hook took the injection, so Claude read it.
    pub read_at: Option<i64>,
}

/// What a session says it is working on, so the others keep clear.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Claim {
    pub what: String,
    /// Absolute files or directories the claim covers.
    pub paths: Vec<String>,
    pub since: i64,
    /// When it lapses by itself; None until released or the session ends.
    pub until: Option<i64>,
}

/// A machine whose collector reports to the hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct HostStatus {
    pub name: String,
    pub connected: bool,
    /// When the link last came up or went down.
    pub since: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Snapshot {
    /// The machine answering: the hub, or a collector showing its mirror.
    pub host: String,
    pub now: i64,
    /// Configured workspace names, for filtering.
    pub workspaces: Vec<String>,
    /// Machines with sessions, for filtering.
    #[serde(default)]
    pub hosts: Vec<HostStatus>,
    pub sessions: Vec<Session>,
    pub rate_limits: Option<RateLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Update {
    Session {
        session: Box<Session>,
    },
    Removed {
        host: String,
        id: String,
    },
    RateLimits {
        rate_limits: RateLimits,
    },
    Hosts {
        hosts: Vec<HostStatus>,
    },
    /// Notes of `workspace` changed; the panel reloads what it shows.
    Notes {
        workspace: String,
    },
}
