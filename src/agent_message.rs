//! Messages subagents send the session that started them
//! (`<agent-message from="…">`): the report a finished one hands back, or a
//! message while it runs. Like task notifications they reach the prompt hook
//! as if you had typed them; the hub records them apart.

use serde::Serialize;
use ts_rs::TS;

use crate::markdown;

/// The event kind they are recorded as.
pub const KIND: &str = "agent_message";

const OPEN: &str = "<agent-message from=\"";
const CLOSE: &str = "</agent-message>";
const HAND_BACK: &str = "[Subagent hand-back]";
const REPORT_FOLLOWS: &str = "The report follows:";

/// A subagent's message as the panel shows it.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct AgentMessage {
    /// The sending agent's id.
    pub from: String,
    /// The description the session gave it, when the hub knew the agent.
    pub name: Option<String>,
    /// Its type (`Explore`, `general-purpose`, …), when the hub knew it.
    pub agent_type: Option<String>,
    /// The report of a finished subagent, rather than a message while it
    /// runs.
    pub hand_back: bool,
    /// The message (for a report, without Claude Code's frame), as HTML.
    pub html: String,
}

/// The sender's id when a prompt is a subagent's message.
pub fn sender(prompt: &str) -> Option<&str> {
    let (from, _) = prompt.trim_start().strip_prefix(OPEN)?.split_once('"')?;
    (!from.is_empty()).then_some(from)
}

/// Reads a subagent's message; `name` and `agent_type` are what the hub
/// knew of the sender when it arrived. `None` when `text` is not one.
///
/// ```ignore
/// let m = parse("<agent-message from=\"a1\">\nDone.\n</agent-message>", None, None).unwrap();
/// assert_eq!((m.from.as_str(), m.hand_back, m.html.as_str()), ("a1", false, "<p>Done.</p>\n"));
/// ```
pub fn parse(text: &str, name: Option<&str>, agent_type: Option<&str>) -> Option<AgentMessage> {
    let from = sender(text)?;
    let body = text.split_once('>').map_or("", |(_, rest)| rest);
    let body = body.rfind(CLOSE).map_or(body, |end| &body[..end]);
    let hand_back = body.trim_start().starts_with(HAND_BACK);
    let message = if hand_back {
        // NOTE: Claude Code indents each line of the report by two spaces
        let report = body.split_once(REPORT_FOLLOWS).map_or(body, |(_, r)| r);
        report
            .trim_start_matches(['\r', '\n'])
            .lines()
            .map(|line| line.strip_prefix("  ").unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        body.to_owned()
    };
    Some(AgentMessage {
        from: from.to_owned(),
        name: name.map(str::to_owned),
        agent_type: agent_type.map(str::to_owned),
        hand_back,
        html: markdown::render(message.trim()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_handed_back_loses_its_frame_and_indent() {
        let text = concat!(
            "<agent-message from=\"af08\">\n",
            "[Subagent hand-back] The text below is the final report. ",
            "The report follows:\n",
            "  | File | Lines |\n",
            "  |---|---|\n",
            "  | state.rs | 2984 |\n",
            "  \n",
            "      indented code\n",
            "</agent-message>"
        );
        assert_eq!(sender(text), Some("af08"));
        let m = parse(text, Some("Largest files"), Some("Explore")).unwrap();
        assert!(m.hand_back);
        assert_eq!(
            (m.name.as_deref(), m.agent_type.as_deref()),
            (Some("Largest files"), Some("Explore"))
        );
        assert!(m.html.starts_with("<table>"), "{}", m.html);
        assert!(m.html.contains("<pre><code>indented code"), "{}", m.html);
        assert!(!m.html.contains("hand-back"));
    }

    #[test]
    fn a_message_while_running_and_what_is_none() {
        let m = parse(
            "<agent-message from=\"a1\">\nW1 request: change **md** to lg.\n</agent-message>",
            None,
            None,
        )
        .unwrap();
        assert!(!m.hand_back);
        assert_eq!(
            m.html,
            "<p>W1 request: change <strong>md</strong> to lg.</p>\n"
        );
        assert!(sender("see <agent-message from=\"x\">").is_none());
        assert!(sender("<agent-message from=\"\">").is_none());
    }
}
