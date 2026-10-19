//! Answering Claude Code's prompts from ccdash: tool approvals, questions
//! (`AskUserQuestion`) and plans (`ExitPlanMode`).
//!
//! `ccdash ask` is the PermissionRequest hook. It hands the prompt to the
//! daemon over `api.sock` and holds it open until ccdash answers (the
//! decision goes to stdout) or the daemon lets go because the prompt was
//! answered elsewhere. Claude Code keeps the terminal dialog open meanwhile
//! and applies whichever answer comes first; a late hook answer is ignored.
//! Hook output: code.claude.com/docs/en/hooks (PermissionRequest).

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;

use crate::{
    emit,
    model::{AskKind, Choice, Question},
    paths,
    util::truncate,
    wire::Source,
};

const MAX_FIELDS: usize = 12;
const FIELD_LEN: usize = 2000;
const PLAN_LEN: usize = 8000;
const ANSWER_LEN: usize = 2000;
// NOTE: Claude Code's own wording for a rejected call; Claude distrusts other tool-result text
const DENIED: &str = "The user doesn't want to proceed with this tool use. The tool use was \
    rejected. STOP what you are doing and wait for the user to tell you how to proceed.";
const DENIED_SAYING: &str = "The user doesn't want to proceed with this tool use. The tool use \
    was rejected. They answered from ccdash, their session dashboard. To tell you how to \
    proceed, the user said:\n";

/// Runs the hook: never fails Claude Code, prints nothing unless ccdash
/// answered.
pub fn run() -> i32 {
    let mut input = Vec::new();
    let _ = std::io::stdin().take(4 << 20).read_to_end(&mut input);
    let Ok(payload) = serde_json::from_slice::<Value>(&input) else {
        return 0;
    };
    if let Some(decision) = wait(payload) {
        let output = json!({
            "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision }
        });
        println!("{output}");
    }
    0
}

/// Sends the prompt and blocks until a decision or until the daemon closes
/// the connection.
fn wait(payload: Value) -> Option<Value> {
    let mut stream = UnixStream::connect(paths::api_socket()).ok()?;
    let request = json!({ "cmd": "ask", "envelope": emit::envelope(Source::Hook, payload, None) });
    let mut line = serde_json::to_vec(&request).ok()?;
    line.push(b'\n');
    stream.write_all(&line).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    let mut reply: Value = serde_json::from_str(&reply).ok()?;
    (reply["type"] == "decision").then(|| reply["decision"].take())
}

/// What ccdash shows for a PermissionRequest payload, plus what answering
/// needs: the tool input and the permission updates "always" applies.
pub fn parse(payload: &Value) -> (AskKind, Value, Vec<Value>) {
    let tool = payload["tool_name"].as_str().unwrap_or("?");
    let input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let (kind, updates) = match tool {
        "AskUserQuestion" => (
            AskKind::Question {
                questions: questions(&input),
            },
            Vec::new(),
        ),
        "ExitPlanMode" => (
            AskKind::Plan {
                plan: truncate(input["plan"].as_str().unwrap_or_default(), PLAN_LEN),
            },
            Vec::new(),
        ),
        _ => {
            let (always, updates) = always(&payload["permission_suggestions"]);
            let kind = AskKind::Tool {
                tool: tool.to_owned(),
                description: input["description"].as_str().map(str::to_owned),
                input: fields(&input),
                always,
            };
            (kind, updates)
        }
    };
    (kind, input, updates)
}

fn questions(input: &Value) -> Vec<Question> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| Question {
            question: q["question"].as_str().unwrap_or_default().to_owned(),
            header: q["header"].as_str().unwrap_or_default().to_owned(),
            options: q["options"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|o| Choice {
                    label: o["label"].as_str().unwrap_or_default().to_owned(),
                    description: o["description"].as_str().unwrap_or_default().to_owned(),
                })
                .collect(),
            multi_select: q["multiSelect"].as_bool().unwrap_or(false),
        })
        .collect()
}

/// The tool's arguments as display text, one entry per top-level field.
fn fields(input: &Value) -> Vec<(String, String)> {
    let Some(map) = input.as_object() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(key, _)| *key != "description")
        .take(MAX_FIELDS)
        .map(|(key, value)| {
            let text = match value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (key.clone(), truncate(&text, FIELD_LEN))
        })
        .collect()
}

