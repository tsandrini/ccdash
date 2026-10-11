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
pub fn web_url(host: &str, port: u16, token: &str) -> String {
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("http://{host}:{port}/#token={token}")
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
    fn web_urls_bracket_ipv6() {
        assert_eq!(
            web_url("10.0.0.2", 7837, "t"),
            "http://10.0.0.2:7837/#token=t"
        );
        assert_eq!(
            web_url("fd00::2", 7837, "t"),
            "http://[fd00::2]:7837/#token=t"
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
}
