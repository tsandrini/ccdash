use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_owned())
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".to_owned())
}

pub fn pid_alive(pid: Option<u32>) -> bool {
    pid.is_some_and(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
}

/// Cuts `s` to at most `max` bytes on a char boundary, marking the cut with `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Keeps the last `max` bytes of `s` on a char boundary, marking the cut with `…`.
pub fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut start = s.len() - max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("short", 10), "short");
        let cut = tail("příliš žluťoučký kůň", 10);
        assert!(cut.len() <= 10);
        assert!(cut.starts_with('…') && cut.ends_with("kůň"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate("příliš žluťoučký kůň", 10);
        assert!(cut.len() <= 10);
        assert!(cut.ends_with('…'));
    }
}
