//! A session's conversation rebuilt from hook events: one exchange per
//! prompt, holding the tool calls, subagents, compactions and replies that
//! followed it.

use anyhow::Result;
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;

use crate::{
    history::{Transition, segments},
    markdown,
    model::Activity,
};

const MAX_TOOLS_PER_EXCHANGE: usize = 200;

#[derive(Debug, Serialize)]
pub struct Conversation {
    pub exchanges: Vec<Exchange>,
    /// Older exchanges exist; ask again with `before` set to the first one's `at`.
    pub more: bool,
}

#[derive(Debug, Serialize)]
pub struct Exchange {
    pub at: i64,
    /// None for events that came before the first prompt in the store.
    pub prompt: Option<String>,
    pub items: Vec<Item>,
    /// Oldest tool calls left out of `items` in very long exchanges.
    pub omitted: u32,
    /// Index and text of the newest `Message`, to drop it when the turn's
    /// reply repeats it.
    #[serde(skip)]
    last_message: Option<(usize, String)>,
}

impl Exchange {
    fn new(at: i64, prompt: Option<String>) -> Self {
        Self {
            at,
            prompt,
            items: Vec::new(),
            omitted: 0,
            last_message: None,
        }
    }

    fn push(&mut self, kind: &str, data: &Value, item: Item) {
        match kind {
            "message" => self.last_message = Some((self.items.len(), text(data, "text"))),
            "reply" => {
                if let Some((i, said)) = self.last_message.take()
                    && same_text(&said, &text(data, "text"))
                {
                    self.items.remove(i);
                }
            }
            _ => {}
        }
        self.items.push(item);
    }
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Tool {
        at: i64,
        name: String,
        summary: String,
        ms: Option<i64>,
        ok: bool,
    },
    Permission {
        at: i64,
        tool: String,
        summary: String,
    },
    /// Text Claude wrote during a turn (between tool calls).
    Message {
        at: i64,
        html: String,
    },
    Reply {
        at: i64,
        html: String,
    },
    Error {
        at: i64,
        error: String,
    },
    Subagent {
        at: i64,
        agent_type: String,
        description: Option<String>,
        ms: Option<i64>,
        tools: Option<u64>,
        html: String,
    },
    Compact {
        at: i64,
        trigger: Option<String>,
        html: String,
    },
    Task {
        at: i64,
        subject: String,
        status: String,
    },
    Start {
        at: i64,
        source: String,
    },
    End {
        at: i64,
        reason: String,
    },
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    pub prompts: i64,
    pub tools: i64,
    pub failures: i64,
    /// Distinct files edited successfully.
    pub files: i64,
    pub subagents: i64,
    pub compactions: i64,
    pub working_ms: i64,
    pub permission_ms: i64,
    pub your_turn_ms: i64,
}

