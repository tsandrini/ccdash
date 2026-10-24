//! Resolving a session from the panel: one last prompt through the
//! channel asks Claude to wrap up (the plugin's `wrap-up` skill), and the
//! hub closes the session when that turn ends.

/// Its first line, which tells the wrap-up turn apart.
const MARK: &str = "Task is resolved, user is closing the session.";

/// What the hub sends when the owner marks a session resolved.
pub const PROMPT: &str = "Task is resolved, user is closing the session.\n\n\
    Wrap up with the ccdash:wrap-up skill: record what this session learned and did \
    in ccdash (memories, issues, progress) and release its claim. Ask nothing and \
    start no new work: the session closes when this turn ends.";

/// Whether a prompt relayed from ccdash carries the wrap-up prompt (alone
/// or with replies Claude Code delivered with it).
pub fn asked(relayed: &str) -> bool {
    relayed.contains(MARK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_is_told_apart_by_its_first_line() {
        assert!(PROMPT.starts_with(MARK));
        assert!(asked(PROMPT));
        assert!(asked(&format!("one more thing\n\n{PROMPT}")));
        assert!(!asked("Task is resolved."));
    }
}
