//! Session state. Every input is applied here; I/O lives in the callers.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ask::Answer,
    model::{
        Activity, Ask, AskKind, Claim, Kind, Limit, Message, PARKED_AFTER_MS, ProcInfo, RateLimits,
        Resources, ScopedLimit, Session, Stamped, Subagent, Task, TaskStatus, ToolRun, Turn,
        WindowRef,
    },
    util::{tail, truncate},
    wire::{Envelope, Source},
    workspace::Workspaces,
};

const GONE_RETENTION_MS: i64 = 10 * 60 * 1000;
/// Hooks are authoritative; `claude agents` only corrects state after this much hook silence.
const HOOK_QUIET_MS: i64 = 10_000;
const PENDING_WINDOW_MS: i64 = 30_000;
const SAMPLE_EVERY_MS: i64 = 60_000;
const SAMPLE_MIN_GAP_MS: i64 = 5_000;
const SUMMARY_LEN: usize = 160;
/// Bytes of a Bash command kept with its event, for the Shell view.
const COMMAND_LEN: usize = 4096;
const DEFAULT_TOOL_OUTPUT: usize = 8192;
const SUBAGENT_TEXT_LEN: usize = 4096;
const COMPACT_SUMMARY_LEN: usize = 8192;
const MAX_SUBAGENTS: usize = 32;
const MAX_PENDING_AGENTS: usize = 8;
const MAX_TASKS: usize = 100;
const MAX_TASK_FORMS: usize = 64;
/// Prompt and reply text kept on the live session; full text lives in events.
pub const PREVIEW_LEN: usize = 1200;
const MESSAGE_TAIL: usize = 800;
const MESSAGE_MAX: usize = 32 * 1024;
const MAX_STREAMS: usize = 8;
const PROC_SAMPLE_EVERY_MS: i64 = 60_000;
/// A session starting this soon after Claude ran `claude --bg` is its child.
const SPAWN_LINK_MS: i64 = 60_000;
const MAX_OUTBOX: usize = 10;
const MAX_ASKS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub id: u64,
    pub app_id: String,
    pub title: String,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum NiriEvent {
    Windows(Vec<Window>),
    WindowChanged(Window),
    WindowClosed(u64),
    Focus(Option<u64>),
}

/// One entry of `claude agents --json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    pub session_id: String,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub started_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StoreMsg {
    Event {
        ts: i64,
        session: String,
        kind: &'static str,
        data: Value,
    },
    Sample {
        ts: i64,
        session: String,
        model: Option<String>,
        context_pct: Option<f64>,
        cost_usd: Option<f64>,
        lines_added: Option<u64>,
        lines_removed: Option<u64>,
        five_hour: Option<f64>,
        seven_day: Option<f64>,
    },
    /// Process tree usage, averaged (CPU) or peaked (memory) over a minute.
    ProcSample {
        ts: i64,
        session: String,
        cpu_pct: f64,
        mem_bytes: u64,
        procs: u32,
    },
    Session {
        id: String,
        cwd: String,
        name: Option<String>,
        title: Option<String>,
        kind: Kind,
        started_at: i64,
        ended_at: Option<i64>,
    },
    /// A scoped limit read from `/usage`.
    Limit { ts: i64, limit: ScopedLimit },
    /// The session's claim changed; None drops it.
    Claim {
        session: String,
        claim: Option<Claim>,
    },
    /// One OpenTelemetry metric point or event Claude Code exported.
    Telemetry {
        ts: i64,
        session: String,
        name: String,
        data: Value,
    },
}

/// What an input changed; the hub turns this into broadcasts and writes.
#[derive(Debug, Default)]
pub struct Effects {
    pub touched: BTreeSet<String>,
    pub created: Vec<String>,
    pub removed: Vec<String>,
    pub store: Vec<StoreMsg>,
    pub rate_limits: bool,
}

/// Persisted fields used to refill a session first seen after a daemon restart.
#[derive(Debug, Default)]
pub struct Restored {
    pub prompt: Option<Stamped>,
    pub reply: Option<Stamped>,
    pub title: Option<String>,
    /// When the session first appeared, so a hub restart keeps its start.
    pub started_at: Option<i64>,
    pub hooked: bool,
    pub turn: Option<Turn>,
    pub tasks: Vec<Task>,
    pub inbox: Vec<Message>,
    pub claim: Option<Claim>,
}

#[derive(Debug, Default)]
struct Aux {
    last_hook_at: i64,
    kind_known: bool,
    missing_polls: u8,
    pending_window: Option<(u64, i64)>,
    last_sample: Option<(i64, Option<f64>)>,
    /// `agent_id` of the pending permission request (None = main thread).
    permission_agent: Option<String>,
    /// When each agent (None = main thread) last got a tool result: a
    /// permission prompt from before then is already answered.
    resolved_at: HashMap<Option<String>, i64>,
    /// `(subagent_type, description)` of Agent calls awaiting SubagentStart.
    pending_agents: Vec<(String, Option<String>)>,
    /// TaskCreate `activeForm` by subject, until the task id is known.
    task_forms: HashMap<String, String>,
    /// Text of assistant messages still streaming, by `message_id`.
    streams: HashMap<String, String>,
    /// Title from `ccdash rename` that Claude Code has not adopted yet.
    pending_title: Option<String>,
    proc_window: Option<ProcWindow>,
}

/// Process samples collected since the last stored one.
#[derive(Debug)]
struct ProcWindow {
    since: i64,
    cpu_sum: f64,
    polls: u32,
    mem_max: u64,
    procs_max: u32,
}

/// One poll of a session's process tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcPoll {
    pub session: String,
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub procs: u32,
    pub top: Vec<ProcInfo>,
    /// Session id this process was forked from (`--fork-session --resume`).
    pub forked_from: Option<String>,
}

pub struct State {
    host: String,
    terminals: Vec<String>,
    workspaces: Workspaces,
    /// Bytes of a tool's output kept with its event (`store.tool_output_bytes`).
    tool_output_bytes: usize,
    sessions: HashMap<String, Session>,
    aux: HashMap<String, Aux>,
    rate_limits: Option<RateLimits>,
    windows: HashMap<u64, Window>,
    focused: Option<u64>,
    /// `(session, ts)` of Bash calls that started a background session.
    spawns: Vec<(String, i64)>,
    /// Claude processes connected through `ccdash channel`.
    channel_pids: HashSet<u32>,
    /// Pids the collector reported as no longer running.
    dead: HashSet<u32>,
}

impl State {
    pub fn new(host: String, terminals: Vec<String>, workspaces: Workspaces) -> Self {
        Self {
            host,
            terminals,
            workspaces,
            tool_output_bytes: DEFAULT_TOOL_OUTPUT,
            sessions: HashMap::new(),
            aux: HashMap::new(),
            rate_limits: None,
            windows: HashMap::new(),
            focused: None,
            spawns: Vec::new(),
            channel_pids: HashSet::new(),
            dead: HashSet::new(),
        }
    }

    /// Caps the output stored with tool events; 0 keeps none.
    pub fn with_tool_output_bytes(mut self, bytes: usize) -> Self {
        self.tool_output_bytes = bytes;
        self
    }

    /// Takes the collector's word on which known pids have exited; the
    /// hub has no `/proc` of that machine.
    pub fn report_dead(&mut self, dead: &[u32]) {
        self.dead = dead.iter().copied().collect();
    }

    fn alive(&self, pid: Option<u32>) -> bool {
        pid.is_some_and(|pid| !self.dead.contains(&pid))
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
    }

    pub fn rate_limits(&self) -> Option<&RateLimits> {
        self.rate_limits.as_ref()
    }

    /// Finds a session by full id, background short id, or unique id prefix.
    pub fn resolve(&self, key: &str) -> Option<&Session> {
        if let Some(s) = self.sessions.get(key) {
            return Some(s);
        }
        if let Some(s) = self
            .sessions
            .values()
            .find(|s| s.short_id.as_deref() == Some(key))
        {
            return Some(s);
        }
        let mut hits = self.sessions.values().filter(|s| s.id.starts_with(key));
        match (hits.next(), hits.next()) {
            (Some(s), None) => Some(s),
            _ => None,
        }
    }