/// What "always allow" allows, and the permission updates it applies: the
/// suggested allow rules (with any suggested directories), else a
/// suggested switch to accepting edits.
fn always(suggestions: &Value) -> (Option<String>, Vec<Value>) {
    let list = suggestions
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let rules: Vec<&Value> = list
        .iter()
        .filter(|s| s["type"] == "addRules" && s["behavior"] == "allow")
        .collect();
    if !rules.is_empty() {
        let label = rules
            .iter()
            .flat_map(|s| s["rules"].as_array().into_iter().flatten())
            .filter_map(|r| {
                let tool = r["toolName"].as_str()?;
                Some(match r["ruleContent"].as_str() {
                    Some(content) => format!("{tool}({content})"),
                    None => tool.to_owned(),
                })
            })
            .collect::<Vec<_>>()
            .join(", ");
        let updates = list
            .iter()
            .filter(|s| s["type"] == "addDirectories" || rules.contains(s))
            .cloned()
            .collect();
        return (Some(label).filter(|l| !l.is_empty()), updates);
    }
    match list
        .iter()
        .find(|s| s["type"] == "setMode" && s["mode"] == "acceptEdits")
    {
        Some(mode) => (Some("all edits this session".into()), vec![mode.clone()]),
        None => (None, Vec::new()),
    }
}

/// An answer from the web UI, the CLI or a notification button.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "types.ts")]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Answer {
    /// Allows the tool call or approves the plan.
    Allow,
    /// Without a message Claude stops and waits, like the terminal's No;
    /// with one it carries on with that feedback.
    Deny {
        #[serde(default)]
        #[ts(optional)]
        message: Option<String>,
    },
    /// Allows the call and applies the suggested permission updates.
    Always,
    /// Question text → chosen label(s) or free text; multi-select labels
    /// joined with ", ".
    #[serde(rename = "answer")]
    Choose { answers: BTreeMap<String, String> },
}

impl Answer {
    /// The notification button `action` on an ask: `allow`, `deny`,
    /// `always`, or `o<N>` for option N of a single question.
    pub fn from_button(kind: &AskKind, action: &str) -> Option<Self> {
        match action {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny { message: None }),
            "always" => Some(Self::Always),
            _ => {
                let n: usize = action.strip_prefix('o')?.parse().ok()?;
                let AskKind::Question { questions } = kind else {
                    return None;
                };
                let [q] = questions.as_slice() else {
                    return None;
                };
                let label = &q.options.get(n)?.label;
                Some(Self::Choose {
                    answers: BTreeMap::from([(q.question.clone(), label.clone())]),
                })
            }
        }
    }

    /// A denial without feedback stops Claude: the turn ends.
    pub fn interrupts(&self) -> bool {
        matches!(self, Self::Deny { message } if message.as_deref().is_none_or(|m| m.trim().is_empty()))
    }

    /// Short name for the stored `answered` event.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny { .. } => "deny",
            Self::Always => "always",
            Self::Choose { .. } => "answer",
        }
    }
}

/// The PermissionRequest `decision` for `answer`, or why it doesn't fit
/// the ask.
pub fn decision(
    kind: &AskKind,
    input: &Value,
    updates: &[Value],
    answer: &Answer,
) -> Result<Value, String> {
    if let Answer::Deny { message } = answer {
        return Ok(match message.as_deref().map(str::trim) {
            Some(m) if !m.is_empty() => json!({
                "behavior": "deny",
                "message": format!("{DENIED_SAYING}{}", truncate(m, ANSWER_LEN)),
            }),
            _ => json!({ "behavior": "deny", "message": DENIED, "interrupt": true }),
        });
    }
    match (kind, answer) {
        (AskKind::Tool { .. }, Answer::Allow) => Ok(json!({ "behavior": "allow" })),
        (AskKind::Tool { .. }, Answer::Always) if !updates.is_empty() => {
            Ok(json!({ "behavior": "allow", "updatedPermissions": updates }))
        }
        (AskKind::Tool { .. }, Answer::Always) => Err("this prompt offers no always-allow".into()),
        // NOTE: ExitPlanMode needs updatedInput paired with allow
        (AskKind::Plan { .. }, Answer::Allow) => {
            Ok(json!({ "behavior": "allow", "updatedInput": input }))
        }
        (AskKind::Question { questions }, Answer::Choose { answers }) => {
            check_answers(questions, answers)?;
            let mut updated = input.clone();
            updated["answers"] = json!(answers);
            Ok(json!({ "behavior": "allow", "updatedInput": updated }))
        }
        (AskKind::Question { .. }, _) => Err("answer the questions".into()),
        _ => Err("that answer doesn't fit this prompt".into()),
    }
}

