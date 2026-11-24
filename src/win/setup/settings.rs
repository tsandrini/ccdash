//! Claude Code's `settings.json`: ccdash's hooks added where they are
//! missing, the statusline and the telemetry variables when asked for.
//! The file is the user's: everything else in it stays, in its order.

use anyhow::{Context, Result, bail};
use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, SeqAccess, Visitor},
    ser::{SerializeMap, SerializeSeq},
};

/// The hook events ccdash listens to, and those of them that name a tool
/// (their hooks take a matcher).
// NOTE: a copy of `hookEvents` and `toolEvents` in flake-parts/modules/home-manager/ccdash.nix
const EVENTS: [&str; 17] = [
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "PermissionRequest",
    "PermissionDenied",
    "Stop",
    "StopFailure",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
    "TaskCreated",
    "TaskCompleted",
    "MessageDisplay",
];
const TOOL_EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "PermissionDenied",
];
/// What Claude Code's OpenTelemetry export needs to reach the collector's
/// receiver (`[telemetry] listen`).
const TELEMETRY: [(&str, &str); 6] = [
    ("CLAUDE_CODE_ENABLE_TELEMETRY", "1"),
    ("OTEL_METRICS_EXPORTER", "otlp"),
    ("OTEL_LOGS_EXPORTER", "otlp"),
    ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
    RECEIVER,
    ("OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE", "delta"),
];
/// The one of them that sends the export to the collector: what tells
/// ccdash's telemetry from an export of the user's own.
const RECEIVER: (&str, &str) = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:7839");

