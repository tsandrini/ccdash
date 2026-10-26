//! Phone notifications. An open prompt or a finished turn notifies after a
//! short wait unless it ends first, and its notification is taken down once
//! it ends. Prompt notifications carry buttons that answer through
//! `/act/{key}/{action}` on the hub, so the phone (or a watch mirroring it)
//! answers without opening anything. The [`Composer`] turns the live state
//! into notes; a [`Sink`] delivers them (Web Push, in `push`).

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

use anyhow::Result;
use serde::Serialize;
use tokio::sync::watch;
use tracing::warn;

use crate::{
    config::Notify,
    model::{Activity, Ask, AskKind, Kind, Session},
    state::State,
    util::{now_ms, truncate},
};

const BODY_LEN: usize = 400;
const LABEL_LEN: usize = 40;
/// At most three buttons; the service worker fits the phone's own limit
/// from the same list.
const MAX_BUTTONS: usize = 3;

/// A notification that is due once its wait passes.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// Identifies the notification (the push tag); the delete addresses it.
    pub seq: String,
    pub opened_at: i64,
    /// The session's window has keyboard focus.
    pub focused: bool,
    pub content: Content,
}

/// What every backend shows.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Content {
    pub title: String,
    pub body: String,
    /// The session the notification is about.
    pub session: String,
    /// Deep link into the panel: the session's page.
    pub url: String,
    /// The ask's button key, when the prompt answers from a button.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub buttons: Vec<Button>,
    /// More answers than buttons: offer opening the panel.
    pub more: bool,
    /// A prompt, as opposed to a finished turn.
    pub urgent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Button {
    pub label: String,
    /// The `/act` action: `allow`, `deny`, `always`, `o<n>`.
    pub action: String,
}

/// Turns the live state into notification candidates.
pub struct Composer {
    cfg: Notify,
    web_url: String,
    /// Things that started before the daemon did never notify.
    started_at: i64,
}

impl Composer {
    pub fn new(cfg: Notify, web_url: String, started_at: i64) -> Self {
        Self {
            cfg,
            web_url: web_url.trim_end_matches('/').to_owned(),
            started_at,
        }
    }

    /// Candidates for every open ask and finished turn; `keys` maps ask ids
    /// to their button keys.
    pub fn notes(&self, state: &State, keys: &HashMap<&str, &str>, now: i64) -> Vec<Note> {
        let mut out = Vec::new();
        for s in state.sessions() {
            if s.activity == Activity::Gone || !self.covers(s) {
                continue;
            }
            let focused = state
                .focused()
                .is_some_and(|f| s.window.as_ref().is_some_and(|w| w.id == f));
            for ask in &s.asks {
                if ask.at < self.started_at || !self.wanted(&ask.kind) {
                    continue;
                }
                let key = keys.get(ask.id.as_str()).copied();
                out.push(Note {
                    seq: format!("ask-{}", ask.id),
                    opened_at: ask.at,
                    focused,
                    content: self.ask_content(s, ask, key),
                });
            }
            // NOTE: hooked only: unhooked turns come from the agents poll, which flaps
            if self.cfg.turns
                && s.hooked
                && (s.kind == Kind::Interactive || s.launched)
                && s.activity == Activity::YourTurn
                && s.asks.is_empty()
                && s.activity_since >= self.started_at
                && s.activity_since <= now
            {
                out.push(Note {
                    seq: format!("turn-{}-{}", s.id, s.activity_since),
                    opened_at: s.activity_since,
                    focused,
                    content: self.turn_content(s),
                });
            }
        }
        out.sort_by(|a, b| a.seq.cmp(&b.seq));
        out
    }

    fn covers(&self, s: &Session) -> bool {
        self.cfg.workspaces.is_empty()
            || s.workspace
                .as_ref()
                .is_some_and(|w| self.cfg.workspaces.contains(w))
    }

    fn wanted(&self, kind: &AskKind) -> bool {
        match kind {
            AskKind::Tool { .. } => self.cfg.permissions,
            AskKind::Question { .. } => self.cfg.questions,
            AskKind::Plan { .. } => self.cfg.plans,
        }
    }

    fn click(&self, s: &Session) -> String {
        format!("{}/sessions/{}", self.web_url, s.id)
    }