/// The newest `limit` exchanges before `before` (all of them when `None`),
/// oldest first.
pub fn conversation(
    conn: &Connection,
    session: &str,
    before: Option<i64>,
    limit: usize,
) -> Result<Conversation> {
    let limit = limit.max(1);
    let before = before.unwrap_or(i64::MAX);
    let prompts: Vec<i64> = conn
        .prepare_cached(
            "SELECT ts FROM events WHERE session = ?1 AND kind = 'prompt' AND ts < ?2
                ORDER BY ts DESC LIMIT ?3",
        )?
        .query_map(params![session, before, i64::try_from(limit)? + 1], |r| {
            r.get(0)
        })?
        .collect::<rusqlite::Result<_>>()?;
    let more = prompts.len() > limit;
    let from = if more { prompts[limit - 1] } else { i64::MIN };

    let mut stmt = conn.prepare_cached(
        "SELECT ts, kind, data FROM events
            WHERE session = ?1 AND ts >= ?2 AND ts < ?3 AND kind != 'activity'
                AND kind != 'notification'
            ORDER BY ts, rowid",
    )?;
    let rows = stmt.query_map(params![session, from, before], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut exchanges: Vec<Exchange> = Vec::new();
    for row in rows {
        let (at, kind, data) = row?;
        let data: Value = serde_json::from_str(&data).unwrap_or_default();
        if kind == "prompt" {
            exchanges.push(Exchange::new(at, Some(text(&data, "text"))));
        } else if let Some(item) = item(at, &kind, &data) {
            if exchanges.is_empty() {
                exchanges.push(Exchange::new(at, None));
            }
            exchanges
                .last_mut()
                .expect("pushed above")
                .push(&kind, &data, item);
        }
    }
    for exchange in &mut exchanges {
        cap_tools(exchange);
    }
    Ok(Conversation { exchanges, more })
}

fn item(at: i64, kind: &str, data: &Value) -> Option<Item> {
    Some(match kind {
        "tool" => Item::Tool {
            at,
            name: text(data, "name"),
            summary: text(data, "summary"),
            ms: data["ms"].as_i64(),
            ok: data["ok"].as_bool().unwrap_or(true),
        },
        "permission" => Item::Permission {
            at,
            tool: text(data, "tool"),
            summary: text(data, "summary"),
        },
        "message" => Item::Message {
            at,
            html: markdown::render(&text(data, "text")),
        },
        "reply" => {
            let reply = text(data, "text");
            if reply.is_empty() {
                return None;
            }
            Item::Reply {
                at,
                html: markdown::render(&reply),
            }
        }
        "error" => Item::Error {
            at,
            error: text(data, "error"),
        },
        "subagent" => Item::Subagent {
            at,
            agent_type: text(data, "agent_type"),
            description: data["description"].as_str().map(str::to_owned),
            ms: data["ms"].as_i64(),
            tools: data["tools"].as_u64(),
            html: markdown::render(&text(data, "text")),
        },
        "compact" => Item::Compact {
            at,
            trigger: data["trigger"].as_str().map(str::to_owned),
            html: markdown::render(&text(data, "summary")),
        },
        "task" => Item::Task {
            at,
            subject: text(data, "subject"),
            status: text(data, "status"),
        },
        "start" => Item::Start {
            at,
            source: text(data, "source"),
        },
        "end" => Item::End {
            at,
            reason: text(data, "reason"),
        },
        _ => return None,
    })
}

fn same_text(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

fn text(data: &Value, key: &str) -> String {
    data[key].as_str().unwrap_or_default().to_owned()
}

fn cap_tools(exchange: &mut Exchange) {
    let tools = exchange
        .items
        .iter()
        .filter(|i| matches!(i, Item::Tool { .. }))
        .count();
    let mut drop = tools.saturating_sub(MAX_TOOLS_PER_EXCHANGE);
    exchange.omitted = u32::try_from(drop).unwrap_or(u32::MAX);
    exchange.items.retain(|i| {
        if drop > 0 && matches!(i, Item::Tool { .. }) {
            drop -= 1;
            return false;
        }
        true
    });
}

/// The session's process samples since `since`: `[ts, cpu %, memory bytes]`.
pub fn resources(conn: &Connection, session: &str, since: i64) -> Result<Vec<(i64, f64, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ts, cpu_pct, mem_bytes FROM proc_samples WHERE session = ?1 AND ts >= ?2 ORDER BY ts",
    )?;
    let rows = stmt.query_map(params![session, since], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Totals over the whole session; time in each activity runs from
/// `started_at` to `end` (now for live sessions).
pub fn stats(conn: &Connection, session: &str, started_at: i64, end: i64) -> Result<Stats> {
    let mut stats = conn.query_row(
        "SELECT
            coalesce(sum(kind = 'prompt'), 0),
            coalesce(sum(kind = 'tool'), 0),
            coalesce(sum(kind = 'tool' AND json_extract(data, '$.ok') = 0), 0),
            coalesce(sum(kind = 'subagent'), 0),
            coalesce(sum(kind = 'compact'), 0)
            FROM events WHERE session = ?1",
        [session],
        |r| {
            Ok(Stats {
                prompts: r.get(0)?,
                tools: r.get(1)?,
                failures: r.get(2)?,
                subagents: r.get(3)?,
                compactions: r.get(4)?,
                ..Stats::default()
            })
        },
    )?;
    stats.files = conn.query_row(
        "SELECT count(DISTINCT json_extract(data, '$.summary')) FROM events
            WHERE session = ?1 AND kind = 'tool' AND json_extract(data, '$.ok') = 1
                AND json_extract(data, '$.name') IN ('Edit', 'Write', 'MultiEdit', 'NotebookEdit')",
        [session],
        |r| r.get(0),
    )?;

    let transitions: Vec<(i64, Transition)> = conn
        .prepare_cached(
            "SELECT ts, data FROM events WHERE session = ?1 AND kind = 'activity' ORDER BY ts, rowid",
        )?
        .query_map([session], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .filter_map(|row| {
            let (ts, data) = row.ok()?;
            Some((ts, serde_json::from_str(&data).ok()?))
        })
        .collect();
    if let Some((_, first)) = transitions.first() {
        for (from, to, activity) in segments(started_at, end, first.from, &transitions) {
            let total = match activity {
                Activity::Working => &mut stats.working_ms,
                Activity::Permission => &mut stats.permission_ms,
                Activity::YourTurn => &mut stats.your_turn_ms,
                Activity::Idle | Activity::Gone => continue,
            };
            *total += to - from;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SCHEMA;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn
    }

    fn event(conn: &Connection, ts: i64, kind: &str, data: Value) {
        conn.execute(
            "INSERT INTO events VALUES (?1, 'h', 's', ?2, ?3)",
            params![ts, kind, data.to_string()],
        )
        .unwrap();
    }

    fn seed(conn: &Connection) {
        use serde_json::json;
        event(conn, 1, "start", json!({"source": "startup"}));
        event(
            conn,
            2,
            "activity",
            json!({"from": "idle", "to": "working"}),
        );
        event(conn, 2, "prompt", json!({"text": "first"}));
        event(
            conn,
            3,
            "tool",
            json!({"name": "Edit", "summary": "a.rs", "ms": 5, "ok": true}),
        );
        event(
            conn,
            4,
            "tool",
            json!({"name": "Bash", "summary": "make", "ms": 9, "ok": false}),
        );
        event(conn, 5, "reply", json!({"text": "**done**"}));
        event(
            conn,
            5,
            "activity",
            json!({"from": "working", "to": "your_turn"}),
        );
        event(conn, 10, "prompt", json!({"text": "second"}));
        event(
            conn,
            10,
            "activity",
            json!({"from": "your_turn", "to": "working"}),
        );
        event(
            conn,
            11,
            "tool",
            json!({"name": "Edit", "summary": "a.rs", "ms": 5, "ok": true}),
        );
        event(
            conn,
            12,
            "subagent",
            json!({"agent_type": "Explore", "description": "find it", "ms": 3, "tools": 4, "text": "found"}),
        );
    }

    #[test]
    fn groups_events_under_prompts_and_pages_backwards() {
        let conn = db();
        seed(&conn);

        let all = conversation(&conn, "s", None, 10).unwrap();
        assert!(!all.more);
        assert_eq!(all.exchanges.len(), 3);
        assert_eq!(all.exchanges[0].prompt, None);
        assert_eq!(
            all.exchanges[0].items,
            [Item::Start {
                at: 1,
                source: "startup".into()
            }]
        );
        assert_eq!(all.exchanges[1].prompt.as_deref(), Some("first"));
        assert_eq!(all.exchanges[1].items.len(), 3);
        assert_eq!(
            all.exchanges[1].items[2],
            Item::Reply {
                at: 5,
                html: "<p><strong>done</strong></p>\n".into()
            }
        );

        let newest = conversation(&conn, "s", None, 1).unwrap();
        assert!(newest.more);
        assert_eq!(newest.exchanges.len(), 1);
        assert_eq!(newest.exchanges[0].prompt.as_deref(), Some("second"));
        assert!(matches!(
            newest.exchanges[0].items[1],
            Item::Subagent { tools: Some(4), .. }
        ));

        let older = conversation(&conn, "s", Some(newest.exchanges[0].at), 1).unwrap();
        assert!(!older.more);
        assert_eq!(older.exchanges.len(), 2);
        assert_eq!(older.exchanges[1].prompt.as_deref(), Some("first"));
    }

    #[test]
    fn messages_between_tools_and_no_duplicate_reply() {
        use serde_json::json;
        let conn = db();
        event(&conn, 1, "prompt", json!({"text": "go"}));
        event(&conn, 2, "message", json!({"text": "Let me look."}));
        event(
            &conn,
            3,
            "tool",
            json!({"name": "Read", "summary": "a", "ok": true}),
        );
        event(&conn, 4, "message", json!({"text": "All  done.\n"}));
        event(&conn, 5, "reply", json!({"text": "All done."}));
        let c = conversation(&conn, "s", None, 10).unwrap();
        let kinds: Vec<&str> = c.exchanges[0]
            .items
            .iter()
            .map(|i| match i {
                Item::Message { .. } => "message",
                Item::Tool { .. } => "tool",
                Item::Reply { .. } => "reply",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["message", "tool", "reply"]);
    }

    #[test]
    fn caps_tools_in_long_exchanges() {
        use serde_json::json;
        let conn = db();
        event(&conn, 1, "prompt", json!({"text": "go"}));
        for ts in 2..(2 + MAX_TOOLS_PER_EXCHANGE as i64 + 5) {
            event(
                &conn,
                ts,
                "tool",
                json!({"name": "Read", "summary": ts.to_string(), "ok": true}),
            );
        }
        let c = conversation(&conn, "s", None, 10).unwrap();
        assert_eq!(c.exchanges[0].omitted, 5);
        assert_eq!(c.exchanges[0].items.len(), MAX_TOOLS_PER_EXCHANGE);
        assert!(matches!(&c.exchanges[0].items[0], Item::Tool { summary, .. } if summary == "7"));
    }

    #[test]
    fn stats_count_and_time() {
        let conn = db();
        seed(&conn);
        let s = stats(&conn, "s", 0, 20).unwrap();
        assert_eq!(
            s,
            Stats {
                prompts: 2,
                tools: 3,
                failures: 1,
                files: 1,
                subagents: 1,
                compactions: 0,
                working_ms: 3 + 10,
                permission_ms: 0,
                your_turn_ms: 5,
            }
        );
    }
}
