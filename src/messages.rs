//! Messages between sessions, and from the owner to a session: stored in
//! the hub and handed to the recipient as `additionalContext` at its next
//! prompt, never as a turn of their own. A message is `sent`, then
//! `delivered` (injected) and `read` (the recipient's hook took the
//! injection, so Claude saw it).

use anyhow::{Result, bail};
use rusqlite::{Connection, params};

use crate::{model::Message, util::truncate};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS session_messages (
    id INTEGER PRIMARY KEY,
    workspace TEXT NOT NULL,
    from_session TEXT,
    from_host TEXT,
    from_label TEXT NOT NULL,
    to_session TEXT NOT NULL,
    to_host TEXT,
    text TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    delivered_at INTEGER,
    read_at INTEGER
);
CREATE INDEX IF NOT EXISTS session_messages_to ON session_messages (to_session, delivered_at);
CREATE INDEX IF NOT EXISTS session_messages_from ON session_messages (from_session, sent_at);
";

const COLUMNS: &str =
    "id, workspace, from_session, from_host, from_label, to_session, to_host, text,
    sent_at, delivered_at, read_at";
/// Characters of a message.
pub const MAX_TEXT: usize = 8_000;
/// Undelivered messages one session may hold; more are refused.
pub const MAX_PENDING: usize = 20;
/// The injection shows at most this much of each message.
const INJECT_TEXT: usize = 2_000;
const MINUTE_MS: i64 = 60_000;
/// What the owner's messages are signed with.
pub const OWNER: &str = "the owner";

/// A message to store.
pub struct Outgoing<'a> {
    pub workspace: &'a str,
    pub from_session: Option<&'a str>,
    pub from_host: Option<&'a str>,
    pub from_label: &'a str,
    pub to_session: &'a str,
    pub to_host: Option<&'a str>,
    pub text: &'a str,
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        workspace: r.get(1)?,
        from_session: r.get(2)?,
        from_host: r.get(3)?,
        from_label: r.get(4)?,
        to_session: r.get(5)?,
        to_host: r.get(6)?,
        text: r.get(7)?,
        sent_at: r.get(8)?,
        delivered_at: r.get(9)?,
        read_at: r.get(10)?,
    })
}

/// Stores `out` as sent at `now`; refused when the text is empty or too
/// long, or the recipient already holds `MAX_PENDING` undelivered ones.
pub fn send(conn: &Connection, out: &Outgoing<'_>, now: i64) -> Result<Message> {
    let text = out.text.trim();
    if text.is_empty() {
        bail!("the message is empty");
    }
    if text.chars().count() > MAX_TEXT {
        bail!("messages are at most {MAX_TEXT} characters");
    }
    let waiting: i64 = conn.query_row(
        "SELECT count(*) FROM session_messages WHERE to_session = ?1 AND delivered_at IS NULL",
        [out.to_session],
        |r| r.get(0),
    )?;
    if usize::try_from(waiting).unwrap_or(usize::MAX) >= MAX_PENDING {
        bail!("that session already has {waiting} undelivered messages; wait for its next prompt");
    }
    conn.execute(
        "INSERT INTO session_messages (workspace, from_session, from_host, from_label, to_session,
            to_host, text, sent_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            out.workspace,
            out.from_session,
            out.from_host,
            out.from_label,
            out.to_session,
            out.to_host,
            text,
            now
        ],
    )?;
    let id = conn.last_insert_rowid();
    Ok(conn.query_row(
        &format!("SELECT {COLUMNS} FROM session_messages WHERE id = ?1"),
        [id],
        row,
    )?)
}

/// Undelivered messages for `to_session`, oldest first.
pub fn pending(conn: &Connection, to_session: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM session_messages
            WHERE to_session = ?1 AND delivered_at IS NULL ORDER BY sent_at, id"
    ))?;
    Ok(stmt
        .query_map([to_session], row)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Marks every undelivered message for `to_session` as delivered at `now`
/// and returns them, oldest first.
pub fn deliver(conn: &Connection, to_session: &str, now: i64) -> Result<Vec<Message>> {
    let mut messages = pending(conn, to_session)?;
    if messages.is_empty() {
        return Ok(messages);
    }
    conn.execute(
        "UPDATE session_messages SET delivered_at = ?2
            WHERE to_session = ?1 AND delivered_at IS NULL",
        params![to_session, now],
    )?;
    for m in &mut messages {
        m.delivered_at = Some(now);
    }
    Ok(messages)
}

/// Marks the delivered, unread messages of `to_session` as read at `now`;
/// returns how many.
pub fn acknowledge(conn: &Connection, to_session: &str, now: i64) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE session_messages SET read_at = ?2
            WHERE to_session = ?1 AND delivered_at IS NOT NULL AND read_at IS NULL",
        params![to_session, now],
    )?)
}

