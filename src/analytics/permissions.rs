//! What asks you for permission, grouped by the allow rule that would cover
//! it, and how you answered. Hooks don't report the answer: a call of the
//! asked tool right after counts as allowed, anything else as denied.
//! Questions (`AskUserQuestion`) and plan approvals are counted apart.

use std::collections::HashMap;

use serde::Serialize;
use ts_rs::TS;

use super::{SessionEvents, percentile};
use crate::{command, history::Transition, model::Activity};

const TOP_RULES: usize = 20;

#[derive(Debug, Default, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Permissions {
    pub asks: u32,
    pub allowed: u32,
    pub denied: u32,
    pub median_answer_ms: Option<i64>,
    pub questions: u32,
    pub median_question_ms: Option<i64>,
    pub plans: u32,
    pub plans_approved: u32,
    /// Tool calls auto mode refused, by tool.
    pub blocked: Vec<(String, u32)>,
    /// Most asked first.
    pub rules: Vec<RuleStat>,
}

#[derive(Debug, Default, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct RuleStat {
    /// An allow rule in settings.json syntax.
    pub rule: String,
    pub tool: String,
    pub asks: u32,
    pub allowed: u32,
    pub denied: u32,
    pub median_answer_ms: Option<i64>,
    pub last_at: i64,
    /// What the last ask was about.
    pub example: String,
}

struct Ask {
    ts: i64,
    tool: String,
    summary: String,
    answered_at: Option<i64>,
}

#[derive(Default)]
struct Tally {
    out: Permissions,
    answers: Vec<i64>,
    question_answers: Vec<i64>,
    rules: HashMap<String, (RuleStat, Vec<i64>)>,
    blocked: HashMap<String, u32>,
}

pub fn permissions(sessions: &[SessionEvents]) -> Permissions {
    let mut tally = Tally::default();
    for session in sessions {
        let mut pending: Option<Ask> = None;
        for ev in &session.events {
            match ev.kind.as_str() {
                "permission" => {
                    if let Some(ask) = pending.take() {
                        tally.finish(ask, false);
                    }
                    pending = Some(Ask {
                        ts: ev.ts,
                        tool: ev.data["tool"].as_str().unwrap_or_default().to_owned(),
                        summary: ev.data["summary"].as_str().unwrap_or_default().to_owned(),
                        answered_at: None,
                    });
                }
                "activity" => {
                    if let Some(ask) = &mut pending
                        && serde_json::from_value::<Transition>(ev.data.clone())
                            .is_ok_and(|t| t.from == Activity::Permission)
                    {
                        ask.answered_at.get_or_insert(ev.ts);
                    }
                }
                "tool" => {
                    if pending
                        .as_ref()
                        .is_some_and(|a| ev.data["name"].as_str() == Some(a.tool.as_str()))
                        && let Some(ask) = pending.take()
                    {
                        tally.finish(ask, true);
                    }
                }
                "prompt" | "reply" | "error" => {
                    if let Some(ask) = pending.take() {
                        tally.finish(ask, false);
                    }
                }
                "denied" => {
                    let tool = ev.data["tool"].as_str().unwrap_or("?").to_owned();
                    *tally.blocked.entry(tool).or_default() += 1;
                }
                _ => {}
            }
        }
        // NOTE: an ask nobody answered yet may still be open
        if let Some(ask) = pending.filter(|a| a.answered_at.is_some()) {
            tally.finish(ask, false);
        }
    }
    tally.done()
}

impl Tally {
    fn finish(&mut self, ask: Ask, allowed: bool) {
        let answer = ask.answered_at.map(|at| at - ask.ts);
        match ask.tool.as_str() {
            "AskUserQuestion" => {
                self.out.questions += 1;
                self.question_answers.extend(answer);
            }
            "ExitPlanMode" => {
                self.out.plans += 1;
                self.out.plans_approved += u32::from(allowed);
            }
            tool => {
                self.out.asks += 1;
                if allowed {
                    self.out.allowed += 1;
                } else {
                    self.out.denied += 1;
                }
                self.answers.extend(answer);
                let rule = rule(tool, &ask.summary);
                let (stat, answers) = self.rules.entry(rule.clone()).or_insert_with(|| {
                    (
                        RuleStat {
                            rule,
                            tool: tool.to_owned(),
                            ..RuleStat::default()
                        },
                        Vec::new(),
                    )
                });
                stat.asks += 1;
                if allowed {
                    stat.allowed += 1;
                } else {
                    stat.denied += 1;
                }
                if ask.ts >= stat.last_at {
                    stat.last_at = ask.ts;
                    stat.example = ask.summary;
                }
                answers.extend(answer);
            }
        }
    }

