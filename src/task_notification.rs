//! Claude Code's task notifications: what it hands a session, through the
//! prompt hook as if you had typed it, when a background task reports (a
//! subagent finished, a background command exited, a monitor saw an event,
//! an MCP task completed). The hub records them apart from prompts and the
//! panel shows them as what they are.

use serde::Serialize;
use ts_rs::TS;

use crate::markdown;

/// The event kind they are recorded as.
pub const KIND: &str = "task_notification";

const OPEN: &str = "<task-notification>";

/// Words that follow the quoted name in a summary (`Agent "x" finished`).
const VERBS: [&str; 6] = [
    "finished",
    "failed",
    "completed",
    "is ",
    "was ",
    "stream ended",
];

/// A task notification as the panel shows it.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct TaskNotification {
    /// What reported: `agent` (a subagent), `shell` (a background command),
    /// `monitor`, `mcp` (an MCP task), `workflow`, else `task`.
    pub source: String,
    /// Its name as Claude Code quotes it: the subagent's, the command's or
    /// the monitor's description; an MCP task's tool.
    pub name: Option<String>,
    /// `completed`, `failed`, `stopped` or `killed`; none for a monitor's
    /// event.
    pub status: Option<String>,
    /// Claude Code's summary after the name (`finished`, `completed (exit
    /// code 0)`); all of it when it names nothing.
    pub summary: String,
    /// The subagent's report or the MCP task's result, as HTML.
    pub html: Option<String>,
    /// A monitor's event.
    pub event: Option<String>,
    /// A subagent's tool calls and run time.
    pub tools: Option<u64>,
    pub ms: Option<u64>,
}

/// Whether a prompt is a task notification.
pub fn is(prompt: &str) -> bool {
    prompt.trim_start().starts_with(OPEN)
}

/// Reads a task notification; `None` when `text` is not one. A text cut
/// short reads up to its end.
///
/// ```ignore
/// let n = parse("<task-notification>\n<status>completed</status>\n<summary>Agent \"Map the API\" finished</summary>\n</task-notification>").unwrap();
/// assert_eq!((n.source.as_str(), n.name.as_deref(), n.summary.as_str()), ("agent", Some("Map the API"), "finished"));
/// ```
pub fn parse(text: &str) -> Option<TaskNotification> {
    let body = text.trim_start().strip_prefix(OPEN)?;
    let (source, name, summary) = classify(tag(body, "summary").unwrap_or_default().trim());
    // NOTE: after the result, which may quote any tag
    let usage = body.rfind("<usage>").map_or("", |at| &body[at..]);
    let number = |name| tag(usage, name).and_then(|v| v.trim().parse().ok());
    Some(TaskNotification {
        source: source.to_owned(),
        name,
        status: filled(tag(body, "status")).map(str::to_owned),
        summary,
        html: filled(outer(body, "result")).map(markdown::render),
        event: filled(outer(body, "event")).map(str::to_owned),
        tools: number("tool_uses"),
        ms: number("duration_ms"),
    })
}

/// Trimmed, and none when empty.
fn filled(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// The text of the first `<name>…</name>`, to the end when it is not closed.
fn tag<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let rest = &body[body.find(&format!("<{name}>"))? + name.len() + 2..];
    Some(
        rest.find(&format!("</{name}>"))
            .map_or(rest, |end| &rest[..end]),
    )
}

/// As `tag`, up to the last closing tag: a result can contain the tags.
fn outer<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let rest = &body[body.find(&format!("<{name}>"))? + name.len() + 2..];
    Some(
        rest.rfind(&format!("</{name}>"))
            .map_or(rest, |end| &rest[..end]),
    )
}

/// The source, the name and the rest of a summary.
fn classify(summary: &str) -> (&'static str, Option<String>, String) {
    const QUOTED: [(&str, &str); 5] = [
        ("Agent \"", "agent"),
        ("Background command \"", "shell"),
        ("Monitor event: \"", "monitor"),
        ("Monitor \"", "monitor"),
        ("Dynamic workflow \"", "workflow"),
    ];
    for (prefix, source) in QUOTED {
        if let Some((name, rest)) = summary.strip_prefix(prefix).and_then(quoted) {
            return (source, Some(name.to_owned()), tidy(rest));
        }
    }
    if let Some((tool, rest)) = summary
        .strip_prefix("MCP task ")
        .and_then(|s| s.split_once(" ("))
        .and_then(|(_, s)| s.split_once(')'))
    {
        let tool = tool.rsplit(':').next().unwrap_or(tool);
        return ("mcp", Some(tool.to_owned()), tidy(rest));
    }
    let lower = summary.to_lowercase();
    let source = if lower.contains("agent") {
        "agent"
    } else if lower.contains("shell command") {
        "shell"
    } else {
        "task"
    };
    (source, None, summary.to_owned())
}

