//! Per-device tokens for the web UI and API: minted at pairing, shown
//! once and kept as a SHA-256 hash. A fresh token lives an hour unless a
//! request claims it; revoking one drops the device's push subscriptions
//! with it. The configured `token_file` stays the bootstrap token and is
//! never stored here.

use anyhow::{Context, Result, anyhow};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use ts_rs::TS;

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ui_tokens (
    id INTEGER PRIMARY KEY,
    token_hash TEXT NOT NULL UNIQUE,
    label TEXT NOT NULL,
    user_agent TEXT,
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER,
    expires_at INTEGER,
    revoked_at INTEGER
);
";

/// How long a pairing link waits to be opened.
pub const PAIRING_MS: i64 = 3_600_000;
/// `last_seen_at` moves at most this often.
pub const SEEN_EVERY_MS: i64 = 60_000;
/// Revoked and expired rows linger this long, then go.
const KEEP_DEAD_MS: i64 = 7 * 86_400_000;
pub const MAX_LABEL: usize = 60;
pub const DEFAULT_LABEL: &str = "New device";

/// A device's token as the panel sees it: never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[ts(export_to = "types.ts")]
pub struct Token {
    pub id: i64,
    pub label: String,
    pub user_agent: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    /// Set while the pairing link waits to be opened; past it the token
    /// is dead.
    pub expires_at: Option<i64>,
    /// The token of the request that listed it.
    pub current: bool,
}

const COLUMNS: &str = "id, label, user_agent, created_at, last_seen_at, expires_at";

fn row(r: &rusqlite::Row) -> rusqlite::Result<Token> {
    Ok(Token {
        id: r.get(0)?,
        label: r.get(1)?,
        user_agent: r.get(2)?,
        created_at: r.get(3)?,
        last_seen_at: r.get(4)?,
        expires_at: r.get(5)?,
        current: false,
    })
}

/// A label as stored: trimmed, at most `MAX_LABEL` characters; `None`
/// when empty.
pub fn label(raw: &str) -> Option<String> {
    let label: String = raw.trim().chars().take(MAX_LABEL).collect();
    (!label.is_empty()).then_some(label)
}

/// Hex SHA-256 of a secret, what the table holds.
pub fn hash(secret: &str) -> String {
    digest::digest(&digest::SHA256, secret.as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Mints a token for a new device: the row and the secret, which only
/// this call ever returns. The link is dead in `PAIRING_MS` unless a
/// request claims it.
pub fn mint(conn: &Connection, label: &str, now: i64) -> Result<(Token, String)> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("no randomness for a token"))?;
    let secret = B64.encode(bytes);
    conn.execute(
        "INSERT INTO ui_tokens (token_hash, label, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![hash(&secret), label, now, now + PAIRING_MS],
    )?;
    let token = get(conn, conn.last_insert_rowid())?.context("token vanished")?;
    Ok((token, secret))
}

pub fn get(conn: &Connection, id: i64) -> Result<Option<Token>> {
    Ok(conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM ui_tokens WHERE id = ?1"))?
        .query_row([id], row)
        .optional()?)
}

/// The live token behind `secret`: not revoked, and not an unclaimed
/// link past its hour.
pub fn lookup(conn: &Connection, secret: &str, now: i64) -> Result<Option<Token>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM ui_tokens WHERE token_hash = ?1 AND revoked_at IS NULL
                AND (expires_at IS NULL OR expires_at > ?2)"
        ))?
        .query_row(params![hash(secret), now], row)
        .optional()?)
}

/// Records a use: the pairing expiry clears, the seen stamp and the user
/// agent move.
pub fn touch(conn: &Connection, id: i64, user_agent: Option<&str>, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE ui_tokens SET last_seen_at = ?2, user_agent = ?3, expires_at = NULL
            WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now, user_agent],
    )?;
    Ok(())
}