/// A JSON value whose objects keep their keys in the file's order, where
/// `serde_json::Value` sorts them.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn text(s: &str) -> Self {
        Self::String(s.to_owned())
    }

    fn object(members: impl IntoIterator<Item = (&'static str, Json)>) -> Self {
        Self::Object(
            members
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    /// The member `key` of an object, to change it; none when there is no
    /// such member.
    fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Self::Object(members) => members.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The member `key` of an object, made with `empty` at the end when
    /// missing; none when this is no object.
    pub fn member(&mut self, key: &str, empty: Json) -> Option<&mut Json> {
        let Self::Object(members) = self else {
            return None;
        };
        let at = match members.iter().position(|(k, _)| k == key) {
            Some(at) => at,
            None => {
                members.push((key.to_owned(), empty));
                members.len() - 1
            }
        };
        Some(&mut members[at].1)
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(b) => serializer.serialize_bool(*b),
            Self::Number(n) => n.serialize(serializer),
            Self::String(s) => serializer.serialize_str(s),
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Object(members) => {
                let mut map = serializer.serialize_map(Some(members.len()))?;
                for (key, value) in members {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Any;

        impl<'de> Visitor<'de> for Any {
            type Value = Json;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Json, E> {
                Ok(Json::Null)
            }

            fn visit_bool<E>(self, v: bool) -> Result<Json, E> {
                Ok(Json::Bool(v))
            }

            fn visit_i64<E>(self, v: i64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }

            fn visit_u64<E>(self, v: u64) -> Result<Json, E> {
                Ok(Json::Number(v.into()))
            }

            fn visit_f64<E>(self, v: f64) -> Result<Json, E> {
                Ok(serde_json::Number::from_f64(v).map_or(Json::Null, Json::Number))
            }

            fn visit_str<E>(self, v: &str) -> Result<Json, E> {
                Ok(Json::String(v.to_owned()))
            }

            fn visit_string<E>(self, v: String) -> Result<Json, E> {
                Ok(Json::String(v))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Json::Array(items))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Json::Object(members))
            }
        }

        deserializer.deserialize_any(Any)
    }
}

/// A JSON document as it is written to a file: indented, with a last newline.
///
/// # Errors
///
/// None in practice: the value holds nothing JSON cannot.
pub fn to_text(json: &Json) -> Result<String> {
    Ok(serde_json::to_string_pretty(json)? + "\n")
}

/// What `ccdash win setup` may add beyond the hooks.
#[derive(Debug, Clone, Copy, Default)]
pub struct Extras {
    pub status_line: bool,
    pub telemetry: bool,
}

/// Whether a hook (`{"type": "command", "command": …}`) is ccdash's.
fn is_ccdash(hook: &Json) -> bool {
    hook.get("command")
        .and_then(Json::as_str)
        .is_some_and(|command| {
            command.starts_with("ccdash emit") || command.starts_with("ccdash ask")
        })
}

/// Whether a list of hook entries (`[{"matcher": …, "hooks": [{"command": …}]}]`)
/// already runs ccdash.
fn runs_ccdash(entries: &Json) -> bool {
    let Json::Array(entries) = entries else {
        return false;
    };
    entries.iter().any(|entry| {
        let Some(Json::Array(hooks)) = entry.get("hooks") else {
            return false;
        };
        hooks.iter().any(is_ccdash)
    })
}

/// `settings.json` without ccdash's part, and how many things went: its
/// hooks, its statusline (one of the user's that ccdash stood in front of
/// is the user's again) and the telemetry variables that still hold what
/// ccdash set. What the user has stays, a hook of theirs in the same list
/// as ccdash's too.
///
/// # Errors
///
/// `text` is no JSON object.
pub fn unmerge(text: &str) -> Result<(String, usize)> {
    // NOTE: an empty file holds nothing of ccdash's, as `merge` takes it for no settings
    if text.trim().is_empty() {
        return Ok((text.to_owned(), 0));
    }
    let mut settings: Json = serde_json::from_str(text).context("it is not valid JSON")?;
    let Json::Object(members) = &mut settings else {
        bail!("it is not a JSON object");
    };
    let mut removed = 0;
    let find = |members: &[(String, Json)], key: &str| members.iter().position(|(k, _)| k == key);
    // NOTE: a list, an event or the whole table goes only when taking ccdash's out emptied it
    if let Some(at) = find(members, "hooks")
        && let Json::Object(events) = &mut members[at].1
    {
        let before = removed;
        events.retain_mut(|(_, entries)| {
            let Json::Array(entries) = entries else {
                return true;
            };
            let had = removed;
            entries.retain_mut(|entry| {
                let Some(Json::Array(hooks)) = entry.get_mut("hooks") else {
                    return true;
                };
                let held = hooks.len();
                hooks.retain(|hook| !is_ccdash(hook));
                removed += held - hooks.len();
                held == hooks.len() || !hooks.is_empty()
            });
            had == removed || !entries.is_empty()
        });
        if before != removed && events.is_empty() {
            members.remove(at);
        }
    }
    if let Some(at) = find(members, "statusLine") {
        let command = members[at].1.get("command").and_then(Json::as_str);
        match command.map(str::trim) {
            Some("ccdash tee") => {
                members.remove(at);
                removed += 1;
            }
            Some(wrapped) if wrapped.starts_with("ccdash tee -- ") => {
                let own = Json::String(wrapped["ccdash tee -- ".len()..].to_owned());
                if let Some(command) = members[at].1.get_mut("command") {
                    *command = own;
                }
                removed += 1;
            }
            _ => {}
        }
    }
    if let Some(at) = find(members, "env")
        && let Json::Object(env) = &mut members[at].1
    {
        let held = env.len();
        // NOTE: only an export that goes to ccdash's receiver: the same variables with
        // another endpoint are the user's own telemetry, whatever else they have in common
        let (endpoint, receiver) = RECEIVER;
        let to_ccdash = env
            .iter()
            .any(|(name, value)| name == endpoint && value.as_str() == Some(receiver));
        env.retain(|(name, value)| {
            !(to_ccdash
                && TELEMETRY
                    .iter()
                    .any(|(ours, set)| name == ours && value.as_str() == Some(set)))
        });
        removed += held - env.len();
        if held != env.len() && env.is_empty() {
            members.remove(at);
        }
    }
    Ok((to_text(&settings)?, removed))
}

fn hook_entry(event: &str) -> Json {
    let (command, timeout) = if event == "PermissionRequest" {
        ("ccdash ask", 86_400)
    } else {
        ("ccdash emit", 5)
    };
    let hook = Json::object([
        ("type", Json::text("command")),
        ("command", Json::text(command)),
        ("timeout", Json::Number(timeout.into())),
    ]);
    let hooks = ("hooks", Json::Array(vec![hook]));
    if TOOL_EVENTS.contains(&event) {
        Json::object([("matcher", Json::text("*")), hooks])
    } else {
        Json::object([hooks])
    }
}

/// `settings.json` (`text`, empty when there is none yet) with ccdash's
/// part where it is missing, and how many things were added; what is
/// there is kept, a hook of the user's next to ccdash's.
///
/// # Errors
///
/// `text` is no JSON object, or holds something else where the hooks, the
/// statusline or the environment belong.
pub fn merge(text: &str, extras: Extras) -> Result<(String, usize)> {
    let mut settings: Json = if text.trim().is_empty() {
        Json::Object(Vec::new())
    } else {
        serde_json::from_str(text).context("it is not valid JSON")?
    };
    if !matches!(settings, Json::Object(_)) {
        bail!("it is not a JSON object");
    }
    let mut added = 0;
    let empty = || Json::Object(Vec::new());
    let hooks = settings.member("hooks", empty()).context("no object")?;
    for event in EVENTS {
        let entries = hooks
            .member(event, Json::Array(Vec::new()))
            .context("`hooks` is not an object")?;
        if runs_ccdash(entries) {
            continue;
        }
        let Json::Array(entries) = entries else {
            bail!("`hooks.{event}` is not a list");
        };
        entries.push(hook_entry(event));
        added += 1;
    }
    if extras.status_line {
        match settings.get("statusLine").and_then(|s| s.get("command")) {
            None => {
                let line = Json::object([
                    ("type", Json::text("command")),
                    ("command", Json::text("ccdash tee")),
                    ("padding", Json::Number(0.into())),
                ]);
                *settings
                    .member("statusLine", Json::Null)
                    .context("no object")? = line;
                added += 1;
            }
            Some(Json::String(command)) if !command.starts_with("ccdash tee") => {
                // NOTE: the user's own statusline goes on, behind ccdash's
                let wrapped = Json::String(format!("ccdash tee -- {command}"));
                let line = settings
                    .member("statusLine", Json::Null)
                    .context("no object")?;
                *line.member("command", Json::Null).context("no object")? = wrapped;
                added += 1;
            }
            Some(_) => {}
        }
    }
    if extras.telemetry {
        let env = settings.member("env", empty()).context("no object")?;
        for (name, value) in TELEMETRY {
            if env.get(name).is_none() {
                *env.member(name, Json::Null)
                    .context("`env` is not an object")? = Json::text(value);
                added += 1;
            }
        }
    }
    Ok((to_text(&settings)?, added))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_first_run_adds_every_hook_and_a_second_adds_nothing() {
        let (text, added) = merge("", Extras::default()).unwrap();
        assert_eq!(added, EVENTS.len());
        let settings = parsed(&text);
        assert_eq!(
            settings["hooks"]["Stop"],
            serde_json::json!([{"hooks": [{"type": "command", "command": "ccdash emit", "timeout": 5}]}])
        );
        assert_eq!(settings["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert_eq!(
            settings["hooks"]["PermissionRequest"][0]["hooks"][0],
            serde_json::json!({"type": "command", "command": "ccdash ask", "timeout": 86_400})
        );
        assert!(settings.get("statusLine").is_none() && settings.get("env").is_none());
        let (again, added) = merge(&text, Extras::default()).unwrap();
        assert_eq!((again, added), (text, 0));
    }

    #[test]
    fn what_the_file_holds_stays_in_its_order_with_ccdash_next_to_it() {
        let before = r#"{
    "model": "opus",
    "hooks": {
        "Stop": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": "my-own-hook"
                    }
                ]
            }
        ],
        "PreToolUse": [
            {
                "matcher": "*",
                "hooks": [
                    {
                        "type": "command",
                        "command": "ccdash emit",
                        "timeout": 5
                    }
                ]
            }
        ]
    },
    "alwaysThinkingEnabled": true,
    "cleanupPeriodDays": 99999
}
"#;
        let (text, added) = merge(before, Extras::default()).unwrap();
        assert_eq!(added, EVENTS.len() - 1, "PreToolUse had ccdash's already");
        let keys = |text: &str| {
            let Json::Object(members) = serde_json::from_str::<Json>(text).unwrap() else {
                unreachable!()
            };
            members.into_iter().map(|(k, _)| k).collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&text),
            [
                "model",
                "hooks",
                "alwaysThinkingEnabled",
                "cleanupPeriodDays"
            ]
        );
        let settings = parsed(&text);
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "my-own-hook"
        );
        assert_eq!(
            settings["hooks"]["Stop"][1]["hooks"][0]["command"],
            "ccdash emit"
        );
        assert_eq!(settings["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert_eq!(settings["cleanupPeriodDays"], 99_999);
        // NOTE: written as read: a file with nothing to add comes back byte for byte
        let (same, _) = merge(&text, Extras::default()).unwrap();
        assert_eq!(same, text);
    }

    #[test]
    fn the_statusline_wraps_the_users_and_telemetry_fills_only_what_is_unset() {
        let extras = Extras {
            status_line: true,
            telemetry: true,
        };
        let (text, _) = merge("{}", extras).unwrap();
        let settings = parsed(&text);
        assert_eq!(
            settings["statusLine"],
            serde_json::json!({"type": "command", "command": "ccdash tee", "padding": 0})
        );
        assert_eq!(
            settings["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
            "http://127.0.0.1:7839"
        );
        assert_eq!(merge(&text, extras).unwrap().1, 0);
        let own = r#"{"statusLine": {"type": "command", "command": "my-line --short"},
            "env": {"OTEL_METRICS_EXPORTER": "none", "MINE": "1"}}"#;
        let (text, _) = merge(own, extras).unwrap();
        let settings = parsed(&text);
        assert_eq!(
            settings["statusLine"]["command"],
            "ccdash tee -- my-line --short"
        );
        assert_eq!(
            settings["env"]["OTEL_METRICS_EXPORTER"], "none",
            "the user's stays"
        );
        assert_eq!(settings["env"]["MINE"], "1");
        assert_eq!(settings["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"], "1");
        assert_eq!(merge(&text, extras).unwrap().1, 0);
    }

    #[test]
    fn what_setup_added_goes_again_and_the_users_stays() {
        let all = Extras {
            status_line: true,
            telemetry: true,
        };
        // NOTE: settings that held nothing are left holding nothing
        let (with, added) = merge("", all).unwrap();
        assert_eq!(unmerge(&with).unwrap(), ("{}\n".to_owned(), added));
        let own = r#"{
    "model": "opus",
    "hooks": {
        "Stop": [
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": "my-own-hook"
                    }
                ]
            }
        ],
        "PreToolUse": [
            {
                "matcher": "Bash",
                "hooks": [
                    {
                        "type": "command",
                        "command": "ccdash emit",
                        "timeout": 5
                    },
                    {
                        "type": "command",
                        "command": "mine-in-the-same-list"
                    }
                ]
            }
        ],
        "Notification": []
    },
    "statusLine": {
        "type": "command",
        "command": "my-line --short"
    },
    "env": {
        "OTEL_METRICS_EXPORTER": "none",
        "MINE": "1"
    }
}
"#;
        let (with, _) = merge(own, all).unwrap();
        let (without, removed) = unmerge(&with).unwrap();
        assert!(removed > 17, "{removed}");
        let settings = parsed(&without);
        assert_eq!(
            settings["hooks"]["Stop"][0]["hooks"][0]["command"],
            "my-own-hook"
        );
        assert_eq!(settings["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"],
            serde_json::json!([{"type": "command", "command": "mine-in-the-same-list"}])
        );
        assert_eq!(
            settings["hooks"].as_object().unwrap().len(),
            2,
            "the events ccdash alone had hooks for are gone"
        );
        assert_eq!(settings["statusLine"]["command"], "my-line --short");
        assert_eq!(
            settings["env"],
            serde_json::json!({"OTEL_METRICS_EXPORTER": "none", "MINE": "1"})
        );
        assert!(!without.contains("ccdash"));
        assert_eq!(unmerge(&without).unwrap(), (without, 0));
        assert!(unmerge("[]").is_err());
        // NOTE: an export of the user's own, to their own endpoint, is not ccdash's to take,
        // though it sets some variables the way ccdash would
        let theirs = r#"{"env": {"CLAUDE_CODE_ENABLE_TELEMETRY": "1", "OTEL_METRICS_EXPORTER": "otlp",
            "OTEL_EXPORTER_OTLP_ENDPOINT": "http://collector.example.org:4318"}}"#;
        let (kept, removed) = unmerge(theirs).unwrap();
        assert_eq!((parsed(&kept), removed), (parsed(theirs), 0));
        // NOTE: a file that is there and empty holds nothing of ccdash's
        assert_eq!(unmerge(" \n").unwrap(), (" \n".to_owned(), 0));
    }

    #[test]
    fn a_file_that_is_not_settings_is_refused() {
        assert!(merge("[1, 2]", Extras::default()).is_err());
        assert!(merge("{not json", Extras::default()).is_err());
        assert!(merge(r#"{"hooks": []}"#, Extras::default()).is_err());
        assert!(merge(r#"{"hooks": {"Stop": {}}}"#, Extras::default()).is_err());
    }
}