    pub fn restore(&mut self, id: &str, saved: Restored) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.prompt = s.prompt.take().or(saved.prompt);
            s.reply = s.reply.take().or(saved.reply);
            s.title = s.title.take().or(saved.title);
            if let Some(t) = saved.started_at {
                s.started_at = s.started_at.min(t);
            }
            s.hooked |= saved.hooked;
            s.turn = s.turn.take().or(saved.turn);
            if s.tasks.is_empty() {
                s.tasks = saved.tasks;
            }
            if s.inbox.is_empty() {
                s.inbox = saved.inbox;
            }
            s.claim = s.claim.take().or(saved.claim);
        }
    }

    /// Shows the messages waiting for the session's next prompt; true
    /// when that changed anything.
    pub fn set_inbox(&mut self, id: &str, inbox: Vec<Message>, fx: &mut Effects) -> bool {
        let Some(s) = self.sessions.get_mut(id) else {
            return false;
        };
        if s.inbox == inbox {
            return false;
        }
        s.inbox = inbox;
        fx.touched.insert(id.to_owned());
        true
    }

    /// Sets or clears what the session says it is working on.
    pub fn set_claim(&mut self, id: &str, claim: Option<Claim>, fx: &mut Effects) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if s.claim == claim {
            return;
        }
        s.claim = claim.clone();
        fx.store.push(StoreMsg::Claim {
            session: id.to_owned(),
            claim,
        });
        fx.touched.insert(id.to_owned());
    }

    pub fn apply(&mut self, env: &Envelope, fx: &mut Effects) {
        match env.source {
            Source::Hook => self.hook(env, fx),
            Source::Statusline => self.statusline(env, fx),
        }
    }

    fn hook(&mut self, env: &Envelope, fx: &mut Effects) {
        let p = &env.payload;
        let (Some(id), Some(event)) = (str_of(p, "session_id"), str_of(p, "hook_event_name"))
        else {
            return;
        };
        let ts = env.ts;
        if event == "SessionEnd" {
            self.mark_gone(id, ts, str_of(p, "reason").unwrap_or("ended"), fx);
            return;
        }
        self.ensure(id, str_of(p, "cwd"), ts, ts, fx);

        let store = &mut fx.store;
        let aux = self.aux.entry(id.to_owned()).or_default();
        aux.last_hook_at = ts;
        let s = self.sessions.get_mut(id).expect("ensured above");
        s.hooked = true;
        s.last_event_at = ts;
        if let Some(pid) = env.pid {
            s.pid = Some(pid);
            sync_channel(s, &self.channel_pids);
        }
        if let Some(remote) = &env.remote_id {
            s.remote_url = Some(remote_url(remote));
        }
        if let Some(cwd) = str_of(p, "cwd")
            && cwd != s.cwd
        {
            s.cwd = cwd.to_owned();
            (s.workspace, s.project) = match self.workspaces.locate(cwd) {
                Some((ws, project)) => (Some(ws), project),
                None => (None, None),
            };
        }
        if let Some(mode) = str_of(p, "permission_mode") {
            s.permission_mode = Some(mode.to_owned());
        }
        if let Some(effort) = p.get("effort").and_then(effort_of) {
            s.effort = Some(effort);
        }
        let mut title_changed = false;
        if let Some(applied) = &env.applied_title {
            aux.pending_title = None;
            s.renaming = false;
            if s.title.as_ref() != Some(applied) {
                s.title = Some(applied.clone());
                store.push(session_row(s, None));
                title_changed = true;
            }
        }
        if s.activity == Activity::Gone {
            set_activity(s, Activity::Idle, ts, store);
        }

        let tool_name = str_of(p, "tool_name").unwrap_or("?");
        let tool_input = p.get("tool_input").unwrap_or(&Value::Null);
        // Only set inside a subagent (and on Subagent* events, as the subagent's own id).
        let agent_id = str_of(p, "agent_id");
        let mut focus_candidate = false;
        match event {
            "SessionStart" => {
                let source = str_of(p, "source").unwrap_or("startup");
                if s.model.is_none() {
                    s.model = str_of(p, "model").map(str::to_owned);
                }
                if source == "clear" {
                    s.turn = None;
                    s.tasks.clear();
                }
                if s.parent.is_none() && matches!(source, "startup" | "fork") {
                    self.spawns.retain(|(_, at)| ts - at <= SPAWN_LINK_MS);
                    if let Some(i) = self.spawns.iter().position(|(parent, _)| parent != id) {
                        s.parent = Some(self.spawns.remove(i).0);
                    }
                }
                record(store, ts, id, "start", json!({ "source": source }));
                focus_candidate = matches!(source, "startup" | "resume");
            }
            "UserPromptSubmit" => {
                let raw = str_of(p, "prompt").unwrap_or_default();
                let relayed = channel_text(raw);
                let text = relayed.as_deref().unwrap_or(raw);
                if relayed.is_some() {
                    s.outbox.clear();
                }
                s.prompt = Some(Stamped {
                    at: ts,
                    text: truncate(text, PREVIEW_LEN),
                });
                s.message = None;
                s.tool = None;
                s.waiting_for = None;
                s.asks.clear();
                s.turn = Some(Turn {
                    started_at: ts,
                    ..Turn::default()
                });
                if let Some(title) = str_of(p, "session_title").filter(|t| !t.is_empty())
                    && aux.pending_title.is_none()
                    && s.title.as_deref() != Some(title)
                {
                    s.title = Some(title.to_owned());
                    store.push(session_row(s, None));
                    title_changed = true;
                }
                set_activity(s, Activity::Working, ts, store);
                let data = match relayed {
                    Some(_) => json!({ "text": text, "via": "ccdash" }),
                    None => json!({ "text": text }),
                };
                record(store, ts, id, "prompt", data);
                focus_candidate = true;
            }
            "PreToolUse" => {
                let run = ToolRun {
                    name: tool_name.to_owned(),
                    summary: tool_summary(tool_name, tool_input, &s.cwd),
                    started_at: ts,
                };
                if tool_name == "TaskCreate"
                    && let (Some(subject), Some(form)) = (
                        str_of(tool_input, "subject"),
                        str_of(tool_input, "activeForm"),
                    )
                {
                    aux.task_forms.insert(subject.to_owned(), form.to_owned());
                }
                if let Some(agent) = agent_id {
                    if let Some(sub) = s.subagents.iter_mut().find(|a| a.id == agent) {
                        sub.tool = Some(run);
                    }
                } else {
                    if tool_name == "Bash"
                        && str_of(tool_input, "command").is_some_and(starts_background_session)
                    {
                        self.spawns.push((id.to_owned(), ts));
                    }
                    if matches!(tool_name, "Agent" | "Task") {
                        aux.pending_agents.push((
                            str_of(tool_input, "subagent_type")
                                .unwrap_or_default()
                                .to_owned(),
                            str_of(tool_input, "description").map(str::to_owned),
                        ));
                        let excess = aux.pending_agents.len().saturating_sub(MAX_PENDING_AGENTS);
                        aux.pending_agents.drain(..excess);
                    }
                    s.tool = Some(run);
                    if s.activity != Activity::Permission {
                        set_activity(s, Activity::Working, ts, store);
                    }
                }
            }
            "PermissionRequest" => {
                let summary = tool_summary(tool_name, tool_input, &s.cwd);
                // NOTE: `ccdash ask` sends this over api.sock, so it can trail the
                // tool result that answered it
                let answered = aux
                    .resolved_at
                    .get(&agent_id.map(str::to_owned))
                    .is_some_and(|at| *at > ts);
                if !answered {
                    s.waiting_for = Some(if summary.is_empty() {
                        tool_name.to_owned()
                    } else {
                        format!("{tool_name}: {summary}")
                    });
                    aux.permission_agent = agent_id.map(str::to_owned);
                    set_activity(s, Activity::Permission, ts, store);
                }
                record(
                    store,
                    ts,
                    id,
                    "permission",
                    json!({ "tool": tool_name, "summary": summary }),
                );
            }
            "Notification" => {
                let kind = str_of(p, "notification_type").unwrap_or_default();
                match kind {
                    "permission_prompt" if s.activity != Activity::Permission => {
                        s.waiting_for = str_of(p, "message").map(str::to_owned);
                        set_activity(s, Activity::Permission, ts, store);
                    }
                    "idle_prompt" if s.activity == Activity::Working && s.tool.is_none() => {
                        set_activity(s, Activity::YourTurn, ts, store);
                    }
                    _ => {}
                }
                record(
                    store,
                    ts,
                    id,
                    "notification",
                    json!({ "type": kind, "message": str_of(p, "message") }),
                );
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let ok = event == "PostToolUse";
                aux.resolved_at.insert(agent_id.map(str::to_owned), ts);
                close_asks(s, agent_id, Some(tool_name));
                if ok {
                    for delta in task_deltas(&s.tasks, tool_name, tool_input, p, aux) {
                        apply_and_record_task(s, &delta, ts, store);
                    }
                }
                if let Some(agent) = agent_id {
                    if let Some(sub) = s.subagents.iter_mut().find(|a| a.id == agent) {
                        sub.tools += 1;
                        sub.tool = None;
                    }
                    if let Some(turn) = &mut s.turn {
                        turn.subagent_tools += 1;
                    }
                } else {
                    let running = s.tool.take_if(|t| t.name == tool_name);
                    let ms = p
                        .get("duration_ms")
                        .and_then(Value::as_i64)
                        .or_else(|| running.as_ref().map(|t| ts - t.started_at));
                    let summary = running.map_or_else(
                        || tool_summary(tool_name, tool_input, &s.cwd),
                        |t| t.summary,
                    );
                    if let Some(turn) = &mut s.turn {
                        turn.note_tool(tool_name, &summary, ok);
                    }
                    let mut data =
                        json!({ "name": tool_name, "summary": summary, "ms": ms, "ok": ok });
                    if tool_name == "Bash"
                        && let Some(command) = str_of(tool_input, "command")
                    {
                        data["command"] = truncate(command, COMMAND_LEN).into();
                    }
                    if let Some(output) = tool_output(ok, p, self.tool_output_bytes) {
                        data["output"] = output.into();
                    }
                    record(store, ts, id, "tool", data);
                }
                if s.activity == Activity::Permission && aux.permission_agent.as_deref() == agent_id
                {
                    end_permission(s, &mut aux.permission_agent, ts, store);
                }
            }
            "PermissionDenied" => {
                aux.resolved_at.insert(agent_id.map(str::to_owned), ts);
                close_asks(s, agent_id, Some(tool_name));
                record(
                    store,
                    ts,
                    id,
                    "denied",
                    json!({
                        "tool": tool_name,
                        "summary": tool_summary(tool_name, tool_input, &s.cwd),
                        "reason": str_of(p, "reason"),
                    }),
                );
                if agent_id.is_none() {
                    s.tool = None;
                }
                if agent_id.is_none()
                    || (s.activity == Activity::Permission
                        && aux.permission_agent.as_deref() == agent_id)
                {
                    end_permission(s, &mut aux.permission_agent, ts, store);
                }
            }
            "Stop" => {
                let text = str_of(p, "last_assistant_message").unwrap_or_default();
                if !text.is_empty() {
                    s.reply = Some(Stamped {
                        at: ts,
                        text: truncate(text, PREVIEW_LEN),
                    });
                }
                s.tool = None;
                s.waiting_for = None;
                if let Some(turn) = &mut s.turn {
                    turn.ended_at = Some(ts);
                }
                set_activity(s, Activity::YourTurn, ts, store);
                record(store, ts, id, "reply", json!({ "text": text }));
            }
            "StopFailure" => {
                let error = str_of(p, "error")
                    .or_else(|| str_of(p, "message"))
                    .unwrap_or("turn failed");
                s.tool = None;
                s.waiting_for = Some(format!("error: {error}"));
                if let Some(turn) = &mut s.turn {
                    turn.ended_at = Some(ts);
                }
                set_activity(s, Activity::YourTurn, ts, store);
                record(store, ts, id, "error", json!({ "error": error }));
            }
            "SubagentStart" => {
                let agent_type = str_of(p, "agent_type").unwrap_or_default();
                let description = take_pending_agent(&mut aux.pending_agents, agent_type);
                if let Some(agent) = agent_id
                    && s.subagents.len() < MAX_SUBAGENTS
                    && !s.subagents.iter().any(|a| a.id == agent)
                {
                    s.subagents.push(Subagent {
                        id: agent.to_owned(),
                        agent_type: agent_type.to_owned(),
                        description,
                        started_at: ts,
                        tools: 0,
                        tool: None,
                    });
                }
            }
            "SubagentStop" => {
                close_asks(s, agent_id, None);
                let done = s
                    .subagents
                    .iter()
                    .position(|a| Some(a.id.as_str()) == agent_id)
                    .map(|i| s.subagents.remove(i));
                let text = str_of(p, "last_assistant_message").unwrap_or_default();
                record(
                    store,
                    ts,
                    id,
                    "subagent",
                    json!({
                        "agent_type": done.as_ref().map_or_else(
                            || str_of(p, "agent_type").unwrap_or_default(),
                            |a| a.agent_type.as_str(),
                        ),
                        "description": done.as_ref().and_then(|a| a.description.as_deref()),
                        "ms": done.as_ref().map(|a| ts - a.started_at),
                        "tools": done.as_ref().map(|a| a.tools),
                        "text": truncate(text, SUBAGENT_TEXT_LEN),
                    }),
                );
            }
            "TaskCreated" | "TaskCompleted" => {
                if let (Some(task), Some(subject)) =
                    (str_of(p, "task_id"), str_of(p, "task_subject"))
                {
                    let mut delta = json!({ "id": task, "subject": subject });
                    if let Some(form) = aux.task_forms.remove(subject) {
                        delta["active_form"] = form.into();
                    }
                    if event == "TaskCompleted" {
                        delta["status"] = "completed".into();
                    }
                    apply_and_record_task(s, &delta, ts, store);
                }
            }
            "MessageDisplay" => {
                if agent_id.is_none()
                    && let Some(message) = str_of(p, "message_id")
                {
                    let text = aux.streams.entry(message.to_owned()).or_default();
                    if text.len() < MESSAGE_MAX {
                        text.push_str(str_of(p, "delta").unwrap_or_default());
                    }
                    if !text.trim().is_empty() {
                        s.message = Some(Stamped {
                            at: ts,
                            text: tail(text.trim(), MESSAGE_TAIL),
                        });
                    }
                    if p.get("final").and_then(Value::as_bool) == Some(true) {
                        let text = aux.streams.remove(message).unwrap_or_default();
                        if !text.trim().is_empty() {
                            record(
                                store,
                                ts,
                                id,
                                "message",
                                json!({ "text": truncate(text.trim(), MESSAGE_MAX) }),
                            );
                        }
                    }
                    if aux.streams.len() > MAX_STREAMS {
                        aux.streams.clear();
                    }
                }
            }
            "PostCompact" => {
                let summary = str_of(p, "compact_summary").unwrap_or_default();
                record(
                    store,
                    ts,
                    id,
                    "compact",
                    json!({
                        "trigger": str_of(p, "trigger"),
                        "summary": truncate(summary, COMPACT_SUMMARY_LEN),
                    }),
                );
            }
            _ => {}
        }
        if title_changed {
            self.match_titles(fx);
        }
        if focus_candidate {
            self.note_focus_candidate(id, ts, fx);
        }
    }

    fn statusline(&mut self, env: &Envelope, fx: &mut Effects) {
        let p = &env.payload;
        let Some(id) = str_of(p, "session_id") else {
            return;
        };
        let ts = env.ts;
        self.ensure(id, str_of(p, "cwd"), ts, ts, fx);

        let s = self.sessions.get_mut(id).expect("ensured above");
        let aux = self.aux.entry(id.to_owned()).or_default();
        s.hooked = true;
        if let Some(pid) = env.pid {
            s.pid = Some(pid);
            sync_channel(s, &self.channel_pids);
        }
        if let Some(remote) = &env.remote_id {
            s.remote_url = Some(remote_url(remote));
        }
        let mut title = str_of(p, "session_name")
            .filter(|t| !t.is_empty())
            .map(str::to_owned);
        if let Some(pending) = &aux.pending_title {
            // NOTE: Claude reports its old title until a hook hands it the new one
            if title.as_ref() == Some(pending) {
                aux.pending_title = None;
                s.renaming = false;
                fx.touched.insert(id.to_owned());
            } else {
                title = None;
            }
        }
        let title_changed = title.is_some() && title != s.title;
        if title_changed {
            s.title = title;
            fx.store.push(session_row(s, None));
        }
        if let Some(model) = p.pointer("/model/display_name").and_then(Value::as_str) {
            s.model = Some(model.to_owned());
        }
        s.context_pct = p
            .pointer("/context_window/used_percentage")
            .and_then(Value::as_f64);
        s.cost_usd = p.pointer("/cost/total_cost_usd").and_then(Value::as_f64);
        s.lines_added = p.pointer("/cost/total_lines_added").and_then(Value::as_u64);
        s.lines_removed = p
            .pointer("/cost/total_lines_removed")
            .and_then(Value::as_u64);
        let (context_pct, cost_usd, model) = (s.context_pct, s.cost_usd, s.model.clone());
        let (lines_added, lines_removed) = (s.lines_added, s.lines_removed);

        if let Some(mut limits) = parse_rate_limits(p, ts) {
            let old = self.rate_limits.take().unwrap_or_default();
            fx.rate_limits |=
                old.five_hour != limits.five_hour || old.seven_day != limits.seven_day;
            limits.scoped = old.scoped;
            self.rate_limits = Some(limits);
        }

        let aux = self.aux.entry(id.to_owned()).or_default();
        let due = aux.last_sample.is_none_or(|(at, ctx)| {
            ts - at >= SAMPLE_EVERY_MS || (ctx != context_pct && ts - at >= SAMPLE_MIN_GAP_MS)
        });
        if due {
            aux.last_sample = Some((ts, context_pct));
            let limits = self.rate_limits.as_ref();
            fx.store.push(StoreMsg::Sample {
                ts,
                session: id.to_owned(),
                model,
                context_pct,
                cost_usd,
                lines_added,
                lines_removed,
                five_hour: limits
                    .and_then(|l| l.five_hour.as_ref())
                    .map(|l| l.used_pct),
                seven_day: limits
                    .and_then(|l| l.seven_day.as_ref())
                    .map(|l| l.used_pct),
            });
        }
        if title_changed {
            self.match_titles(fx);
        }
    }

    /// Applies one process poll; sessions missing from it lose their numbers.
    pub fn resources(&mut self, polls: Vec<ProcPoll>, now: i64, fx: &mut Effects) {
        let mut polled = HashSet::new();
        for poll in polls {
            let Some(s) = self.sessions.get_mut(&poll.session) else {
                continue;
            };
            polled.insert(poll.session.clone());
            let resources = Resources {
                cpu_pct: poll.cpu_pct.round(),
                mem_bytes: round_mib(poll.mem_bytes),
                procs: poll.procs,
                top: poll
                    .top
                    .into_iter()
                    .map(|p| ProcInfo {
                        cpu_pct: p.cpu_pct.round(),
                        mem_bytes: round_mib(p.mem_bytes),
                        ..p
                    })
                    .collect(),
            };
            if s.resources.as_ref() != Some(&resources) {
                s.resources = Some(resources);
                fx.touched.insert(poll.session.clone());
            }
            if s.kind == Kind::Background
                && poll.forked_from.is_some()
                && s.parent != poll.forked_from
            {
                s.parent = poll.forked_from.clone();
                fx.touched.insert(poll.session.clone());
            }
            let aux = self.aux.entry(poll.session.clone()).or_default();
            let window = aux.proc_window.get_or_insert(ProcWindow {
                since: now,
                cpu_sum: 0.0,
                polls: 0,
                mem_max: 0,
                procs_max: 0,
            });
            window.cpu_sum += poll.cpu_pct;
            window.polls += 1;
            window.mem_max = window.mem_max.max(poll.mem_bytes);
            window.procs_max = window.procs_max.max(poll.procs);
            if now - window.since >= PROC_SAMPLE_EVERY_MS {
                fx.store.push(StoreMsg::ProcSample {
                    ts: now,
                    session: poll.session,
                    cpu_pct: window.cpu_sum / f64::from(window.polls),
                    mem_bytes: window.mem_max,
                    procs: window.procs_max,
                });
                aux.proc_window = None;
            }
        }
        for s in self.sessions.values_mut() {
            if !polled.contains(&s.id) && s.resources.take().is_some() {
                fx.touched.insert(s.id.clone());
            }
        }
    }

    /// Shows `title` at once and remembers it until Claude Code adopts it
    /// (the caller queues it for the session's next prompt). Returns the
    /// session id.
    pub fn rename(&mut self, key: &str, title: &str, fx: &mut Effects) -> Result<String, String> {
        let id = match self.resolve(key) {
            Some(s) if s.activity == Activity::Gone => return Err("the session has ended".into()),
            Some(s) => s.id.clone(),
            None => return Err(format!("no session matches {key:?}")),
        };
        self.mark_pending(&id, title, fx);
        Ok(id)
    }

    /// Marks `title` as waiting for Claude Code to adopt it.
    pub fn mark_pending(&mut self, id: &str, title: &str, fx: &mut Effects) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        s.title = Some(title.to_owned());
        s.renaming = true;
        fx.store.push(session_row(s, None));
        fx.touched.insert(id.to_owned());
        self.aux.entry(id.to_owned()).or_default().pending_title = Some(title.to_owned());
    }

    /// Marks the session run by `pid` as connected to (or cut off from) its
    /// ccdash channel.
    pub fn channel(&mut self, pid: u32, up: bool, fx: &mut Effects) {
        if up {
            self.channel_pids.insert(pid);
        } else {
            self.channel_pids.remove(&pid);
        }
        for s in self.sessions.values_mut().filter(|s| s.pid == Some(pid)) {
            if sync_channel(s, &self.channel_pids) {
                fx.touched.insert(s.id.clone());
            }
        }
    }

    /// Shows a reply sent through the channel until Claude picks it up.
    pub fn sent_reply(&mut self, id: &str, text: &str, now: i64, fx: &mut Effects) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        s.outbox.push(Stamped {
            at: now,
            text: truncate(text, PREVIEW_LEN),
        });
        let excess = s.outbox.len().saturating_sub(MAX_OUTBOX);
        s.outbox.drain(..excess);
        fx.touched.insert(id.to_owned());
    }

    /// Shows a prompt `ccdash ask` holds open; false if the session is
    /// unknown or ended, or a tool result already answered the prompt.
    pub fn add_ask(&mut self, session: &str, ask: Ask, fx: &mut Effects) -> bool {
        let Some(s) = self
            .sessions
            .get_mut(session)
            .filter(|s| s.activity != Activity::Gone)
        else {
            return false;
        };
        let answered = self
            .aux
            .get(session)
            .and_then(|aux| aux.resolved_at.get(&ask.agent_id))
            .is_some_and(|at| *at > ask.at);
        if answered {
            return false;
        }
        s.asks.push(ask);
        let excess = s.asks.len().saturating_sub(MAX_ASKS);
        s.asks.drain(..excess);
        fx.touched.insert(session.to_owned());
        true
    }

    pub fn ask(&self, session: &str, id: &str) -> Option<&Ask> {
        self.sessions.get(session)?.asks.iter().find(|a| a.id == id)
    }

    /// Takes the ask `id` off its session, e.g. once its hook went away.
    pub fn remove_ask(&mut self, session: &str, id: &str, fx: &mut Effects) -> Option<Ask> {
        let s = self.sessions.get_mut(session)?;
        let i = s.asks.iter().position(|a| a.id == id)?;
        fx.touched.insert(session.to_owned());
        Some(s.asks.remove(i))
    }

    /// Closes an ask ccdash answered and records how. An answer that
    /// stops Claude ends the turn: no hook reports an interrupt.
    pub fn answered(
        &mut self,
        session: &str,
        id: &str,
        answer: &Answer,
        via: &str,
        now: i64,
        fx: &mut Effects,
    ) {
        let Some(ask) = self.remove_ask(session, id, fx) else {
            return;
        };
        let store = &mut fx.store;
        record(
            store,
            now,
            session,
            "answered",
            json!({ "tool": ask_tool(&ask.kind), "action": answer.name(), "via": via }),
        );
        if answer.interrupts()
            && let Some(s) = self.sessions.get_mut(session)
        {
            s.tool = None;
            s.waiting_for = None;
            if let Some(turn) = &mut s.turn {
                turn.ended_at = Some(now);
            }
            set_activity(s, Activity::YourTurn, now, store);
        }
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    /// The niri window id with keyboard focus.
    pub fn focused(&self) -> Option<u64> {
        self.focused
    }

    pub fn agents(&mut self, list: &[AgentEntry], now: i64, fx: &mut Effects) {
        let mut seen = HashSet::new();
        for e in list {
            let id = e.session_id.as_str();
            seen.insert(id);
            self.ensure(id, e.cwd.as_deref(), e.started_at.unwrap_or(now), now, fx);
            let aux = self.aux.entry(id.to_owned()).or_default();
            aux.kind_known = true;
            aux.missing_polls = 0;
            let quiet = now - aux.last_hook_at > HOOK_QUIET_MS;

            let s = self.sessions.get_mut(id).expect("ensured above");
            s.pid = e.pid.or(s.pid);
            sync_channel(s, &self.channel_pids);
            let kind = match e.kind.as_deref() {
                Some("background") => Kind::Background,
                _ => Kind::Interactive,
            };
            if kind != s.kind {
                s.kind = kind;
                fx.store.push(session_row(s, None));
            }
            if kind == Kind::Interactive {
                s.parent = None;
            }
            s.short_id = e.id.clone();
            if e.name.is_some() {
                s.name = e.name.clone();
            }
            // NOTE: an open ask means a live hook holds the prompt, whatever the poll says
            if quiet
                && s.asks.is_empty()
                && let Some(to) = agent_activity(e, s)
            {
                if to == Activity::Permission {
                    s.waiting_for = e.waiting_for.clone();
                } else if s.activity == Activity::Permission {
                    s.waiting_for = None;
                }
                set_activity(s, to, now, &mut fx.store);
            }
        }
        for e in list {
            self.resolve_pending(&e.session_id, now, fx);
        }

        let mut vanished = Vec::new();
        for (id, s) in &self.sessions {
            if s.activity == Activity::Gone || seen.contains(id.as_str()) {
                continue;
            }
            let aux = self.aux.entry(id.clone()).or_default();
            aux.missing_polls = aux.missing_polls.saturating_add(1);
            if aux.missing_polls >= 2 && !self.alive(s.pid) {
                vanished.push(id.clone());
            }
        }
        for id in vanished {
            self.mark_gone(&id, now, "vanished", fx);
        }
    }

    pub fn niri(&mut self, event: NiriEvent, fx: &mut Effects) {
        match event {
            NiriEvent::Windows(list) => {
                self.focused = list.iter().find(|w| w.focused).map(|w| w.id);
                self.windows = list.into_iter().map(|w| (w.id, w)).collect();
                for s in self.sessions.values_mut() {
                    let Some(r) = &mut s.window else { continue };
                    match self.windows.get(&r.id) {
                        None => {
                            s.window = None;
                            fx.touched.insert(s.id.clone());
                        }
                        Some(w) if shown_title(&w.title) != r.title => {
                            r.title = shown_title(&w.title).to_owned();
                            fx.touched.insert(s.id.clone());
                        }
                        Some(_) => {}
                    }
                }
            }
            NiriEvent::WindowChanged(w) => {
                if w.focused {
                    self.focused = Some(w.id);
                }
                let (wid, title) = (w.id, shown_title(&w.title).to_owned());
                self.windows.insert(w.id, w);
                if let Some(owner) = self.owner_of(wid)
                    && let Some(r) = self
                        .sessions
                        .get_mut(&owner)
                        .and_then(|s| s.window.as_mut())
                    && r.title != title
                {
                    r.title = title;
                    fx.touched.insert(owner);
                }
            }
            NiriEvent::WindowClosed(wid) => {
                self.windows.remove(&wid);
                if self.focused == Some(wid) {
                    self.focused = None;
                }
                if let Some(owner) = self.owner_of(wid)
                    && let Some(s) = self.sessions.get_mut(&owner)
                {
                    s.window = None;
                    fx.touched.insert(owner);
                }
                return;
            }
            NiriEvent::Focus(id) => {
                self.focused = id;
                return;
            }
        }
        self.match_titles(fx);
    }

    /// Takes the scoped limits (Fable's, say) a `/usage` poll read.
    pub fn usage(&mut self, scoped: Vec<ScopedLimit>, now: i64, fx: &mut Effects) {
        let limits = self.rate_limits.get_or_insert_with(RateLimits::default);
        let same = |a: &ScopedLimit, b: &ScopedLimit| {
            (&a.label, a.used_pct, a.resets_at, &a.severity)
                == (&b.label, b.used_pct, b.resets_at, &b.severity)
        };
        fx.rate_limits |= limits.scoped.len() != scoped.len()
            || limits.scoped.iter().zip(&scoped).any(|(a, b)| !same(a, b));
        fx.store.extend(scoped.iter().map(|limit| StoreMsg::Limit {
            ts: now,
            limit: limit.clone(),
        }));
        limits.scoped = scoped;
    }

    /// Parks long-waiting sessions, marks sessions whose process died as gone
    /// and forgets old gone ones.
    pub fn tick(&mut self, now: i64, fx: &mut Effects) {
        for s in self.sessions.values_mut() {
            if s.kind == Kind::Interactive
                && s.activity == Activity::YourTurn
                && now - s.activity_since >= PARKED_AFTER_MS
            {
                let handed_over = s.activity_since;
                set_activity(
                    s,
                    Activity::Parked,
                    handed_over + PARKED_AFTER_MS,
                    &mut fx.store,
                );
                // NOTE: a parked session's age counts from the hand-over
                s.activity_since = handed_over;
                fx.touched.insert(s.id.clone());
            }
        }
        let lapsed: Vec<String> = self
            .sessions
            .values()
            .filter(|s| {
                s.claim
                    .as_ref()
                    .is_some_and(|c| c.until.is_some_and(|u| u <= now))
            })
            .map(|s| s.id.clone())
            .collect();
        for id in lapsed {
            self.set_claim(&id, None, fx);
        }
        let dead: Vec<String> = self
            .sessions
            .values()
            .filter(|s| s.activity != Activity::Gone && s.pid.is_some() && !self.alive(s.pid))
            .map(|s| s.id.clone())
            .collect();
        for id in dead {
            self.mark_gone(&id, now, "exited", fx);
        }
        let expired: Vec<String> = self
            .sessions
            .values()
            .filter(|s| s.activity == Activity::Gone && now - s.activity_since > GONE_RETENTION_MS)
            .map(|s| s.id.clone())
            .collect();
        for id in expired {
            self.sessions.remove(&id);
            self.aux.remove(&id);
            fx.removed.push(id);
        }
        for aux in self.aux.values_mut() {
            if aux
                .pending_window
                .is_some_and(|(_, at)| now - at > PENDING_WINDOW_MS)
            {
                aux.pending_window = None;
            }
        }
    }

    fn ensure(&mut self, id: &str, cwd: Option<&str>, started_at: i64, now: i64, fx: &mut Effects) {
        fx.touched.insert(id.to_owned());
        if self.sessions.contains_key(id) {
            return;
        }
        let session = Session {
            host: self.host.clone(),
            id: id.to_owned(),
            short_id: None,
            kind: Kind::Interactive,
            parent: None,
            pid: None,
            cwd: cwd.unwrap_or_default().to_owned(),
            workspace: cwd.and_then(|cwd| self.workspaces.of(cwd)),
            project: cwd
                .and_then(|cwd| self.workspaces.locate(cwd))
                .and_then(|(_, p)| p),
            name: None,
            title: None,
            activity: Activity::Idle,
            activity_since: now,
            waiting_for: None,
            tool: None,
            prompt: None,
            reply: None,
            message: None,
            model: None,
            effort: None,
            permission_mode: None,
            context_pct: None,
            cost_usd: None,
            lines_added: None,
            lines_removed: None,
            remote_url: None,
            window: None,
            hooked: false,
            renaming: false,
            channel: false,
            asks: Vec::new(),
            outbox: Vec::new(),
            inbox: Vec::new(),
            claim: None,
            resources: None,
            turn: None,
            tasks: Vec::new(),
            subagents: Vec::new(),
            started_at,
            last_event_at: now,
        };
        fx.store.push(session_row(&session, None));
        fx.created.push(id.to_owned());
        self.sessions.insert(id.to_owned(), session);
        self.aux.insert(id.to_owned(), Aux::default());
    }

    fn mark_gone(&mut self, id: &str, ts: i64, reason: &str, fx: &mut Effects) {
        let Some(s) = self.sessions.get_mut(id) else {
            return;
        };
        if s.activity == Activity::Gone {
            return;
        }
        s.tool = None;
        s.waiting_for = None;
        s.subagents.clear();
        set_activity(s, Activity::Gone, ts, &mut fx.store);
        record(&mut fx.store, ts, id, "end", json!({ "reason": reason }));
        fx.store.push(session_row(s, Some(ts)));
        fx.touched.insert(id.to_owned());
        self.set_claim(id, None, fx);
    }

    fn is_terminal(&self, window: &Window) -> bool {
        self.terminals.contains(&window.app_id)
    }

    fn owner_of(&self, wid: u64) -> Option<String> {
        self.sessions
            .values()
            .find(|s| s.window.as_ref().is_some_and(|w| w.id == wid))
            .map(|s| s.id.clone())
    }

    /// Remembers the focused terminal as this session's likely window. Claimed
    /// once `claude agents` confirms the session is interactive and the window
    /// shows a Claude title.
    fn note_focus_candidate(&mut self, id: &str, ts: i64, fx: &mut Effects) {
        let Some(wid) = self.focused else { return };
        if !self.windows.get(&wid).is_some_and(|w| self.is_terminal(w)) {
            return;
        }
        if let Some(aux) = self.aux.get_mut(id) {
            aux.pending_window = Some((wid, ts));
        }
        self.resolve_pending(id, ts, fx);
    }

    fn resolve_pending(&mut self, id: &str, now: i64, fx: &mut Effects) {
        let Some(aux) = self.aux.get_mut(id) else {
            return;
        };
        let Some((wid, at)) = aux.pending_window else {
            return;
        };
        if !aux.kind_known {
            if now - at > PENDING_WINDOW_MS {
                aux.pending_window = None;
            }
            return;
        }
        aux.pending_window = None;
        let interactive = self
            .sessions
            .get(id)
            .is_some_and(|s| s.kind == Kind::Interactive);
        let titled = self
            .windows
            .get(&wid)
            .is_some_and(|w| claude_title(&w.title).is_some());
        if interactive && titled {
            self.claim(id, wid, fx);
        }
    }

    fn claim(&mut self, id: &str, wid: u64, fx: &mut Effects) {
        let Some(w) = self.windows.get(&wid) else {
            return;
        };
        if !self.is_terminal(w) {
            return;
        }
        let title = shown_title(&w.title).to_owned();
        let Some(s) = self.sessions.get(id) else {
            return;
        };
        if let (Some(known), Some(shown)) = (&s.title, claude_title(&w.title))
            && known != shown
        {
            return;
        }
        if let Some(owner) = self.owner_of(wid) {
            if owner == id || self.sessions[&owner].activity != Activity::Gone {
                return;
            }
            if let Some(old) = self.sessions.get_mut(&owner) {
                old.window = None;
            }
            fx.touched.insert(owner);
        }
        if let Some(s) = self.sessions.get_mut(id) {
            s.window = Some(WindowRef { id: wid, title });
            fx.touched.insert(id.to_owned());
        }
    }

    /// Pairs live sessions without a window with the one terminal whose
    /// Claude title equals the session title.
    fn match_titles(&mut self, fx: &mut Effects) {
        let pairs: Vec<(String, u64)> = self
            .sessions
            .values()
            .filter(|s| s.activity != Activity::Gone && s.window.is_none())
            .filter_map(|s| {
                let title = s.title.as_deref()?;
                let mut hits = self
                    .windows
                    .values()
                    .filter(|w| self.is_terminal(w) && claude_title(&w.title) == Some(title));
                match (hits.next(), hits.next()) {
                    (Some(w), None) => Some((s.id.clone(), w.id)),
                    _ => None,
                }
            })
            .collect();
        for (id, wid) in pairs {
            self.claim(&id, wid, fx);
        }
    }
}

