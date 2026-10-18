//! The on-disk outbox: hook and telemetry messages the collector could not
//! deliver, replayed in order when the hub link comes back. One JSON
//! message per line; bounded, newest dropped once full.

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
};

use tracing::warn;

use crate::protocol::ToHub;

pub struct Queue {
    path: PathBuf,
    max_bytes: u64,
    dropped: u64,
}

impl Queue {
    pub fn open(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            dropped: 0,
        }
    }

    /// Keeps `msg` for later if it is worth keeping.
    pub fn park(&mut self, msg: &ToHub) {
        if !msg.durable() {
            return;
        }
        let Ok(line) = serde_json::to_string(msg) else {
            return;
        };
        self.append(&[line]);
    }

    /// Every parked message, oldest first; the file is emptied.
    pub fn take(&mut self) -> Vec<String> {
        let text = fs::read_to_string(&self.path).unwrap_or_default();
        let lines: Vec<String> = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        if !lines.is_empty() {
            let _ = fs::remove_file(&self.path);
        }
        lines
    }

    /// Puts back lines `take` handed out that were not delivered, ahead of
    /// anything parked since.
    pub fn restore(&mut self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }
        let later = self.take();
        self.append(lines);
        self.append(&later);
    }

    pub fn len_bytes(&self) -> u64 {
        fs::metadata(&self.path).map_or(0, |m| m.len())
    }

    fn append(&mut self, lines: &[String]) {
        let size = self.len_bytes();
        let adding: u64 = lines.iter().map(|l| l.len() as u64 + 1).sum();
        if size + adding > self.max_bytes {
            self.dropped += lines.len() as u64;
            if self.dropped == lines.len() as u64 {
                warn!("outbox full ({size} bytes), dropping events until the hub is back");
            }
            return;
        }
        if let Some(dir) = self.path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path);
        match file {
            Ok(mut file) => {
                let mut buf = String::with_capacity(adding as usize);
                for line in lines {
                    buf.push_str(line);
                    buf.push('\n');
                }
                if let Err(e) = file.write_all(buf.as_bytes()) {
                    warn!("outbox: {e}");
                }
            }
            Err(e) => warn!("outbox {}: {e}", self.path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Envelope, Source};
    use serde_json::json;

    fn envelope(n: i64) -> ToHub {
        ToHub::Envelope {
            envelope: Envelope {
                v: 1,
                source: Source::Hook,
                ts: n,
                pid: None,
                remote_id: None,
                applied_title: None,
                payload: json!({ "n": n }),
            },
        }
    }

    #[test]
    fn parks_durable_messages_in_order_and_restores_leftovers() {
        let dir = std::env::temp_dir().join(format!("ccdash-queue-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut q = Queue::open(dir.join("outbox.ndjson"), 10_000);
        q.park(&envelope(1));
        q.park(&ToHub::Niri {
            event: crate::state::NiriEvent::Focus(None),
        });
        q.park(&envelope(2));
        let lines = q.take();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains(r#""ts":1"#) && lines[1].contains(r#""ts":2"#));
        assert!(q.take().is_empty());

        q.park(&envelope(3));
        q.restore(&lines[1..]);
        let again = q.take();
        assert_eq!(again.len(), 2);
        assert!(again[0].contains(r#""ts":2"#) && again[1].contains(r#""ts":3"#));

        let mut small = Queue::open(dir.join("small.ndjson"), 150);
        small.park(&envelope(4));
        small.park(&envelope(5));
        assert_eq!(small.take().len(), 1, "the second one did not fit");
        fs::remove_dir_all(dir).unwrap();
    }
}