fn check_answers(questions: &[Question], answers: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(extra) = answers
        .keys()
        .find(|k| !questions.iter().any(|q| &q.question == *k))
    {
        return Err(format!("no question {extra:?}"));
    }
    for q in questions {
        match answers.get(&q.question).map(|a| a.trim()) {
            None | Some("") => return Err(format!("{:?} has no answer", q.question)),
            Some(a) if a.len() > ANSWER_LEN => {
                return Err(format!("answers are at most {ANSWER_LEN} bytes"));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_payload() -> Value {
        json!({
            "tool_name": "Bash",
            "tool_input": {"command": "git push origin main", "description": "Push the branch"},
            "permission_suggestions": [
                {
                    "type": "addRules",
                    "rules": [{"toolName": "Bash", "ruleContent": "git push *"}],
                    "behavior": "allow",
                    "destination": "localSettings"
                },
                {"type": "addDirectories", "directories": ["/w"], "destination": "session"},
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}
            ]
        })
    }

    #[test]
    fn tool_asks_offer_the_suggested_rules() {
        let (kind, input, updates) = parse(&bash_payload());
        assert_eq!(
            kind,
            AskKind::Tool {
                tool: "Bash".into(),
                description: Some("Push the branch".into()),
                input: vec![("command".into(), "git push origin main".into())],
                always: Some("Bash(git push *)".into()),
            }
        );
        assert_eq!(updates.len(), 2, "rules and directories, not the mode");
        assert_eq!(
            decision(&kind, &input, &updates, &Answer::Always).unwrap(),
            json!({"behavior": "allow", "updatedPermissions": updates})
        );
        assert_eq!(
            decision(&kind, &input, &updates, &Answer::Allow).unwrap(),
            json!({"behavior": "allow"})
        );
    }

    #[test]
    fn edits_fall_back_to_accepting_edits() {
        let payload = json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": "/w/a.rs"},
            "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]
        });
        let (kind, input, updates) = parse(&payload);
        assert!(
            matches!(&kind, AskKind::Tool { always: Some(a), .. } if a == "all edits this session")
        );
        assert!(decision(&kind, &input, &updates, &Answer::Always).is_ok());

        let bare = json!({"tool_name": "WebFetch", "tool_input": {"url": "https://x"}});
        let (kind, input, updates) = parse(&bare);
        assert!(decision(&kind, &input, &updates, &Answer::Always).is_err());
    }

    #[test]
    fn denials_stop_claude_unless_they_carry_feedback() {
        let (kind, input, updates) = parse(&bash_payload());
        let plain = decision(&kind, &input, &updates, &Answer::Deny { message: None }).unwrap();
        assert_eq!(plain["interrupt"], true);
        let told = Answer::Deny {
            message: Some(" push to a branch instead ".into()),
        };
        let d = decision(&kind, &input, &updates, &told).unwrap();
        assert!(d.get("interrupt").is_none());
        assert!(
            d["message"]
                .as_str()
                .unwrap()
                .ends_with("the user said:\npush to a branch instead")
        );
    }

    #[test]
    fn questions_need_every_answer() {
        let payload = json!({
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {
                    "question": "Which colors?",
                    "header": "Colors",
                    "multiSelect": true,
                    "options": [{"label": "Red", "description": "r"}, {"label": "Blue", "description": "b"}]
                },
                {
                    "question": "Which shape?",
                    "header": "Shape",
                    "options": [{"label": "Circle", "description": "c"}]
                }
            ]}
        });
        let (kind, input, updates) = parse(&payload);
        let AskKind::Question { questions } = &kind else {
            panic!("{kind:?}")
        };
        assert!(questions[0].multi_select && !questions[1].multi_select);
        let answers = |pairs: &[(&str, &str)]| Answer::Choose {
            answers: pairs
                .iter()
                .map(|(q, a)| ((*q).to_owned(), (*a).to_owned()))
                .collect(),
        };
        assert!(
            decision(
                &kind,
                &input,
                &updates,
                &answers(&[("Which colors?", "Red")])
            )
            .is_err()
        );
        assert!(decision(&kind, &input, &updates, &Answer::Allow).is_err());
        let full = answers(&[
            ("Which colors?", "Red, Blue"),
            ("Which shape?", "a hexagon"),
        ]);
        let d = decision(&kind, &input, &updates, &full).unwrap();
        assert_eq!(d["behavior"], "allow");
        assert_eq!(d["updatedInput"]["answers"]["Which shape?"], "a hexagon");
        assert_eq!(d["updatedInput"]["questions"], input["questions"]);
        // Buttons only answer a lone question.
        assert_eq!(Answer::from_button(&kind, "o0"), None);
    }

    #[test]
    fn buttons_pick_options_and_plans_need_their_input() {
        let one = AskKind::Question {
            questions: vec![Question {
                question: "Which color?".into(),
                header: "Color".into(),
                options: vec![
                    Choice {
                        label: "Red".into(),
                        description: String::new(),
                    },
                    Choice {
                        label: "Blue".into(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
            }],
        };
        assert_eq!(
            Answer::from_button(&one, "o1"),
            Some(Answer::Choose {
                answers: BTreeMap::from([("Which color?".into(), "Blue".into())])
            })
        );
        assert_eq!(Answer::from_button(&one, "o2"), None);
        assert_eq!(Answer::from_button(&one, "nope"), None);

        let (plan, input, updates) =
            parse(&json!({"tool_name": "ExitPlanMode", "tool_input": {"plan": "1. do it"}}));
        assert_eq!(
            plan,
            AskKind::Plan {
                plan: "1. do it".into()
            }
        );
        assert_eq!(
            decision(&plan, &input, &updates, &Answer::Allow).unwrap(),
            json!({"behavior": "allow", "updatedInput": {"plan": "1. do it"}})
        );
    }
}