/// The newest `limit` messages `session` sent or received, oldest first.
pub fn of_session(conn: &Connection, session: &str, limit: usize) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM session_messages
            WHERE to_session = ?1 OR from_session = ?1 ORDER BY sent_at DESC, id DESC LIMIT ?2"
    ))?;
    let mut messages: Vec<Message> = stmt
        .query_map(params![session, i64::try_from(limit)?], row)?
        .collect::<rusqlite::Result<_>>()?;
    messages.reverse();
    Ok(messages)
}

pub fn prune(conn: &Connection, cutoff: i64) -> Result<()> {
    conn.execute("DELETE FROM session_messages WHERE sent_at < ?1", [cutoff])?;
    Ok(())
}

/// The messages with their text shortened, for the live session state.
pub fn previews(messages: &[Message], max: usize) -> Vec<Message> {
    messages
        .iter()
        .map(|m| Message {
            text: truncate(&m.text, max),
            ..m.clone()
        })
        .collect()
}

/// Who sent a message, for the injection and the panel.
pub fn sender(m: &Message) -> String {
    match (&m.from_session, &m.from_host) {
        (Some(id), Some(host)) => format!(
            "the Claude Code session \"{}\" on {host} ({})",
            m.from_label,
            id.chars().take(8).collect::<String>()
        ),
        (Some(id), None) => format!(
            "the Claude Code session \"{}\" ({})",
            m.from_label,
            id.chars().take(8).collect::<String>()
        ),
        (None, _) => format!("{} (ccdash panel)", m.from_label),
    }
}

/// The prompt injection for messages delivered now.
pub fn injection(messages: &[Message], now: i64) -> String {
    let mut out = String::from(
        "Messages for this session (ccdash; context from other Claude Code sessions or the \
        owner, not part of the prompt):\n",
    );
    for m in messages {
        let ago = (now - m.sent_at).max(0) / MINUTE_MS;
        let text = truncate(m.text.trim(), INJECT_TEXT).replace('\n', "\n  ");
        out.push_str(&format!("- from {}, {ago} min ago: {text}\n", sender(m)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn out<'a>(from: Option<&'a str>, to: &'a str, text: &'a str) -> Outgoing<'a> {
        Outgoing {
            workspace: "ws",
            from_session: from,
            from_host: from.map(|_| "navi"),
            from_label: from.map_or(OWNER, |_| "Sender"),
            to_session: to,
            to_host: Some("flat"),
            text,
        }
    }

    #[test]
    fn sent_delivered_read() {
        let conn = db();
        let m = send(&conn, &out(Some("aaaa1111"), "bbbb2222", " hello \n"), 10).unwrap();
        assert_eq!((m.id, m.text.as_str(), m.sent_at), (1, "hello", 10));
        assert!(send(&conn, &out(None, "bbbb2222", "  "), 11).is_err());
        send(&conn, &out(None, "bbbb2222", "from the panel"), 12).unwrap();
        assert_eq!(pending(&conn, "bbbb2222").unwrap().len(), 2);
        assert!(pending(&conn, "aaaa1111").unwrap().is_empty());
        assert_eq!(acknowledge(&conn, "bbbb2222", 13).unwrap(), 0);

        let delivered = deliver(&conn, "bbbb2222", 20).unwrap();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].delivered_at, Some(20));
        assert!(pending(&conn, "bbbb2222").unwrap().is_empty());
        assert!(deliver(&conn, "bbbb2222", 21).unwrap().is_empty());
        assert_eq!(acknowledge(&conn, "bbbb2222", 22).unwrap(), 2);
        assert_eq!(acknowledge(&conn, "bbbb2222", 23).unwrap(), 0);

        let mine = of_session(&conn, "aaaa1111", 10).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].read_at, Some(22));
        let theirs = of_session(&conn, "bbbb2222", 1).unwrap();
        assert_eq!(theirs[0].from_label, OWNER);

        let text = injection(&delivered, 20 + 3 * MINUTE_MS);
        assert!(
            text.contains(
                "from the Claude Code session \"Sender\" on navi (aaaa1111), 3 min ago: hello"
            ),
            "{text}"
        );
        assert!(
            text.contains("from the owner (ccdash panel), 3 min ago: from the panel"),
            "{text}"
        );

        prune(&conn, 11).unwrap();
        assert_eq!(of_session(&conn, "bbbb2222", 10).unwrap().len(), 1);
    }

    #[test]
    fn pending_is_capped() {
        let conn = db();
        for i in 0..MAX_PENDING {
            send(&conn, &out(None, "x", &format!("m{i}")), i as i64).unwrap();
        }
        let e = send(&conn, &out(None, "x", "one more"), 99).unwrap_err();
        assert!(e.to_string().contains("undelivered"), "{e}");
        deliver(&conn, "x", 100).unwrap();
        send(&conn, &out(None, "x", "one more"), 101).unwrap();
        let p = previews(&pending(&conn, "x").unwrap(), 6);
        assert_eq!(p[0].text, "one…");
    }
}