    fn done(mut self) -> Permissions {
        self.answers.sort_unstable();
        self.question_answers.sort_unstable();
        self.out.median_answer_ms = percentile(&self.answers, 50);
        self.out.median_question_ms = percentile(&self.question_answers, 50);
        let mut rules: Vec<RuleStat> = self
            .rules
            .into_values()
            .map(|(mut stat, mut answers)| {
                answers.sort_unstable();
                stat.median_answer_ms = percentile(&answers, 50);
                stat
            })
            .collect();
        rules.sort_by(|a, b| b.asks.cmp(&a.asks).then_with(|| b.last_at.cmp(&a.last_at)));
        rules.truncate(TOP_RULES);
        self.out.rules = rules;
        let mut blocked: Vec<(String, u32)> = self.blocked.into_iter().collect();
        blocked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        self.out.blocked = blocked;
        self.out
    }
}

/// The allow rule covering `tool` called on `summary`
/// (code.claude.com/docs/en/permissions).
fn rule(tool: &str, summary: &str) -> String {
    match tool {
        "Bash" => {
            let command = command::parse(summary);
            if command.prefix.is_empty() {
                "Bash".to_owned()
            } else {
                command.rule()
            }
        }
        // NOTE: Write and NotebookEdit are checked against Edit rules
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => format!("Edit({})", path_rule(summary)),
        "Read" => format!("Read({})", path_rule(summary)),
        "WebFetch" => host(summary).map_or_else(
            || "WebFetch".to_owned(),
            |host| format!("WebFetch(domain:{host})"),
        ),
        tool => tool.to_owned(),
    }
}

/// Files under the path's directory: relative paths anchor at the working
/// directory, absolute ones take a leading `//`.
fn path_rule(path: &str) -> String {
    let dir = path.rsplit_once('/').map(|(dir, _)| dir);
    match (path.starts_with('/'), dir) {
        (true, Some(dir)) => format!("/{dir}/**"),
        (false, Some(dir)) => format!("{dir}/**"),
        (_, None) => path.to_owned(),
    }
}

fn host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::analytics::Ev;

    fn ev(ts: i64, kind: &str, data: serde_json::Value) -> Ev {
        Ev {
            ts,
            kind: kind.into(),
            data,
        }
    }

    #[test]
    fn rules_follow_claude_code_syntax() {
        assert_eq!(rule("Bash", "git push origin main"), "Bash(git push *)");
        assert_eq!(rule("Write", "src/api/http.rs"), "Edit(src/api/**)");
        assert_eq!(rule("Edit", "/tmp/scratch/a.txt"), "Edit(//tmp/scratch/**)");
        assert_eq!(rule("Edit", "README.md"), "Edit(README.md)");
        assert_eq!(
            rule("WebFetch", "https://docs.rs/axum/latest"),
            "WebFetch(domain:docs.rs)"
        );
        assert_eq!(rule("mcp__hm__fetch", ""), "mcp__hm__fetch");
    }

    #[test]
    fn answers_are_inferred_from_what_follows() {
        let perm = |ts, tool: &str, summary: &str| {
            ev(ts, "permission", json!({"tool": tool, "summary": summary}))
        };
        let left = |ts| {
            ev(
                ts,
                "activity",
                json!({"from": "permission", "to": "working"}),
            )
        };
        let session = SessionEvents {
            id: "a".into(),
            host: "h".into(),
            cwd: "/w".into(),
            label: "a".into(),
            events: vec![
                perm(0, "Bash", "git push origin main"),
                left(4_000),
                ev(5_000, "tool", json!({"name": "Bash", "ok": true})),
                perm(10_000, "Bash", "git push --force"),
                left(12_000),
                ev(13_000, "reply", json!({})),
                perm(20_000, "AskUserQuestion", ""),
                left(50_000),
                ev(60_000, "denied", json!({"tool": "Bash"})),
                perm(70_000, "Bash", "rm -rf /"),
            ],
        };
        let p = permissions(&[session]);
        assert_eq!((p.asks, p.allowed, p.denied), (2, 1, 1));
        assert_eq!((p.questions, p.median_question_ms), (1, Some(30_000)));
        assert_eq!(p.rules[0].rule, "Bash(git push *)");
        assert_eq!(p.rules[0].median_answer_ms, Some(4_000));
        assert_eq!(p.rules[0].example, "git push --force");
        assert_eq!(p.blocked, [("Bash".to_owned(), 1)]);
    }
}
