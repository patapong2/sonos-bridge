//! Small shared helpers: error alias, time, IPs, percent/hex codecs.

use std::process::Command;

pub type Error = String;
pub type Res<T> = std::result::Result<T, Error>;

pub fn estr<E: std::fmt::Display>(e: E) -> Error {
    e.to_string()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn clamp_i64(v: i64, lo: i64, hi: i64) -> i64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// 10.0.0.0/8, 172.16/12, 192.168/16, 127/8, 169.254/16
pub fn is_rfc1918(ip: &str) -> bool {
    let parts: Vec<&str> = ip.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    let n: Vec<u32> = parts.iter().filter_map(|p| p.parse::<u32>().ok()).collect();
    if n.len() != 4 || n.iter().any(|v| *v > 255) {
        return false;
    }
    n[0] == 10
        || n[0] == 127
        || (n[0] == 192 && n[1] == 168)
        || (n[0] == 172 && (16..=31).contains(&n[1]))
        || (n[0] == 169 && n[1] == 254)
}

/// Best-effort "the IPv4 the Sonos speakers can reach us on".
pub fn lan_address(preferred: &str) -> String {
    if !preferred.is_empty() {
        return preferred.to_string();
    }
    let mut found: Vec<String> = Vec::new();
    if let Ok(out) = Command::new("ip")
        .args(["-4", "-o", "addr", "show", "scope", "global"])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            for line in text.lines() {
                if let Some(pos) = line.find("inet ") {
                    let rest = &line[pos + 5..];
                    let addr: String = rest.chars().take_while(|c| *c != ' ' && *c != '/').collect();
                    if addr.matches('.').count() == 3 && !addr.starts_with("172.17.") {
                        found.push(addr);
                    }
                }
            }
        }
    }
    if found.is_empty() {
        // last resort: ask the kernel which local address a route would use
        let probe = std::net::UdpSocket::bind("0.0.0.0:0");
        if let Ok(sock) = probe {
            if sock.connect(("8.8.8.8", 53)).is_ok() {
                if let Ok(addr) = sock.local_addr() {
                    found.push(addr.ip().to_string());
                }
            }
        }
    }
    found.into_iter().next().unwrap_or_else(|| "127.0.0.1".to_string())
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes.len() % 2 != 0 {
        return None;
    }
    let digit = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = digit(bytes[i])?;
        let lo = digit(bytes[i + 1])?;
        out.push(hi * 16 + lo);
        i += 2;
    }
    Some(out)
}

pub fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// `a=1&b=hello+there` -> [("a","1"), ("b","hello there")]
pub fn parse_query(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = percent_decode(it.next().unwrap_or(""));
        let v = percent_decode(it.next().unwrap_or(""));
        out.push((k, v));
    }
    out
}

pub fn query_get(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

pub fn truthy(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on" | "y"
    )
}

pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let (entity, len) = match tail {
            _ if tail.starts_with("&amp;") => ("&", 5),
            _ if tail.starts_with("&lt;") => ("<", 4),
            _ if tail.starts_with("&gt;") => (">", 4),
            _ if tail.starts_with("&quot;") => ("\"", 6),
            _ if tail.starts_with("&apos;") => ("'", 6),
            _ => ("&", 1),
        };
        out.push_str(entity);
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

pub fn redact(url: &str, token: &str) -> String {
    if token.is_empty() {
        return url.to_string();
    }
    url.replace(token, "<token>")
}

pub fn contains_ci(hay: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.to_lowercase().contains(&needle.to_lowercase())
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(std::time::Duration::from_millis(ms));
}