/// The devices in the order they were added: paired ones and links still
/// waiting to be opened.
pub fn list(conn: &Connection, now: i64) -> Result<Vec<Token>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM ui_tokens WHERE revoked_at IS NULL
            AND (expires_at IS NULL OR expires_at > ?1) ORDER BY id"
    ))?;
    let rows = stmt
        .query_map([now], row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// `None` when no live token has that id.
pub fn relabel(conn: &Connection, id: i64, label: &str, now: i64) -> Result<Option<Token>> {
    let changed = conn.execute(
        "UPDATE ui_tokens SET label = ?2 WHERE id = ?1 AND revoked_at IS NULL
            AND (expires_at IS NULL OR expires_at > ?3)",
        params![id, label, now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// Revokes the token and drops the device's push subscriptions; how many
/// went, or `None` when no live token has that id.
pub fn revoke(conn: &Connection, id: i64, now: i64) -> Result<Option<usize>> {
    let changed = conn.execute(
        "UPDATE ui_tokens SET revoked_at = ?2 WHERE id = ?1 AND revoked_at IS NULL",
        params![id, now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let dropped = conn.execute("DELETE FROM push_subscriptions WHERE token_id = ?1", [id])?;
    Ok(Some(dropped))
}

/// Drops revoked tokens and dead pairing links after a week.
pub fn prune(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM ui_tokens WHERE revoked_at < ?1
            OR (expires_at IS NOT NULL AND expires_at < ?1)",
        [now - KEEP_DEAD_MS],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::push;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(push::SCHEMA).unwrap();
        conn
    }

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(label("  Phone "), Some("Phone".into()));
        assert_eq!(label(" \n"), None);
        assert_eq!(label(&"x".repeat(100)).unwrap().chars().count(), MAX_LABEL);
    }

    #[test]
    fn tokens_are_minted_claimed_expired_and_revoked() {
        let conn = db();
        let (token, secret) = mint(&conn, "Phone", 1_000).unwrap();
        assert_eq!(secret.len(), 43);
        assert_eq!(token.expires_at, Some(1_000 + PAIRING_MS));
        assert!(
            !conn
                .query_row("SELECT token_hash FROM ui_tokens", [], |r| r
                    .get::<_, String>(0))
                .unwrap()
                .contains(&secret)
        );
        assert_eq!(lookup(&conn, "wrong", 2_000).unwrap(), None);
        assert_eq!(lookup(&conn, &secret, 2_000).unwrap().unwrap().id, token.id);
        // an unclaimed link dies after the hour
        assert_eq!(lookup(&conn, &secret, 1_000 + PAIRING_MS).unwrap(), None);
        assert!(list(&conn, 1_000 + PAIRING_MS).unwrap().is_empty());
        // a claimed one lives on
        touch(&conn, token.id, Some("Firefox"), 2_000).unwrap();
        let seen = lookup(&conn, &secret, 1_000 + 2 * PAIRING_MS)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                seen.expires_at,
                seen.last_seen_at,
                seen.user_agent.as_deref()
            ),
            (None, Some(2_000), Some("Firefox"))
        );
        let renamed = relabel(&conn, token.id, "Pixel", 3_000).unwrap().unwrap();
        assert_eq!(renamed.label, "Pixel");
        assert_eq!(relabel(&conn, 99, "x", 3_000).unwrap(), None);

        // revoking drops the device's push subscriptions, no one else's
        let sub = |endpoint: &str| {
            push::Subscribe {
            endpoint: endpoint.into(),
            keys: push::SubscribeKeys {
                p256dh: "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4".into(),
                auth: "BTBZMqHH6r4Tts7J_aSIgg".into(),
            },
        }
        };
        push::subscribe(&conn, &sub("https://p/1"), None, Some(token.id), 3_000).unwrap();
        push::subscribe(&conn, &sub("https://p/2"), None, None, 3_000).unwrap();
        assert_eq!(revoke(&conn, token.id, 4_000).unwrap(), Some(1));
        assert_eq!(revoke(&conn, token.id, 4_000).unwrap(), None);
        assert_eq!(lookup(&conn, &secret, 4_000).unwrap(), None);
        assert!(list(&conn, 4_000).unwrap().is_empty());
        let left = push::subscriptions(&conn).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].endpoint, "https://p/2");

        // the revoked row and a dead link go after a week
        mint(&conn, "Never opened", 5_000).unwrap();
        let (kept, secret) = mint(&conn, "Kept", 5_000).unwrap();
        touch(&conn, kept.id, None, 5_500).unwrap();
        prune(&conn, 5_000 + PAIRING_MS).unwrap();
        assert_eq!(count(&conn), 3);
        prune(&conn, 5_001 + PAIRING_MS + KEEP_DEAD_MS).unwrap();
        assert_eq!(count(&conn), 1);
        assert!(lookup(&conn, &secret, 6_000).unwrap().is_some());
    }

    fn count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM ui_tokens", [], |r| r.get(0))
            .unwrap()
    }
}
