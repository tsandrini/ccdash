use std::{
    net::{IpAddr, UdpSocket},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use qrcode::{
    QrCode,
    render::{svg, unicode::Dense1x2},
};

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

/// Expands `$NAME` and `${NAME}` from the environment, as in paths Home
/// Manager's agenix hands out (`${XDG_RUNTIME_DIR}/agenix/…`). Unset
/// variables stay as written.
pub fn expand_env(s: &str) -> String {
    expand_with(s, |name| std::env::var(name).ok())
}

fn expand_with(s: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, len) = match after.strip_prefix('{') {
            Some(braced) => match braced.find('}') {
                Some(end) => (&braced[..end], end + 2),
                None => ("", 0),
            },
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], end)
            }
        };
        match lookup(name).filter(|_| !name.is_empty()) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[i..=i + len]),
        }
        rest = &rest[i + 1 + len..];
    }
    out.push_str(rest);
    out
}

/// `bytes` random bytes from the kernel, hex-encoded.
pub fn random_hex(bytes: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
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

/// This machine's address on the default route, IPv4 first.
pub fn lan_address() -> Option<IpAddr> {
    [("0.0.0.0:0", "192.0.2.1:9"), ("[::]:0", "[2001:db8::1]:9")]
        .into_iter()
        .find_map(|(bind, probe)| {
            // NOTE: connecting a UDP socket sends nothing; it only picks the source address
            let socket = UdpSocket::bind(bind).ok()?;
            socket.connect(probe).ok()?;
            let ip = socket.local_addr().ok()?.ip();
            (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
        })
}

/// The web UI address with the access token in the fragment.
pub fn web_url(base: &str, token: &str) -> String {
    format!("{}/#token={token}", base.trim_end_matches('/'))
}

/// `http://host:port`, IPv6 addresses bracketed.
pub fn base_url(host: &str, port: u16) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("http://{host}:{port}")
}

/// Compares secrets without an early exit.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A QR code drawn with block characters, light on dark for dark terminals.
pub fn qr_text(data: &str) -> Result<String> {
    Ok(QrCode::new(data)?
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .build())
}

pub fn qr_svg(data: &str) -> Result<String> {
    Ok(QrCode::new(data)?
        .render::<svg::Color>()
        .min_dimensions(240, 240)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

/// Milliseconds since the epoch of an RFC 3339 timestamp such as
/// `2026-10-15T00:59:59.823512+00:00`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let num = |range: std::ops::Range<usize>| s.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !matches!(s.get(10..11)?, "T" | "t" | " ") || !(1..=12).contains(&month) {
        return None;
    }
    let rest = &s[19..];
    let digits = rest
        .strip_prefix('.')
        .map_or(0, |r| r.bytes().take_while(u8::is_ascii_digit).count());
    let millis = match digits {
        0 => 0,
        n => format!("{:0<3}", &rest[1..=n.min(3)]).parse::<i64>().ok()?,
    };
    let zone = &rest[if digits > 0 { digits + 1 } else { 0 }..];
    let offset_min = match zone {
        "Z" | "z" => 0,
        _ => {
            let sign = match zone.get(..1)? {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let h = zone.get(1..3)?.parse::<i64>().ok()?;
            let m = zone.get(4..6)?.parse::<i64>().ok()?;
            sign * (h * 60 + m)
        }
    };
    // NOTE: days from civil, http://howardhinnant.github.io/date_algorithms.html
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_min * 60;
    Some(secs * 1_000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_timestamps() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-10-15T00:59:59.823512+00:00"),
            Some(1_792_025_999_823)
        );
        assert_eq!(
            parse_rfc3339("2026-10-15T02:59:59+02:00"),
            Some(1_792_025_999_000)
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T12:00:00.5Z"),
            Some(1_709_208_000_500)
        );
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn tail_respects_char_boundaries() {
        assert_eq!(tail("short", 10), "short");
        let cut = tail("příliš žluťoučký kůň", 10);
        assert!(cut.len() <= 10);
        assert!(cut.starts_with('…') && cut.ends_with("kůň"));
    }

    #[test]
    fn web_urls_bracket_ipv6() {
        assert_eq!(
            web_url(&base_url("10.0.0.2", 7837), "t"),
            "http://10.0.0.2:7837/#token=t"
        );
        assert_eq!(
            web_url(&base_url("fd00::2", 7837), "t"),
            "http://[fd00::2]:7837/#token=t"
        );
        assert_eq!(
            web_url("https://x.example/", "t"),
            "https://x.example/#token=t"
        );
        assert!(qr_svg("http://x/#token=t").unwrap().starts_with("<?xml"));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate("příliš žluťoučký kůň", 10);
        assert!(cut.len() <= 10);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn env_references_expand() {
        let env = |name: &str| (name == "XDG_RUNTIME_DIR").then(|| "/run/user/1".to_owned());
        assert_eq!(
            expand_with("${XDG_RUNTIME_DIR}/agenix/t", env),
            "/run/user/1/agenix/t"
        );
        assert_eq!(expand_with("$XDG_RUNTIME_DIR/t", env), "/run/user/1/t");
        assert_eq!(expand_with("/a/$NOPE/${NOPE}/b", env), "/a/$NOPE/${NOPE}/b");
        assert_eq!(expand_with("/cost$", env), "/cost$");
        assert_eq!(expand_with("/x/${broken", env), "/x/${broken");
    }
}