    fn ask_content(&self, s: &Session, ask: &Ask, key: Option<&str>) -> Content {
        let label = label(s);
        let mut buttons: Vec<Button> = Vec::new();
        let mut more = false;
        let button = |label: &str, action: &str| Button {
            label: truncate(label, LABEL_LEN),
            action: action.to_owned(),
        };
        let (title, body) = match &ask.kind {
            AskKind::Tool {
                tool,
                description,
                input,
                always,
            } => {
                let detail = input
                    .iter()
                    .find(|(k, _)| {
                        matches!(k.as_str(), "command" | "file_path" | "url" | "pattern")
                    })
                    .or(input.first())
                    .map(|(_, v)| v.as_str())
                    .unwrap_or_default();
                let body = [description.as_deref().unwrap_or_default(), detail]
                    .into_iter()
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                buttons.push(button("Allow", "allow"));
                buttons.push(button("Deny", "deny"));
                if always.is_some() {
                    buttons.push(button("Always", "always"));
                }
                (format!("{label}: allow {tool}?"), body)
            }
            AskKind::Question { questions } => {
                let body = questions
                    .iter()
                    .map(|q| {
                        let options = q
                            .options
                            .iter()
                            .map(|o| o.label.as_str())
                            .collect::<Vec<_>>()
                            .join(" · ");
                        format!("{}\n{options}", q.question)
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                match questions.as_slice() {
                    [q] if !q.multi_select => {
                        let fits = q.options.len() <= MAX_BUTTONS;
                        let shown = if fits { MAX_BUTTONS } else { MAX_BUTTONS - 1 };
                        for (i, o) in q.options.iter().take(shown).enumerate() {
                            buttons.push(button(&o.label, &format!("o{i}")));
                        }
                        more = !fits;
                    }
                    _ => more = true,
                }
                (format!("{label} asks"), body)
            }
            AskKind::Plan { plan } => {
                buttons.push(button("Approve", "allow"));
                buttons.push(button("Keep planning", "deny"));
                (format!("{label}: plan ready"), plan.clone())
            }
        };
        if key.is_none() {
            buttons.clear();
        }
        Content {
            title,
            body: truncate(&body, BODY_LEN),
            session: s.id.clone(),
            url: self.click(s),
            key: key.map(str::to_owned),
            buttons,
            more,
            urgent: true,
        }
    }

    fn turn_content(&self, s: &Session) -> Content {
        let body = match (&s.waiting_for, &s.reply) {
            (Some(error), _) if error.starts_with("error:") => error.clone(),
            (_, Some(reply)) if !reply.text.trim().is_empty() => reply.text.clone(),
            _ => "Claude finished.".into(),
        };
        // NOTE: a session started from the panel runs in the background: it finished
        let title = if s.launched {
            format!("Finished: {}", label(s))
        } else {
            format!("{}: your turn", label(s))
        };
        Content {
            title,
            body: truncate(&body, BODY_LEN),
            session: s.id.clone(),
            url: self.click(s),
            key: None,
            buttons: Vec::new(),
            more: false,
            urgent: false,
        }
    }
}

fn label(s: &Session) -> String {
    let label = s
        .title
        .clone()
        .or_else(|| s.name.clone())
        .unwrap_or_else(|| {
            Path::new(&s.cwd).file_name().map_or_else(
                || s.id.chars().take(8).collect(),
                |n| n.to_string_lossy().into(),
            )
        });
    truncate(&label, LABEL_LEN)
}

/// A notification backend.
pub trait Sink: Send + 'static {
    const NAME: &'static str;
    fn publish(&self, note: &Note) -> impl Future<Output = Result<()>> + Send;
    fn delete(&self, seq: &str) -> impl Future<Output = Result<()>> + Send;
}

/// How long a note waits before it shows, in seconds.
#[derive(Debug, Clone, Copy)]
pub struct Delays {
    pub normal: u64,
    pub focused: u64,
}

impl From<&Notify> for Delays {
    fn from(cfg: &Notify) -> Self {
        Self {
            normal: cfg.delay_secs,
            focused: cfg.focused_delay_secs,
        }
    }
}

/// Publishes notes to `sink` once their wait passes and deletes the ones
/// that ended.
pub async fn run<S: Sink>(delays: Delays, mut rx: watch::Receiver<Vec<Note>>, sink: S) {
    let mut shown: HashSet<String> = HashSet::new();
    let mut last_error = None;
    loop {
        let notes = rx.borrow_and_update().clone();
        let now = now_ms();
        let live: HashSet<&str> = notes.iter().map(|n| n.seq.as_str()).collect();
        let ended: Vec<String> = shown
            .iter()
            .filter(|seq| !live.contains(seq.as_str()))
            .cloned()
            .collect();
        for seq in ended {
            shown.remove(&seq);
            report::<S>(sink.delete(&seq).await, &mut last_error);
        }
        let mut next = None::<i64>;
        for note in &notes {
            if shown.contains(&note.seq) {
                continue;
            }
            let wait = if note.focused {
                delays.focused
            } else {
                delays.normal
            };
            let due = note.opened_at + 1000 * i64::try_from(wait).unwrap_or(i64::MAX / 2000);
            if due <= now {
                shown.insert(note.seq.clone());
                report::<S>(sink.publish(note).await, &mut last_error);
            } else {
                next = Some(next.map_or(due, |n| n.min(due)));
            }
        }
        let sleep = next.map_or(Duration::from_secs(3600), |due| {
            Duration::from_millis(u64::try_from(due - now_ms()).unwrap_or(0))
        });
        tokio::select! {
            changed = rx.changed() => if changed.is_err() { return },
            () = tokio::time::sleep(sleep) => {}
        }
    }
}

/// Logs each new kind of failure once.
fn report<S: Sink>(result: Result<()>, last_error: &mut Option<String>) {
    match result {
        Ok(()) => *last_error = None,
        Err(e) => {
            let message = format!("{e:#}");
            if last_error.as_ref() != Some(&message) {
                warn!("{}: {message}", S::NAME);
                *last_error = Some(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        model::{Choice, Question},
        state::Effects,
        wire::{Envelope, Source},
        workspace::Workspaces,
    };

    const SID: &str = "11111111-2222-3333-4444-555555555555";

    fn state_with_prompt(ts: i64) -> State {
        let mut st = State::new("h".into(), Vec::new(), Workspaces::default());
        let mut fx = Effects::default();
        for (event, extra) in [
            ("UserPromptSubmit", json!({"prompt": "go"})),
            (
                "PermissionRequest",
                json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
            ),
        ] {
            let mut payload =
                json!({"session_id": SID, "cwd": "/w/repo", "hook_event_name": event});
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let env = Envelope {
                v: 1,
                source: Source::Hook,
                ts,
                pid: None,
                remote_id: None,
                applied_title: None,
                launch: None,
                branch: None,
                payload,
            };
            st.apply(&env, &mut fx);
        }
        st
    }

    fn tool_ask(at: i64) -> Ask {
        Ask {
            id: "a1".into(),
            agent_id: None,
            at,
            kind: AskKind::Tool {
                tool: "Bash".into(),
                description: Some("List files".into()),
                input: vec![("command".into(), "ls".into())],
                always: Some("Bash(ls *)".into()),
            },
        }
    }

    const WEB: &str = "http://10.0.0.2:7837/";

    fn composer() -> Composer {
        Composer::new(Notify::default(), WEB.into(), 1_000)
    }

    #[test]
    fn prompts_get_buttons_with_their_key() {
        let mut st = state_with_prompt(2_000);
        assert!(st.add_ask(SID, tool_ask(2_000), &mut Effects::default()));
        let keys = HashMap::from([("a1", "k3y")]);
        let notes = composer().notes(&st, &keys, 3_000);
        let [note] = notes.as_slice() else {
            panic!("{notes:?}")
        };
        assert_eq!((note.seq.as_str(), note.opened_at), ("ask-a1", 2_000));
        let c = &note.content;
        assert_eq!(c.title, "repo: allow Bash?");
        assert_eq!(c.body, "List files\nls");
        assert_eq!(c.url, format!("http://10.0.0.2:7837/sessions/{SID}"));
        assert_eq!((c.session.as_str(), c.key.as_deref()), (SID, Some("k3y")));
        assert!(c.urgent && !c.more);
        let actions: Vec<&str> = c.buttons.iter().map(|b| b.action.as_str()).collect();
        assert_eq!(actions, ["allow", "deny", "always"]);

        // The push payload carries them for the service worker to lay out.
        let payload = serde_json::to_value(c).unwrap();
        assert_eq!(payload["key"], "k3y");
        assert_eq!(
            payload["buttons"][1],
            json!({"label": "Deny", "action": "deny"})
        );

        // Without a key there is nothing to press.
        let notes = composer().notes(&st, &HashMap::new(), 3_000);
        assert!(notes[0].content.buttons.is_empty() && notes[0].content.key.is_none());

        // Nothing from before the daemon started; nothing for turned-off kinds.
        let late = Composer::new(Notify::default(), "http://x".into(), 5_000);
        assert!(late.notes(&st, &keys, 6_000).is_empty());
        let quiet = Composer::new(
            Notify {
                permissions: false,
                ..Notify::default()
            },
            "http://x".into(),
            0,
        );
        assert!(quiet.notes(&st, &keys, 3_000).is_empty());
    }

    #[test]
    fn questions_fit_their_options_into_three_buttons() {
        let session = |options: usize, multi: bool| {
            let mut s = state_with_prompt(2_000);
            let ask = Ask {
                id: "q".into(),
                agent_id: None,
                at: 2_000,
                kind: AskKind::Question {
                    questions: vec![Question {
                        question: "Which?".into(),
                        header: "Pick".into(),
                        options: (0..options)
                            .map(|i| Choice {
                                label: format!("opt{i}"),
                                description: String::new(),
                            })
                            .collect(),
                        multi_select: multi,
                    }],
                },
            };
            assert!(s.add_ask(SID, ask, &mut Effects::default()));
            s
        };
        let labels = |options, multi| -> (Vec<String>, bool) {
            let st = session(options, multi);
            let notes = composer().notes(&st, &HashMap::from([("q", "k")]), 3_000);
            let c = &notes[0].content;
            (c.buttons.iter().map(|b| b.label.clone()).collect(), c.more)
        };
        assert_eq!(
            labels(3, false),
            (vec!["opt0".into(), "opt1".into(), "opt2".into()], false)
        );
        assert_eq!(labels(4, false), (vec!["opt0".into(), "opt1".into()], true));
        assert_eq!(labels(2, true), (Vec::new(), true));
    }

    #[test]
    fn finished_turns_notify_with_the_reply() {
        let mut st = state_with_prompt(2_000);
        let mut fx = Effects::default();
        let env = Envelope {
            v: 1,
            source: Source::Hook,
            ts: 4_000,
            pid: None,
            remote_id: None,
            applied_title: None,
            launch: None,
            branch: None,
            payload: json!({"session_id": SID, "hook_event_name": "Stop",
                "last_assistant_message": "All done."}),
        };
        st.apply(&env, &mut fx);
        let notes = composer().notes(&st, &HashMap::new(), 5_000);
        assert_eq!(notes[0].seq, format!("turn-{SID}-4000"));
        assert_eq!(notes[0].content.body, "All done.");
        assert!(!notes[0].content.urgent && notes[0].content.buttons.is_empty());

        let elsewhere = Composer::new(
            Notify {
                workspaces: vec!["work".into()],
                ..Notify::default()
            },
            "http://x".into(),
            0,
        );
        assert!(elsewhere.notes(&st, &HashMap::new(), 5_000).is_empty());
    }

    #[test]
    fn launched_background_sessions_notify_when_finished() {
        let mut st = state_with_prompt(2_000);
        let mut fx = Effects::default();
        let poll = |st: &mut State, fx: &mut Effects| {
            st.agents(
                &[crate::state::AgentEntry {
                    session_id: SID.into(),
                    kind: Some("background".into()),
                    ..Default::default()
                }],
                3_000,
                fx,
            );
        };
        poll(&mut st, &mut fx);
        let stop = Envelope {
            v: 1,
            source: Source::Hook,
            ts: 4_000,
            pid: None,
            remote_id: None,
            applied_title: None,
            launch: None,
            branch: None,
            payload: json!({"session_id": SID, "hook_event_name": "Stop",
                "last_assistant_message": "Done."}),
        };
        st.apply(&stop, &mut fx);
        assert!(composer().notes(&st, &HashMap::new(), 5_000).is_empty());
        assert!(st.set_launched(SID, None, &mut fx));
        let notes = composer().notes(&st, &HashMap::new(), 5_000);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].content.title.starts_with("Finished: "));
        assert_eq!(notes[0].content.body, "Done.");
    }
}