/// Updates `s.channel` from the connected pids; true if it changed. A lost
/// channel takes the undelivered replies with it.
fn sync_channel(s: &mut Session, channel_pids: &HashSet<u32>) -> bool {
    let up = s.pid.is_some_and(|pid| channel_pids.contains(&pid));
    if s.channel == up {
        return false;
    }
    s.channel = up;
    if !up {
        s.outbox.clear();
    }
    true
}

/// The text of a prompt made only of ccdash channel messages, as Claude Code
/// wraps them: `<channel source="…ccdash…" …>\ntext\n</channel>`.
fn channel_text(prompt: &str) -> Option<String> {
    let mut rest = prompt.trim();
    let mut parts = Vec::new();
    while !rest.is_empty() {
        let (source, after) = rest.strip_prefix("<channel source=\"")?.split_once('"')?;
        if !source.contains("ccdash") {
            return None;
        }
        let (content, after) = after.split_once('>')?.1.split_once("</channel>")?;
        parts.push(content.trim_matches('\n'));
        rest = after.trim_start();
    }
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The agent that raised the permission wait got its answer: the session
/// works on, or waits on the next open ask.
fn end_permission(
    s: &mut Session,
    permission_agent: &mut Option<String>,
    ts: i64,
    store: &mut Vec<StoreMsg>,
) {
    let Some(next) = s.asks.first() else {
        s.waiting_for = None;
        set_activity(s, Activity::Working, ts, store);
        return;
    };
    *permission_agent = next.agent_id.clone();
    s.waiting_for = Some(match &next.kind {
        AskKind::Tool { tool, input, .. } => match input.first() {
            Some((_, value)) => truncate(&format!("{tool}: {value}"), SUMMARY_LEN),
            None => tool.clone(),
        },
        other => ask_tool(other).to_owned(),
    });
}

/// Closes the asks of `agent` (None = main thread), only those of `tool`
/// when given.
fn close_asks(s: &mut Session, agent: Option<&str>, tool: Option<&str>) {
    s.asks
        .retain(|a| a.agent_id.as_deref() != agent || tool.is_some_and(|t| t != ask_tool(&a.kind)));
}

/// The tool behind an ask.
pub fn ask_tool(kind: &AskKind) -> &str {
    match kind {
        AskKind::Tool { tool, .. } => tool,
        AskKind::Question { .. } => "AskUserQuestion",
        AskKind::Plan { .. } => "ExitPlanMode",
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// `claude … --bg …` in a shell command.
fn starts_background_session(command: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.contains(&"--bg") && words.iter().any(|w| w.rsplit('/').next() == Some("claude"))
}

fn round_mib(bytes: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    (bytes + MIB / 2) / MIB * MIB
}

fn effort_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Description of the Agent call that started a subagent of `agent_type`.
fn take_pending_agent(
    pending: &mut Vec<(String, Option<String>)>,
    agent_type: &str,
) -> Option<String> {
    let i = pending
        .iter()
        .position(|(t, _)| t == agent_type || (t.is_empty() && agent_type == "general-purpose"))?;
    pending.remove(i).1
}

/// Task-list changes made by a successful TaskCreate, TaskUpdate or TodoWrite call.
fn task_deltas(
    tasks: &[Task],
    tool: &str,
    input: &Value,
    payload: &Value,
    aux: &mut Aux,
) -> Vec<Value> {
    match tool {
        "TaskCreate" => {
            let subject = str_of(input, "subject");
            if let Some(subject) = subject {
                aux.task_forms.remove(subject);
            }
            if aux.task_forms.len() > MAX_TASK_FORMS {
                aux.task_forms.clear();
            }
            payload
                .pointer("/tool_response/task/id")
                .and_then(id_of)
                .map(|id| {
                    json!({
                        "id": id,
                        "subject": subject,
                        "active_form": str_of(input, "activeForm"),
                    })
                })
                .into_iter()
                .collect()
        }
        "TaskUpdate" => input
            .get("taskId")
            .and_then(id_of)
            .map(|id| {
                json!({
                    "id": id,
                    "subject": str_of(input, "subject"),
                    "active_form": str_of(input, "activeForm"),
                    "status": str_of(input, "status"),
                })
            })
            .into_iter()
            .collect(),
        "TodoWrite" => {
            let Some(todos) = input.get("todos").and_then(Value::as_array) else {
                return Vec::new();
            };
            let listed = |t: &Task| todos.iter().any(|d| str_of(d, "content") == Some(&t.id));
            let mut out: Vec<Value> = tasks
                .iter()
                .filter(|t| !listed(t))
                .map(|t| json!({ "id": t.id, "status": "deleted" }))
                .collect();
            out.extend(todos.iter().filter_map(|d| {
                let content = str_of(d, "content")?;
                Some(json!({
                    "id": content,
                    "subject": content,
                    "active_form": str_of(d, "activeForm"),
                    "status": str_of(d, "status"),
                }))
            }));
            out
        }
        _ => Vec::new(),
    }
}

fn apply_and_record_task(s: &mut Session, delta: &Value, ts: i64, store: &mut Vec<StoreMsg>) {
    if let Some(entry) = apply_task(&mut s.tasks, delta) {
        record(store, ts, &s.id, "task", entry);
    }
}

/// Applies one task-list change (`{id, subject?, active_form?, status?}`,
/// status `deleted` removes). Returns the resulting entry when anything
/// changed; replaying those entries rebuilds the list.
pub fn apply_task(tasks: &mut Vec<Task>, delta: &Value) -> Option<Value> {
    let id = str_of(delta, "id")?;
    let status = str_of(delta, "status");
    if status == Some("deleted") {
        let i = tasks.iter().position(|t| t.id == id)?;
        let gone = tasks.remove(i);
        return Some(json!({ "id": id, "subject": gone.subject, "status": "deleted" }));
    }
    let (i, created) = match tasks.iter().position(|t| t.id == id) {
        Some(i) => (i, false),
        None => {
            let subject = str_of(delta, "subject")?;
            if tasks.len() >= MAX_TASKS {
                return None;
            }
            tasks.push(Task {
                id: id.to_owned(),
                subject: subject.to_owned(),
                active_form: None,
                status: TaskStatus::Pending,
            });
            (tasks.len() - 1, true)
        }
    };
    let task = &mut tasks[i];
    let before = task.clone();
    if let Some(subject) = str_of(delta, "subject") {
        task.subject = subject.to_owned();
    }
    if let Some(form) = str_of(delta, "active_form") {
        task.active_form = Some(form.to_owned());
    }
    if let Some(status) = status.and_then(task_status) {
        task.status = status;
    }
    (created || *task != before).then(|| serde_json::to_value(&*task).unwrap_or_default())
}

fn task_status(s: &str) -> Option<TaskStatus> {
    match s {
        "pending" => Some(TaskStatus::Pending),
        "in_progress" => Some(TaskStatus::InProgress),
        "completed" => Some(TaskStatus::Completed),
        _ => None,
    }
}

fn remote_url(remote_id: &str) -> String {
    format!("https://claude.ai/code/{remote_id}")
}

fn record(store: &mut Vec<StoreMsg>, ts: i64, session: &str, kind: &'static str, data: Value) {
    store.push(StoreMsg::Event {
        ts,
        session: session.to_owned(),
        kind,
        data,
    });
}

fn set_activity(s: &mut Session, to: Activity, ts: i64, store: &mut Vec<StoreMsg>) {
    if s.activity == to {
        return;
    }
    if !matches!(to, Activity::Permission | Activity::Working) {
        s.asks.clear();
    }
    record(
        store,
        ts,
        &s.id,
        "activity",
        json!({ "from": s.activity, "to": to }),
    );
    s.activity = to;
    s.activity_since = ts;
}

fn session_row(s: &Session, ended_at: Option<i64>) -> StoreMsg {
    StoreMsg::Session {
        id: s.id.clone(),
        cwd: s.cwd.clone(),
        name: s.name.clone(),
        title: s.title.clone(),
        kind: s.kind,
        started_at: s.started_at,
        ended_at,
    }
}

/// The poll's view of `s`, when it should override the hooks'. After a Stop,
/// only hooks end the turn: the poll can report `busy` for minutes past it.
fn agent_activity(e: &AgentEntry, s: &Session) -> Option<Activity> {
    let waiting_permission = e
        .waiting_for
        .as_deref()
        .is_some_and(|w| w.contains("permission"));
    let turn_over = matches!(s.activity, Activity::YourTurn | Activity::Parked);
    match e.status.as_deref()? {
        "busy" if s.hooked && turn_over && s.subagents.is_empty() => None,
        "busy" => Some(Activity::Working),
        "waiting" if waiting_permission => Some(Activity::Permission),
        "waiting" if turn_over => None,
        "waiting" => Some(Activity::YourTurn),
        "idle" if matches!(s.activity, Activity::Working | Activity::Permission) => {
            Some(Activity::YourTurn)
        }
        _ => None,
    }
}

fn parse_rate_limits(p: &Value, ts: i64) -> Option<RateLimits> {
    let limits = p.get("rate_limits")?;
    let limit = |key: &str| {
        let l = limits.get(key)?;
        Some(Limit {
            used_pct: l.get("used_percentage")?.as_f64()?,
            resets_at: l.get("resets_at").and_then(Value::as_i64).unwrap_or(0),
        })
    };
    Some(RateLimits {
        five_hour: limit("five_hour"),
        seven_day: limit("seven_day"),
        updated_at: ts,
        scoped: Vec::new(),
    })
}

/// What a tool call left behind, for the Shell view: a Bash call's stdout
/// and stderr, or the error of a failed call of any tool. Empty → None.
fn tool_output(ok: bool, payload: &Value, max: usize) -> Option<String> {
    if max == 0 {
        return None;
    }
    let text = if ok {
        let response = &payload["tool_response"];
        let part = |key: &str| response[key].as_str().unwrap_or_default().trim_end();
        match (part("stdout"), part("stderr")) {
            (out, "") => out.to_owned(),
            ("", err) => err.to_owned(),
            (out, err) => format!("{out}\n{err}"),
        }
    } else {
        str_of(payload, "error")
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    };
    (!text.trim().is_empty()).then(|| truncate(&text, max))
}

fn tool_summary(name: &str, input: &Value, cwd: &str) -> String {
    let field = |key: &str| input.get(key).and_then(Value::as_str);
    let text = match name {
        "Bash" => field("command").map(|c| c.lines().next().unwrap_or_default().to_owned()),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => field("file_path")
            .or_else(|| field("notebook_path"))
            .map(|p| relative(p, cwd)),
        "Grep" | "Glob" => field("pattern").map(str::to_owned),
        "WebFetch" => field("url").map(str::to_owned),
        "WebSearch" | "ToolSearch" => field("query").map(str::to_owned),
        "Skill" => field("skill").map(str::to_owned),
        "AskUserQuestion" => input
            .pointer("/questions/0/question")
            .and_then(Value::as_str)
            .map(str::to_owned),
        "TaskCreate" => field("subject").map(str::to_owned),
        "TaskUpdate" => input.get("taskId").and_then(id_of).map(|id| {
            let status = field("status").map(|s| s.replace('_', " "));
            format!("#{id} {}", status.unwrap_or_else(|| "updated".into()))
        }),
        _ => field("description").map(str::to_owned),
    };
    truncate(&text.unwrap_or_default(), SUMMARY_LEN)
}

fn relative(path: &str, cwd: &str) -> String {
    path.strip_prefix(cwd)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|rest| !rest.is_empty())
        .unwrap_or(path)
        .to_owned()
}

/// Claude Code prefixes its terminal title with a status glyph and a space.
fn claude_title(title: &str) -> Option<&str> {
    let mut chars = title.chars();
    let glyph = chars.next()?;
    if glyph.is_ascii() || glyph.is_alphanumeric() {
        return None;
    }
    chars.as_str().strip_prefix(' ')
}

fn shown_title(title: &str) -> &str {
    claude_title(title).unwrap_or(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::VERSION;

    const SID: &str = "11111111-2222-3333-4444-555555555555";
    const GHOSTTY: &str = "com.mitchellh.ghostty";

    fn state() -> State {
        State::new(
            "testhost".into(),
            vec![GHOSTTY.into()],
            Workspaces::new(&["/work".into()], "/home/u"),
        )
    }

    fn envelope(source: Source, ts: i64, payload: Value) -> Envelope {
        Envelope {
            v: VERSION,
            source,
            ts,
            pid: Some(std::process::id()),
            remote_id: Some("session_remote".into()),
            applied_title: None,
            payload,
        }
    }

    fn hook(st: &mut State, ts: i64, event: &str, extra: Value) -> Effects {
        let mut payload = json!({
            "session_id": SID,
            "cwd": "/work/repo",
            "hook_event_name": event,
        });
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut fx = Effects::default();
        st.apply(&envelope(Source::Hook, ts, payload), &mut fx);
        fx
    }

    fn statusline(st: &mut State, ts: i64, title: &str) -> Effects {
        let mut fx = Effects::default();
        let payload = json!({
            "session_id": SID,
            "cwd": "/work/repo",
            "session_name": title,
            "model": {"id": "claude-haiku-5-5", "display_name": "Haiku 5.5"},
            "cost": {"total_cost_usd": 0.25, "total_lines_added": 3, "total_lines_removed": 1},
            "context_window": {"used_percentage": 4},
            "rate_limits": {
                "five_hour": {"used_percentage": 1, "resets_at": 1791691800},
                "seven_day": {"used_percentage": 27, "resets_at": 1792026000},
            },
        });
        st.apply(&envelope(Source::Statusline, ts, payload), &mut fx);
        fx
    }

    fn agents(st: &mut State, now: i64, kind: &str, status: &str) -> Effects {
        let mut fx = Effects::default();
        let entry = AgentEntry {
            session_id: SID.into(),
            pid: Some(std::process::id()),
            id: (kind == "background").then(|| "11111111".into()),
            kind: Some(kind.into()),
            name: Some("repo-a1".into()),
            status: Some(status.into()),
            ..Default::default()
        };
        st.agents(&[entry], now, &mut fx);
        fx
    }

    fn window(id: u64, title: &str, focused: bool) -> Window {
        Window {
            id,
            app_id: GHOSTTY.into(),
            title: title.into(),
            focused,
        }
    }

    fn session(st: &State) -> &Session {
        st.session(SID).unwrap()
    }

    fn kinds(fx: &Effects) -> Vec<&'static str> {
        fx.store
            .iter()
            .filter_map(|m| match m {
                StoreMsg::Event { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn full_turn() {
        let mut st = state();
        let fx = hook(
            &mut st,
            1,
            "SessionStart",
            json!({"source": "startup", "model": "claude-haiku-5-5"}),
        );
        assert_eq!(fx.created, vec![SID]);
        assert_eq!(session(&st).activity, Activity::Idle);

        hook(
            &mut st,
            2,
            "UserPromptSubmit",
            json!({"prompt": "do the thing", "permission_mode": "default"}),
        );
        assert_eq!(session(&st).activity, Activity::Working);
        assert_eq!(session(&st).prompt.as_ref().unwrap().text, "do the thing");
        assert_eq!(
            session(&st).remote_url.as_deref(),
            Some("https://claude.ai/code/session_remote")
        );

        hook(
            &mut st,
            3,
            "PreToolUse",
            json!({"tool_name": "Read", "tool_input": {"file_path": "/work/repo/src/main.rs"}}),
        );
        let tool = session(&st).tool.clone().unwrap();
        assert_eq!(
            (tool.name.as_str(), tool.summary.as_str()),
            ("Read", "src/main.rs")
        );

        let fx = hook(
            &mut st,
            50,
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {}, "duration_ms": 47}),
        );
        assert!(session(&st).tool.is_none());
        let StoreMsg::Event { data, .. } = &fx.store[0] else {
            panic!()
        };
        assert_eq!(data["ms"], 47);
        assert_eq!(data["summary"], "src/main.rs");

        let fx = hook(
            &mut st,
            60,
            "Stop",
            json!({"last_assistant_message": "- done"}),
        );
        let s = session(&st);
        assert_eq!((s.activity, s.activity_since), (Activity::YourTurn, 60));
        assert_eq!(s.reply.as_ref().unwrap().text, "- done");
        assert_eq!(kinds(&fx), vec!["activity", "reply"]);
    }

    #[test]
    fn turn_progress_and_prompt_title() {
        let mut st = state();
        hook(
            &mut st,
            1,
            "UserPromptSubmit",
            json!({"prompt": "go", "session_title": "Fix the parser", "effort": "high"}),
        );
        let s = session(&st);
        assert!(s.hooked);
        assert_eq!(s.title.as_deref(), Some("Fix the parser"));
        assert_eq!(s.effort.as_deref(), Some("high"));
        for (ts, event, file) in [
            (2, "PostToolUse", "/work/repo/src/a.rs"),
            (3, "PostToolUse", "/work/repo/src/a.rs"),
            (4, "PostToolUseFailure", "/work/repo/src/b.rs"),
        ] {
            hook(
                &mut st,
                ts,
                event,
                json!({"tool_name": "Edit", "tool_input": {"file_path": file}}),
            );
        }
        hook(&mut st, 9, "Stop", json!({}));
        let turn = session(&st).turn.clone().unwrap();
        assert_eq!(
            turn,
            Turn {
                started_at: 1,
                ended_at: Some(9),
                tools: 3,
                failures: 1,
                subagent_tools: 0,
                files: vec!["src/a.rs".into()],
            }
        );
    }

    #[test]
    fn subagents_track_progress_without_moving_the_main_state() {
        let mut st = state();
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "look"}));
        hook(
            &mut st,
            2,
            "PreToolUse",
            json!({"tool_name": "Agent", "tool_input": {"description": "Find callers", "subagent_type": "Explore", "run_in_background": true}}),
        );
        hook(
            &mut st,
            3,
            "SubagentStart",
            json!({"agent_id": "a1", "agent_type": "Explore"}),
        );
        hook(&mut st, 4, "PostToolUse", json!({"tool_name": "Agent"}));
        hook(
            &mut st,
            5,
            "Stop",
            json!({"last_assistant_message": "started"}),
        );
        assert_eq!(session(&st).activity, Activity::YourTurn);

        hook(
            &mut st,
            6,
            "PreToolUse",
            json!({"agent_id": "a1", "agent_type": "Explore", "tool_name": "Grep", "tool_input": {"pattern": "parse("}}),
        );
        let s = session(&st);
        assert_eq!(s.activity, Activity::YourTurn);
        assert!(s.tool.is_none());
        let sub = &s.subagents[0];
        assert_eq!(sub.description.as_deref(), Some("Find callers"));
        assert_eq!(sub.tool.as_ref().unwrap().summary, "parse(");

        let fx = hook(
            &mut st,
            7,
            "PostToolUse",
            json!({"agent_id": "a1", "tool_name": "Grep"}),
        );
        assert!(kinds(&fx).is_empty(), "subagent tools are not recorded");
        assert_eq!(session(&st).subagents[0].tools, 1);
        assert_eq!(session(&st).turn.as_ref().unwrap().subagent_tools, 1);

        let fx = hook(
            &mut st,
            9,
            "SubagentStop",
            json!({"agent_id": "a1", "agent_type": "Explore", "last_assistant_message": "3 callers"}),
        );
        assert!(session(&st).subagents.is_empty());
        let StoreMsg::Event { kind, data, .. } = &fx.store[0] else {
            panic!()
        };
        assert_eq!(*kind, "subagent");
        assert_eq!(
            data,
            &json!({"agent_type": "Explore", "description": "Find callers", "ms": 6, "tools": 1, "text": "3 callers"})
        );
    }

    #[test]
    fn subagent_permission_is_cleared_by_that_subagent_only() {
        let mut st = state();
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "go"}));
        hook(
            &mut st,
            2,
            "PermissionRequest",
            json!({"agent_id": "a1", "tool_name": "Bash", "tool_input": {"command": "rm x"}}),
        );
        hook(
            &mut st,
            3,
            "PostToolUse",
            json!({"agent_id": "a2", "tool_name": "Read"}),
        );
        assert_eq!(session(&st).activity, Activity::Permission);
        hook(
            &mut st,
            4,
            "PostToolUse",
            json!({"agent_id": "a1", "tool_name": "Bash"}),
        );
        assert_eq!(session(&st).activity, Activity::Working);
    }

    #[test]
    fn task_list_from_hooks_and_tools() {
        let mut st = state();
        hook(
            &mut st,
            1,
            "PreToolUse",
            json!({"tool_name": "TaskCreate", "tool_input": {"subject": "Flash board", "activeForm": "Flashing board"}}),
        );
        hook(
            &mut st,
            2,
            "TaskCreated",
            json!({"task_id": "1", "task_subject": "Flash board"}),
        );
        let fx = hook(
            &mut st,
            3,
            "PostToolUse",
            json!({"tool_name": "TaskCreate", "tool_input": {"subject": "Flash board", "activeForm": "Flashing board"}, "tool_response": {"task": {"id": "1", "subject": "Flash board"}}}),
        );
        assert!(kinds(&fx).contains(&"tool") && !kinds(&fx).contains(&"task"));
        hook(
            &mut st,
            4,
            "PostToolUse",
            json!({"tool_name": "TaskCreate", "tool_input": {"subject": "Verify"}, "tool_response": {"task": {"id": 2}}}),
        );
        hook(
            &mut st,
            5,
            "PostToolUse",
            json!({"tool_name": "TaskUpdate", "tool_input": {"taskId": "1", "status": "in_progress"}}),
        );
        let tasks = &session(&st).tasks;
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks[0],
            Task {
                id: "1".into(),
                subject: "Flash board".into(),
                active_form: Some("Flashing board".into()),
                status: TaskStatus::InProgress,
            }
        );
        let fx = hook(
            &mut st,
            6,
            "TaskCompleted",
            json!({"task_id": "1", "task_subject": "Flash board"}),
        );
        let StoreMsg::Event { data, .. } = &fx.store[0] else {
            panic!()
        };
        assert_eq!(data["status"], "completed");
        hook(
            &mut st,
            7,
            "PostToolUse",
            json!({"tool_name": "TaskUpdate", "tool_input": {"taskId": "2", "status": "deleted"}}),
        );
        assert_eq!(session(&st).tasks.len(), 1);
        assert_eq!(session(&st).tasks[0].status, TaskStatus::Completed);
    }

    #[test]
    fn todo_write_replaces_the_list() {
        let mut st = state();
        let todos =
            |items: Value| json!({"tool_name": "TodoWrite", "tool_input": {"todos": items}});
        hook(
            &mut st,
            1,
            "PostToolUse",
            todos(json!([
                {"content": "a", "status": "completed", "activeForm": "Doing a"},
                {"content": "b", "status": "in_progress", "activeForm": "Doing b"},
            ])),
        );
        hook(
            &mut st,
            2,
            "PostToolUse",
            todos(json!([{"content": "b", "status": "completed"}])),
        );
        let tasks = &session(&st).tasks;
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            (tasks[0].subject.as_str(), tasks[0].status),
            ("b", TaskStatus::Completed)
        );
    }

    #[test]
    fn permission_round_trip() {
        let mut st = state();
        hook(
            &mut st,
            1,
            "UserPromptSubmit",
            json!({"prompt": "touch it"}),
        );
        hook(
            &mut st,
            2,
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "touch /tmp/x\necho more"}}),
        );
        hook(
            &mut st,
            3,
            "PermissionRequest",
            json!({"tool_name": "Bash", "tool_input": {"command": "touch /tmp/x"}}),
        );
        hook(
            &mut st,
            4,
            "Notification",
            json!({"notification_type": "permission_prompt", "message": "Claude needs your permission"}),
        );
        let s = session(&st);
        assert_eq!((s.activity, s.activity_since), (Activity::Permission, 3));
        assert_eq!(s.waiting_for.as_deref(), Some("Bash: touch /tmp/x"));

        hook(&mut st, 9, "PostToolUse", json!({"tool_name": "Bash"}));
        let s = session(&st);
        assert_eq!(s.activity, Activity::Working);
        assert!(s.waiting_for.is_none());
    }

    #[test]
    fn session_end_marks_gone_and_tick_expires() {
        let mut st = state();
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "x"}));
        let fx = hook(&mut st, 2, "SessionEnd", json!({"reason": "other"}));
        assert_eq!(session(&st).activity, Activity::Gone);
        assert!(fx.store.iter().any(|m| matches!(
            m,
            StoreMsg::Session {
                ended_at: Some(2),
                ..
            }
        )));

        let mut fx = Effects::default();
        st.tick(2 + GONE_RETENTION_MS + 1, &mut fx);
        assert_eq!(fx.removed, vec![SID]);
        assert!(st.session(SID).is_none());
    }

    #[test]
    fn scoped_limits_survive_statusline_updates() {
        let mut st = state();
        let fable = ScopedLimit {
            label: "Fable".into(),
            used_pct: 91.0,
            resets_at: 1_792_026_000,
            severity: "critical".into(),
            read_at: 500,
        };
        let mut fx = Effects::default();
        st.usage(vec![fable.clone()], 500, &mut fx);
        assert!(fx.rate_limits);
        assert!(matches!(&fx.store[..], [StoreMsg::Limit { ts: 500, .. }]));

        statusline(&mut st, 1_000, "Fix the parser");
        let limits = st.rate_limits().unwrap();
        assert_eq!((limits.scoped.len(), limits.updated_at), (1, 1_000));

        // The same reading again changes nothing to publish.
        let mut fx = Effects::default();
        st.usage(
            vec![ScopedLimit {
                read_at: 900,
                ..fable
            }],
            900,
            &mut fx,
        );
        assert!(!fx.rate_limits);
    }

    #[test]
    fn statusline_sets_stats_and_throttles_samples() {
        let mut st = state();
        let fx = statusline(&mut st, 1_000, "Fix the parser");
        let s = session(&st);
        assert_eq!(s.title.as_deref(), Some("Fix the parser"));
        assert_eq!(s.model.as_deref(), Some("Haiku 5.5"));
        assert_eq!(
            (s.context_pct, s.cost_usd, s.lines_added),
            (Some(4.0), Some(0.25), Some(3))
        );
        assert!(fx.rate_limits);
        assert_eq!(
            st.rate_limits()
                .unwrap()
                .seven_day
                .as_ref()
                .unwrap()
                .used_pct,
            27.0
        );

        let fx = statusline(&mut st, 2_000, "Fix the parser");
        assert!(!fx.rate_limits);
        assert!(
            !fx.store
                .iter()
                .any(|m| matches!(m, StoreMsg::Sample { .. }))
        );
        let fx = statusline(&mut st, 1_000 + SAMPLE_EVERY_MS, "Fix the parser");
        assert!(
            fx.store
                .iter()
                .any(|m| matches!(m, StoreMsg::Sample { .. }))
        );
    }

    #[test]
    fn agents_create_reconcile_and_vanish() {
        let mut st = state();
        let fx = agents(&mut st, 100_000, "background", "busy");
        assert_eq!(fx.created, vec![SID]);
        let s = session(&st);
        assert_eq!((s.kind, s.activity), (Kind::Background, Activity::Working));
        assert_eq!(s.short_id.as_deref(), Some("11111111"));

        // Fresh hooks win over a lagging poll.
        hook(
            &mut st,
            100_500,
            "Stop",
            json!({"last_assistant_message": "ok"}),
        );
        agents(&mut st, 101_000, "background", "busy");
        assert_eq!(session(&st).activity, Activity::YourTurn);

        // Missing twice with a dead pid → gone.
        st.sessions.get_mut(SID).unwrap().pid = Some(u32::MAX);
        st.report_dead(&[u32::MAX]);
        let mut fx = Effects::default();
        st.agents(&[], 200_000, &mut fx);
        assert_eq!(session(&st).activity, Activity::YourTurn);
        st.agents(&[], 205_000, &mut fx);
        assert_eq!(session(&st).activity, Activity::Gone);
    }

    #[test]
    fn a_busy_poll_does_not_reopen_a_finished_turn() {
        let mut st = state();
        hook(&mut st, 1_000, "UserPromptSubmit", json!({"prompt": "hi"}));
        hook(
            &mut st,
            2_000,
            "Stop",
            json!({"last_assistant_message": "ok"}),
        );
        agents(&mut st, 60_000, "interactive", "busy");
        assert_eq!(session(&st).activity, Activity::YourTurn);

        // A running subagent makes the session busy for real.
        hook(
            &mut st,
            61_000,
            "SubagentStart",
            json!({"agent_id": "a1", "agent_type": "Explore"}),
        );
        agents(&mut st, 80_000, "interactive", "busy");
        assert_eq!(session(&st).activity, Activity::Working);
    }

    #[test]
    fn your_turn_parks_after_an_hour() {
        let mut st = state();
        hook(&mut st, 1_000, "UserPromptSubmit", json!({"prompt": "hi"}));
        hook(
            &mut st,
            2_000,
            "Stop",
            json!({"last_assistant_message": "ok"}),
        );
        let mut fx = Effects::default();
        st.tick(2_000 + PARKED_AFTER_MS - 1, &mut fx);
        assert_eq!(session(&st).activity, Activity::YourTurn);
        st.tick(2_000 + PARKED_AFTER_MS + 10_000, &mut fx);
        let s = session(&st);
        assert_eq!((s.activity, s.activity_since), (Activity::Parked, 2_000));
        assert!(fx.touched.contains(SID));
        let StoreMsg::Event { ts, data, .. } = fx.store.last().unwrap() else {
            panic!("expected the transition");
        };
        assert_eq!(
            (*ts, data["to"].as_str()),
            (2_000 + PARKED_AFTER_MS, Some("parked"))
        );

        // The poll's view of an idle session doesn't unpark it; a prompt does.
        agents(
            &mut st,
            2_000 + PARKED_AFTER_MS + 20_000,
            "interactive",
            "waiting",
        );
        assert_eq!(session(&st).activity, Activity::Parked);
        hook(
            &mut st,
            2_000 + PARKED_AFTER_MS + 30_000,
            "UserPromptSubmit",
            json!({"prompt": "next"}),
        );
        assert_eq!(session(&st).activity, Activity::Working);
    }

    #[test]
    fn focus_claim_waits_for_kind_and_claude_title() {
        let mut st = state();
        let mut fx = Effects::default();
        st.niri(
            NiriEvent::Windows(vec![window(7, "~/P/repo", true), window(8, "other", false)]),
            &mut fx,
        );

        hook(&mut st, 1, "SessionStart", json!({"source": "startup"}));
        assert!(session(&st).window.is_none());

        // Claude has set its title by the time `claude agents` reports the session.
        st.niri(
            NiriEvent::WindowChanged(window(7, "✳ Claude Code", true)),
            &mut fx,
        );
        agents(&mut st, 2, "interactive", "idle");
        assert_eq!(
            session(&st).window,
            Some(WindowRef {
                id: 7,
                title: "Claude Code".into()
            })
        );

        // Spinner glyph changes don't count as title changes.
        let mut fx = Effects::default();
        st.niri(
            NiriEvent::WindowChanged(window(7, "◐ Claude Code", true)),
            &mut fx,
        );
        assert!(fx.touched.is_empty());

        st.niri(NiriEvent::WindowClosed(7), &mut fx);
        assert!(session(&st).window.is_none());
    }

    #[test]
    fn background_sessions_never_claim_the_focused_terminal() {
        let mut st = state();
        let mut fx = Effects::default();
        st.niri(
            NiriEvent::Windows(vec![window(7, "✳ Some other chat", true)]),
            &mut fx,
        );
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "x"}));
        agents(&mut st, 2, "background", "busy");
        assert!(session(&st).window.is_none());
    }

    #[test]
    fn title_match_and_mismatch() {
        let mut st = state();
        let mut fx = Effects::default();
        st.niri(
            NiriEvent::Windows(vec![
                window(3, "✳ Fix the parser", false),
                window(4, "✳ Something else", true),
            ]),
            &mut fx,
        );
        statusline(&mut st, 1, "Fix the parser");
        assert_eq!(session(&st).window.as_ref().map(|w| w.id), Some(3));

        // A prompt from the phone while another Claude window is focused must not steal it.
        agents(&mut st, 2, "interactive", "busy");
        hook(
            &mut st,
            3,
            "UserPromptSubmit",
            json!({"prompt": "from phone"}),
        );
        assert_eq!(session(&st).window.as_ref().map(|w| w.id), Some(3));
    }

    #[test]
    fn resolve_by_prefix_and_short_id() {
        let mut st = state();
        agents(&mut st, 1, "background", "idle");
        assert!(st.resolve("11111111").is_some());
        assert!(st.resolve("11111111-2222").is_some());
        assert!(st.resolve("nope").is_none());
    }

    #[test]
    fn message_display_streams_and_records_whole_messages() {
        let mut st = state();
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "go"}));
        let delta = |index: u32, text: &str, last: bool| json!({"turn_id": "t", "message_id": "m1", "index": index, "final": last, "delta": text});
        let fx = hook(
            &mut st,
            2,
            "MessageDisplay",
            delta(0, "Let me check\n", false),
        );
        assert!(kinds(&fx).is_empty());
        assert_eq!(session(&st).message.as_ref().unwrap().text, "Let me check");
        let fx = hook(&mut st, 3, "MessageDisplay", delta(1, "the logs.", true));
        let StoreMsg::Event { kind, data, .. } = &fx.store[0] else {
            panic!()
        };
        assert_eq!(
            (*kind, data["text"].as_str()),
            ("message", Some("Let me check\nthe logs."))
        );
        assert_eq!(session(&st).activity, Activity::Working);

        let fx = hook(
            &mut st,
            4,
            "MessageDisplay",
            json!({"agent_id": "a1", "message_id": "m2", "final": true, "delta": "sub"}),
        );
        assert!(kinds(&fx).is_empty(), "subagent text is not recorded");

        hook(&mut st, 5, "UserPromptSubmit", json!({"prompt": "next"}));
        assert!(session(&st).message.is_none());
    }

    #[test]
    fn long_prompts_are_previews_on_the_session() {
        let mut st = state();
        let long = "x".repeat(PREVIEW_LEN * 3);
        let fx = hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": long}));
        assert!(session(&st).prompt.as_ref().unwrap().text.len() <= PREVIEW_LEN);
        let stored = fx.store.iter().find_map(|m| match m {
            StoreMsg::Event {
                kind: "prompt",
                data,
                ..
            } => data["text"].as_str(),
            _ => None,
        });
        assert_eq!(stored.map(str::len), Some(PREVIEW_LEN * 3));
    }

    #[test]
    fn resources_publish_and_sample_per_minute() {
        let mut st = state();
        hook(&mut st, 1, "SessionStart", json!({}));
        let poll = |cpu: f64, rss: u64| ProcPoll {
            session: SID.into(),
            cpu_pct: cpu,
            mem_bytes: rss,
            procs: 2,
            top: Vec::new(),
            forked_from: None,
        };
        let mut fx = Effects::default();
        st.resources(vec![poll(10.2, 300 << 20)], 1_000, &mut fx);
        assert!(fx.touched.contains(SID));
        assert!(fx.store.is_empty());
        let r = session(&st).resources.clone().unwrap();
        assert_eq!((r.cpu_pct, r.mem_bytes), (10.0, 300 << 20));

        let mut fx = Effects::default();
        st.resources(vec![poll(30.0, 400 << 20)], 61_000, &mut fx);
        let Some(StoreMsg::ProcSample {
            cpu_pct, mem_bytes, ..
        }) = fx.store.first()
        else {
            panic!("expected a sample")
        };
        assert_eq!((*cpu_pct, *mem_bytes), (20.1, 400 << 20));

        let mut fx = Effects::default();
        st.resources(Vec::new(), 66_000, &mut fx);
        assert!(session(&st).resources.is_none() && fx.touched.contains(SID));
    }

    #[test]
    fn background_sessions_link_to_the_session_that_started_them() {
        let mut st = state();
        hook(
            &mut st,
            1,
            "PreToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "cd /tmp/x && claude --bg --model m \"go\""}}),
        );
        let start = |st: &mut State, id: &str, ts: i64| {
            let mut fx = Effects::default();
            let payload = json!({"session_id": id, "cwd": "/tmp/x", "hook_event_name": "SessionStart", "source": "startup"});
            st.apply(&envelope(Source::Hook, ts, payload), &mut fx);
        };
        start(&mut st, "child", 2_000);
        assert_eq!(st.session("child").unwrap().parent.as_deref(), Some(SID));
        start(&mut st, "later", 3_000);
        assert_eq!(
            st.session("later").unwrap().parent,
            None,
            "a spawn links once"
        );

        let entry = |id: &str, kind: &str| AgentEntry {
            session_id: id.into(),
            kind: Some(kind.into()),
            status: Some("idle".into()),
            pid: Some(std::process::id()),
            ..Default::default()
        };
        let mut fx = Effects::default();
        st.agents(
            &[entry("child", "background"), entry(SID, "interactive")],
            4_000,
            &mut fx,
        );
        assert_eq!(st.session("child").unwrap().kind, Kind::Background);
        assert_eq!(st.session("child").unwrap().parent.as_deref(), Some(SID));
        assert!(starts_background_session("/nix/store/x/bin/claude --bg hi"));
        assert!(!starts_background_session("claude agents --json"));
    }

    #[test]
    fn renames_show_at_once_and_end_when_claude_adopts_them() {
        let mut st = state();
        statusline(&mut st, 1, "Old title");
        let mut fx = Effects::default();
        assert_eq!(
            st.rename(&SID[..8], "New title", &mut fx),
            Ok(SID.to_owned())
        );
        assert!(fx.touched.contains(SID));
        let s = session(&st);
        assert_eq!((s.title.as_deref(), s.renaming), (Some("New title"), true));

        statusline(&mut st, 2, "Old title");
        hook(
            &mut st,
            3,
            "UserPromptSubmit",
            json!({"prompt": "x", "session_title": "Old title"}),
        );
        assert_eq!(
            session(&st).title.as_deref(),
            Some("New title"),
            "stale titles are ignored"
        );

        let mut env = envelope(
            Source::Hook,
            4,
            json!({"session_id": SID, "hook_event_name": "UserPromptSubmit", "prompt": "y"}),
        );
        env.applied_title = Some("New title".into());
        st.apply(&env, &mut Effects::default());
        assert!(!session(&st).renaming);
        statusline(&mut st, 5, "Renamed in Claude");
        assert_eq!(session(&st).title.as_deref(), Some("Renamed in Claude"));

        let mut fx = Effects::default();
        st.rename(SID, "Again", &mut fx).unwrap();
        statusline(&mut st, 6, "Again");
        assert!(
            !session(&st).renaming,
            "the statusline confirms a lost hand-off"
        );
    }

    #[test]
    fn channel_replies_queue_until_claude_takes_them() {
        let mut st = state();
        let pid = std::process::id();
        let mut fx = Effects::default();
        st.channel(pid, true, &mut fx);
        assert!(fx.touched.is_empty(), "no session runs that pid yet");
        hook(&mut st, 1, "SessionStart", json!({"source": "startup"}));
        assert!(session(&st).channel, "the pid arrives after the channel");

        st.sent_reply(SID, "from the phone", 2, &mut Effects::default());
        assert_eq!(session(&st).outbox.len(), 1);
        hook(&mut st, 3, "UserPromptSubmit", json!({"prompt": "typed"}));
        assert_eq!(session(&st).outbox.len(), 1, "a typed prompt is not it");
        let fx = hook(
            &mut st,
            4,
            "UserPromptSubmit",
            json!({"prompt": "<channel source=\"plugin:ccdash:ccdash\">\nfrom the phone\n</channel>"}),
        );
        let s = session(&st);
        assert!(s.outbox.is_empty());
        assert_eq!(s.prompt.as_ref().unwrap().text, "from the phone");
        assert!(fx.store.iter().any(|m| matches!(m,
            StoreMsg::Event { kind: "prompt", data, .. } if data["via"] == "ccdash")));

        st.sent_reply(SID, "lost", 5, &mut Effects::default());
        let mut fx = Effects::default();
        st.channel(pid, false, &mut fx);
        assert!(fx.touched.contains(SID));
        let s = session(&st);
        assert!(!s.channel && s.outbox.is_empty());
    }

    #[test]
    fn asks_close_with_their_prompt() {
        let mut st = state();
        let ask = |id: &str, at: i64, agent: Option<&str>| Ask {
            id: id.into(),
            agent_id: agent.map(str::to_owned),
            at,
            kind: AskKind::Tool {
                tool: "Bash".into(),
                description: None,
                input: vec![("command".into(), "ls".into())],
                always: None,
            },
        };
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "go"}));
        let bash = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        hook(&mut st, 2, "PermissionRequest", bash.clone());
        assert!(st.add_ask(SID, ask("a", 2, None), &mut Effects::default()));
        assert!(st.add_ask(SID, ask("s", 2, Some("sub")), &mut Effects::default()));
        assert!(st.ask(SID, "a").is_some());

        // A different tool's result leaves the prompt open; this one's closes it.
        hook(
            &mut st,
            3,
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {}}),
        );
        assert!(st.ask(SID, "a").is_some());
        hook(&mut st, 4, "PostToolUse", bash.clone());
        assert!(st.ask(SID, "a").is_none());
        assert!(st.ask(SID, "s").is_some(), "the subagent's prompt stays");

        // A prompt that arrives after its own answer is not shown.
        assert!(!st.add_ask(SID, ask("late", 3, None), &mut Effects::default()));
        let mut fx = Effects::default();
        hook(&mut st, 5, "PermissionRequest", bash.clone());
        assert!(st.add_ask(SID, ask("b", 5, None), &mut fx));
        st.answered(SID, "b", &Answer::Allow, "web", 6, &mut fx);
        assert!(st.ask(SID, "b").is_none());
        assert!(kinds(&fx).contains(&"answered"));

        // The session moving on clears whatever is left.
        hook(
            &mut st,
            7,
            "Stop",
            json!({"last_assistant_message": "done"}),
        );
        assert!(session(&st).asks.is_empty());
    }

    #[test]
    fn the_poll_leaves_a_held_prompt_alone() {
        let mut st = state();
        hook(&mut st, 1, "UserPromptSubmit", json!({"prompt": "go"}));
        hook(
            &mut st,
            2,
            "PermissionRequest",
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        );
        let held = Ask {
            id: "a".into(),
            agent_id: None,
            at: 2,
            kind: AskKind::Plan { plan: "p".into() },
        };
        assert!(st.add_ask(SID, held, &mut Effects::default()));
        agents(&mut st, 60_000, "interactive", "idle");
        assert_eq!(session(&st).activity, Activity::Permission);
    }

    #[test]
    fn channel_prompts_unwrap_only_when_all_from_ccdash() {
        assert_eq!(
            channel_text("<channel source=\"ccdash\" k=\"v\">\nhi\n</channel>").as_deref(),
            Some("hi")
        );
        assert_eq!(
            channel_text(
                "<channel source=\"ccdash\">\na\n</channel>\n<channel source=\"ccdash\">\nb\n</channel>"
            )
            .as_deref(),
            Some("a\n\nb")
        );
        assert_eq!(
            channel_text("<channel source=\"webhook\">\nx\n</channel>"),
            None
        );
        assert_eq!(channel_text("plain prompt"), None);
        assert_eq!(
            channel_text("<channel source=\"ccdash\">\nunterminated"),
            None
        );
    }

    #[test]
    fn tool_summaries() {
        let cases = [
            ("Bash", json!({"command": "make\nmake test"}), "make"),
            (
                "Edit",
                json!({"file_path": "/work/repo/src/a.rs"}),
                "src/a.rs",
            ),
            (
                "TaskCreate",
                json!({"subject": "Ship", "description": "long"}),
                "Ship",
            ),
            (
                "TaskUpdate",
                json!({"taskId": "2", "status": "in_progress"}),
                "#2 in progress",
            ),
            (
                "Agent",
                json!({"description": "Find callers"}),
                "Find callers",
            ),
            (
                "AskUserQuestion",
                json!({"questions": [{"question": "Which one?"}]}),
                "Which one?",
            ),
        ];
        for (name, input, want) in cases {
            assert_eq!(tool_summary(name, &input, "/work/repo"), want, "{name}");
        }
    }

    #[test]
    fn claude_title_detection() {
        assert_eq!(claude_title("✳ Fix it"), Some("Fix it"));
        assert_eq!(claude_title("◐ Fix it"), Some("Fix it"));
        assert_eq!(claude_title("~/P/repo"), None);
        assert_eq!(claude_title("Čtení"), None);
    }
}
