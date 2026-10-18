//! Phone notifications through ntfy (docs.ntfy.sh/publish). An open prompt
//! or a finished turn notifies after a short wait unless it ends first,
//! and its notification is deleted once it ends. Prompt notifications carry
//! buttons that answer through `/act/{key}/{action}` on this daemon, so the
//! phone (or a watch mirroring it) answers without opening anything.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::watch;
use tracing::warn;

use crate::{
    config::Notify,
    model::{Activity, Ask, AskKind, Kind, Session},
    state::State,
    util::{expand_env, now_ms, truncate},
};

const BODY_LEN: usize = 400;
const LABEL_LEN: usize = 40;
/// ntfy shows at most three buttons.
const MAX_BUTTONS: usize = 3;

/// A notification that is due once its wait passes.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// ntfy sequence id; the delete addresses it.
    pub seq: String,
    pub opened_at: i64,
    /// The session's window has keyboard focus.
    pub focused: bool,
    /// The ntfy JSON message without `topic` and `sequence_id`.
    pub message: Value,
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
                    message: self.ask_message(s, ask, key),
                });
            }
            // NOTE: hooked only: unhooked turns come from the agents poll, which flaps
            if self.cfg.turns
                && s.hooked
                && s.kind == Kind::Interactive
                && s.activity == Activity::YourTurn
                && s.asks.is_empty()
                && s.activity_since >= self.started_at
                && s.activity_since <= now
            {
                out.push(Note {
                    seq: format!("turn-{}-{}", s.id, s.activity_since),
                    opened_at: s.activity_since,
                    focused,
                    message: self.turn_message(s),
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
        format!("{}/#session={}", self.web_url, s.id)
    }

    fn button(&self, key: &str, label: &str, action: &str) -> Value {
        json!({
            "action": "http",
            "label": truncate(label, LABEL_LEN),
            "url": format!("{}/act/{key}/{action}", self.web_url),
            "method": "POST",
            "clear": true,
        })
    }

    fn ask_message(&self, s: &Session, ask: &Ask, key: Option<&str>) -> Value {
        let label = label(s);
        let mut buttons: Vec<(String, String)> = Vec::new();
        let mut more = false;
        let (title, body, tag) = match &ask.kind {
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
                buttons.push(("Allow".into(), "allow".into()));
                buttons.push(("Deny".into(), "deny".into()));
                if always.is_some() {
                    buttons.push(("Always".into(), "always".into()));
                }
                (format!("{label}: allow {tool}?"), body, "key")
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
                            buttons.push((o.label.clone(), format!("o{i}")));
                        }
                        more = !fits;
                    }
                    _ => more = true,
                }
                (format!("{label} asks"), body, "question")
            }
            AskKind::Plan { plan } => {
                buttons.push(("Approve".into(), "allow".into()));
                buttons.push(("Keep planning".into(), "deny".into()));
                (format!("{label}: plan ready"), plan.clone(), "clipboard")
            }
        };
        let mut actions: Vec<Value> = match key {
            Some(key) => buttons
                .iter()
                .map(|(text, action)| self.button(key, text, action))
                .collect(),
            None => Vec::new(),
        };
        if more {
            actions.push(json!({
                "action": "view",
                "label": "Answer",
                "url": self.click(s),
                "clear": true,
            }));
        }
        json!({
            "title": title,
            "message": truncate(&body, BODY_LEN),
            "priority": 4,
            "tags": [tag],
            "click": self.click(s),
            "actions": actions,
        })
    }

    fn turn_message(&self, s: &Session) -> Value {
        let body = match (&s.waiting_for, &s.reply) {
            (Some(error), _) if error.starts_with("error:") => error.clone(),
            (_, Some(reply)) if !reply.text.trim().is_empty() => reply.text.clone(),
            _ => "Claude finished.".into(),
        };
        json!({
            "title": format!("{}: your turn", label(s)),
            "message": truncate(&body, BODY_LEN),
            "priority": 3,
            "tags": ["speech_balloon"],
            "click": self.click(s),
        })
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

/// Publishes notes once their wait passes and deletes the ones that ended.
pub async fn run(cfg: Notify, mut rx: watch::Receiver<Vec<Note>>) {
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
            report(delete(&cfg, &seq).await, &mut last_error);
        }
        let mut next = None::<i64>;
        for note in &notes {
            if shown.contains(&note.seq) {
                continue;
            }
            let wait = if note.focused {
                cfg.focused_delay_secs
            } else {
                cfg.delay_secs
            };
            let due = note.opened_at + 1000 * i64::try_from(wait).unwrap_or(i64::MAX / 2000);
            if due <= now {
                shown.insert(note.seq.clone());
                report(publish(&cfg, note).await, &mut last_error);
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
fn report(result: Result<()>, last_error: &mut Option<String>) {
    match result {
        Ok(()) => *last_error = None,
        Err(e) => {
            let message = format!("{e:#}");
            if last_error.as_ref() != Some(&message) {
                warn!("ntfy: {message}");
                *last_error = Some(message);
            }
        }
    }
}

async fn publish(cfg: &Notify, note: &Note) -> Result<()> {
    let mut body = note.message.clone();
    body["topic"] = cfg.topic.clone().into();
    body["sequence_id"] = note.seq.clone().into();
    let (url, token) = (cfg.url.trim_end_matches('/').to_owned(), token(cfg)?);
    blocking(move || {
        let mut request = agent()
            .post(&url)
            .header("Content-Type", "application/json");
        if let Some(token) = &token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        request.send(body.to_string()).map(drop)
    })
    .await
    .context("publishing")
}

async fn delete(cfg: &Notify, seq: &str) -> Result<()> {
    let url = format!("{}/{}/{seq}", cfg.url.trim_end_matches('/'), cfg.topic);
    let token = token(cfg)?;
    blocking(move || {
        let mut request = agent().delete(&url);
        if let Some(token) = &token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        request.call().map(drop)
    })
    .await
    .context("deleting")
}

async fn blocking(call: impl FnOnce() -> Result<(), ureq::Error> + Send + 'static) -> Result<()> {
    Ok(tokio::task::spawn_blocking(call).await??)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into()
}

/// Read at every call, so a rotated token needs no restart.
fn token(cfg: &Notify) -> Result<Option<String>> {
    let Some(path) = &cfg.token_file else {
        return Ok(None);
    };
    let path = PathBuf::from(expand_env(&path.to_string_lossy()));
    let token =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("{} is empty", path.display());
    }
    Ok(Some(token.to_owned()))
}

#[cfg(test)]
mod tests {
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

    fn composer() -> Composer {
        Composer::new(Notify::default(), "http://10.0.0.2:7837/".into(), 1_000)
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
        let m = &note.message;
        assert_eq!(m["title"], "repo: allow Bash?");
        assert_eq!(m["message"], "List files\nls");
        assert_eq!(m["click"], format!("http://10.0.0.2:7837/#session={SID}"));
        let urls: Vec<&str> = m["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            [
                "http://10.0.0.2:7837/act/k3y/allow",
                "http://10.0.0.2:7837/act/k3y/deny",
                "http://10.0.0.2:7837/act/k3y/always",
            ]
        );

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
        let labels = |options, multi| -> Vec<String> {
            let st = session(options, multi);
            let notes = composer().notes(&st, &HashMap::from([("q", "k")]), 3_000);
            notes[0].message["actions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["label"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(labels(3, false), ["opt0", "opt1", "opt2"]);
        assert_eq!(labels(4, false), ["opt0", "opt1", "Answer"]);
        assert_eq!(labels(2, true), ["Answer"]);
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
            payload: json!({"session_id": SID, "hook_event_name": "Stop",
                "last_assistant_message": "All done."}),
        };
        st.apply(&env, &mut fx);
        let notes = composer().notes(&st, &HashMap::new(), 5_000);
        assert_eq!(notes[0].seq, format!("turn-{SID}-4000"));
        assert_eq!(notes[0].message["message"], "All done.");
        assert!(notes[0].message.get("actions").is_none());

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

    /// Serves one canned response and returns the raw request.
    fn capture() -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut buf = vec![0u8; 8192];
            let mut got = Vec::new();
            while let Ok(n) = conn.read(&mut buf) {
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        break;
                    }
                }
            }
            conn.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });
        (url, handle)
    }

    #[tokio::test]
    async fn publishes_and_deletes_with_the_token() {
        let dir = std::env::temp_dir().join(format!("ccdash-notify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let token_file = dir.join("token");
        std::fs::write(&token_file, "tk_test\n").unwrap();
        let (url, server) = capture();
        let cfg = Notify {
            url: url.clone(),
            topic: "t".into(),
            token_file: Some(token_file),
            ..Notify::default()
        };
        let note = Note {
            seq: "ask-1".into(),
            opened_at: 0,
            focused: false,
            message: json!({"title": "x"}),
        };
        publish(&cfg, &note).await.unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST / HTTP/1.1"), "{request}");
        assert!(
            request.contains("authorization: Bearer tk_test")
                || request.contains("Authorization: Bearer tk_test")
        );
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({"title": "x", "topic": "t", "sequence_id": "ask-1"})
        );

        let (url, server) = capture();
        let cfg = Notify { url, ..cfg };
        delete(&cfg, "ask-1").await.unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("DELETE /t/ask-1 HTTP/1.1"), "{request}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