/// Splits `name" rest` at the quote that ends the name: the first one
/// followed by the end or by one of `VERBS` (a name can hold quotes).
fn quoted(after_open: &str) -> Option<(&str, &str)> {
    after_open
        .match_indices('"')
        .map(|(at, _)| at)
        .find(|&at| {
            let rest = &after_open[at + 1..];
            rest.is_empty()
                || rest
                    .strip_prefix(' ')
                    .is_some_and(|r| VERBS.iter().any(|v| r.starts_with(v)))
        })
        .map(|at| (&after_open[..at], &after_open[at + 1..]))
}

fn tidy(rest: &str) -> String {
    rest.trim().trim_end_matches('.').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(inner: &str) -> String {
        format!("<task-notification>\n{inner}\n</task-notification>")
    }

    #[test]
    fn subagent_with_report_and_usage() {
        let n = parse(&note(concat!(
            "<task-id>a1</task-id>\n<status>completed</status>\n",
            "<summary>Agent \"Survey the claude files\" finished</summary>\n",
            "<note>internal</note>\n<result>**Found** 3 files</result>\n",
            "<usage><subagent_tokens>9</subagent_tokens><tool_uses>31</tool_uses>",
            "<duration_ms>195897</duration_ms></usage>",
        )))
        .unwrap();
        assert_eq!(
            n,
            TaskNotification {
                source: "agent".into(),
                name: Some("Survey the claude files".into()),
                status: Some("completed".into()),
                summary: "finished".into(),
                html: Some("<p><strong>Found</strong> 3 files</p>\n".into()),
                event: None,
                tools: Some(31),
                ms: Some(195_897),
            }
        );
    }

    #[test]
    fn summaries_of_every_source() {
        let read = |summary: &str| {
            let n = parse(&note(&format!("<summary>{summary}</summary>"))).unwrap();
            (n.source, n.name, n.summary)
        };
        let named = |source: &str, name: &str, summary: &str| {
            (source.to_owned(), Some(name.to_owned()), summary.to_owned())
        };
        assert_eq!(
            read("Background command \"Run the \"slow\" tests\" completed (exit code 0)"),
            named("shell", "Run the \"slow\" tests", "completed (exit code 0)")
        );
        assert_eq!(
            read("Agent \"Write pages\" failed: API error \"x\" was raised"),
            named("agent", "Write pages", "failed: API error \"x\" was raised")
        );
        assert_eq!(
            read("Monitor event: \"new lines in requests.md\""),
            named("monitor", "new lines in requests.md", "")
        );
        assert_eq!(
            read("Monitor \"DNS switch\" stream ended"),
            named("monitor", "DNS switch", "stream ended")
        );
        assert_eq!(
            read("MCP task k1 (plugin:hm:playwright/browser_run_code) completed."),
            named("mcp", "playwright/browser_run_code", "completed")
        );
        let orphan =
            "3 background shell command tasks didn't finish before the previous session ended.";
        assert_eq!(read(orphan), ("shell".to_owned(), None, orphan.to_owned()));
    }

    #[test]
    fn a_result_quoting_the_tags_and_a_cut_text() {
        let n = parse(&note(concat!(
            "<status>completed</status>\n<summary>MCP task k (t) completed.</summary>\n",
            "<result>says </result> and <usage> inside</result>",
        )))
        .unwrap();
        assert_eq!(
            n.html.as_deref(),
            Some("<p>says &lt;/result&gt; and &lt;usage&gt; inside</p>\n")
        );
        assert_eq!(n.tools, None);

        let cut = parse("<task-notification>\n<summary>Monitor event: \"log\"</summary>\n<event>line one\nline tw").unwrap();
        assert_eq!(cut.event.as_deref(), Some("line one\nline tw"));
        assert!(parse("please read <task-notification>").is_none());
        assert!(is("\n<task-notification>\n") && !is("hi"));
    }
}
