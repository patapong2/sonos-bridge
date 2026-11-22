//! std-only client for the sonos-bridge daemon (HTTP JSON API).
//! Shared by the GPUI front-end and the `apicheck` command line tool.

pub mod jsonlite;
pub mod locate;

use std::io::Read;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use jsonlite::{parse, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct PlayerInfo {
    pub name: String,
    pub udn: String,
    pub model: String,
    pub ip: String,
    pub coordinator: bool,
    pub members: i64,
    /// sub or surround: driven by its zone's coordinator, not playable itself
    pub satellite: bool,
    pub volume: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub version: String,
    pub mode: String,
    pub sink: String,
    pub sink_description: String,
    pub hw_sink: String,
    pub local_kind: String,
    pub sonos_state: String,
    pub capture_backend: String,
    pub stream_format: String,
    pub url: String,
    pub error: String,
    pub note: String,
    pub sink_loaded: bool,
    pub audio_active: bool,
    pub peak: f64,
    pub rms: f64,
    pub stream_port: i64,
    pub uptime_s: i64,
    pub stream_clients: i64,
    pub dropped_bytes: i64,
    /// PCM the capture produced but the encoder never received. Non-zero means
    /// audio is being thrown away between the sound server and ffmpeg, which is
    /// heard as stutter that worsens until the player gives up.
    pub feed_lost: i64,
    pub capture_pid: i64,
    pub encoder_pid: i64,
    pub local_pid: i64,
    pub players: Vec<PlayerInfo>,
    pub wanted: Vec<String>,
    pub active: Vec<String>,
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Where the daemon's panel/API lives.
pub fn base_url(explicit: &str) -> String {
    if !explicit.is_empty() {
        return explicit.trim_end_matches('/').to_string();
    }
    if let Ok(v) = std::env::var("SONOS_BRIDGE_UI") {
        if !v.is_empty() {
            return v.trim_end_matches('/').to_string();
        }
    }
    let path = match std::env::var("XDG_CONFIG_HOME") {
        Ok(d) if !d.is_empty() => format!("{}/sonos-bridge/config", d),
        _ => format!(
            "{}/.config/sonos-bridge/config",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string())
        ),
    };
    let mut host = String::from("127.0.0.1");
    let mut port = String::from("8688");
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            if let Some(pos) = line.find('=') {
                let k = line[..pos].trim();
                let v = line[pos + 1..].trim();
                match k {
                    "ui_host" if !v.is_empty() && v != "0.0.0.0" => host = v.to_string(),
                    "ui_port" if !v.is_empty() => port = v.to_string(),
                    _ => {}
                }
            }
        }
    }
    if host == "localhost" {
        host = String::from("127.0.0.1");
    }
    format!("http://{}:{}", host, port)
}

fn split_base(base: &str) -> Result<(String, u16), String> {
    let without = base
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let (host, port) = match without.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(8688)),
        None => (without.to_string(), 8688),
    };
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    Ok((host, port))
}

pub fn http_get(base: &str, path: &str, timeout: Duration) -> Result<String, String> {
    let (host, port) = split_base(base)?;
    let addrs: Vec<std::net::SocketAddr> = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .collect();
    if addrs.is_empty() {
        return Err(format!("cannot resolve {}", host));
    }
    let mut sock = TcpStream::connect_timeout(&addrs[0], timeout).map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\nAccept: */*\r\n\r\n",
        path, host, port
    );
    use std::io::Write;
    sock.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    sock.flush().map_err(|e| e.to_string())?;
    let mut raw: Vec<u8> = Vec::new();
    sock.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&raw).to_string();
    let body = match text.find("\r\n\r\n") {
        Some(p) => text[p + 4..].to_string(),
        None => String::new(),
    };
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status >= 400 {
        return Err(format!("http {}: {}", status, body.trim()));
    }
    Ok(body)
}

pub fn state(base: &str) -> Result<Snapshot, String> {
    let body = http_get(base, "/api/state", Duration::from_millis(1500))?;
    let v = parse(&body)?;
    Ok(snapshot_of(&v))
}

fn snapshot_of(v: &Value) -> Snapshot {
    let mut s = Snapshot {
        version: v.strf("version"),
        mode: v.strf("mode"),
        sink: v.strf("sink"),
        sink_description: v.strf("sink_description"),
        hw_sink: v.strf("hw_sink"),
        local_kind: v.strf("local_kind"),
        sonos_state: v.strf("sonos_state"),
        capture_backend: v.strf("capture_backend"),
        stream_format: v.strf("stream_format"),
        url: v.strf("url"),
        error: v.strf("error"),
        note: v.strf("note"),
        sink_loaded: v.boolf("sink_loaded"),
        audio_active: v.boolf("audio_active"),
        peak: v.f64f("peak"),
        rms: v.f64f("rms"),
        stream_port: v.f64f("stream_port") as i64,
        uptime_s: v.f64f("uptime_s") as i64,
        stream_clients: v.f64f("stream_clients") as i64,
        dropped_bytes: v.f64f("dropped_bytes") as i64,
        feed_lost: v.f64f("feed_lost") as i64,
        capture_pid: v.f64f("capture_pid") as i64,
        encoder_pid: v.f64f("encoder_pid") as i64,
        local_pid: v.f64f("local_pid") as i64,
        ..Default::default()
    };
    for p in v.arrf("players").iter() {
        s.players.push(PlayerInfo {
            name: p.strf("name"),
            udn: p.strf("udn"),
            model: p.strf("model"),
            ip: p.strf("ip"),
            coordinator: p.boolf("coordinator"),
            members: p.f64f("members") as i64,
            satellite: p.boolf("satellite"),
            volume: p.opt_f64("volume").map(|x| x as u32),
        });
    }
    s.wanted = v
        .arrf("wanted")
        .iter()
        .filter_map(|x| x.as_str().map(|s| s.to_string()))
        .collect();
    s.active = v
        .arrf("active")
        .iter()
        .filter_map(|x| x.as_str().map(|s| s.to_string()))
        .collect();
    s
}

pub fn command(base: &str, line: &str) -> Result<String, String> {
    http_get(
        base,
        &format!("/api/cmd?line={}", encode(line)),
        Duration::from_millis(4000),
    )
    .map(|s| s.trim().to_string())
}

pub fn checks(base: &str) -> Result<Vec<(bool, String, String)>, String> {
    let body = http_get(base, "/api/check", Duration::from_millis(3000))?;
    let v = parse(&body)?;
    let mut out = Vec::new();
    for c in v.as_arr().cloned().unwrap_or_default().iter() {
        out.push((c.boolf("ok"), c.strf("name"), c.strf("detail")));
    }
    Ok(out)
}

// ---- what the panel should show --------------------------------------- //
//
// Kept here rather than in the view so the tick-mark rules are testable
// offline: they are the part of the panel that decides whether a click
// "did anything", and getting them wrong looks like a routing failure.

/// Do these names mean "every speaker"? The daemon stores the word `all`
/// literally, and an empty list means the same thing.
pub fn all_of(list: &[String]) -> bool {
    list.is_empty() || list.iter().any(|w| w == "all" || w == "*")
}

/// Is audio currently going to Sonos?
pub fn routing_to_sonos(mode: &str) -> bool {
    mode == "sonos" || mode == "both"
}

/// Whether a speaker row is ticked.
///
/// A local pick is shown immediately (the daemon only answers on the next
/// poll); otherwise the daemon's wanted list decides. Routing to "all" ticks
/// every row, and routing off ticks nothing.
pub fn picked(selected: &[String], wanted: &[String], mode: &str, name: &str) -> bool {
    if !selected.is_empty() && !all_of(selected) {
        return selected.iter().any(|s| s == name);
    }
    if all_of(wanted) {
        routing_to_sonos(mode)
    } else {
        wanted.iter().any(|w| w == name)
    }
}

/// Offline checks for the display rules above.
pub fn selftest() -> i32 {
    let mut fails = 0usize;
    let mut total = 0usize;
    let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).collect() };
    let mut t = |name: &str, ok: bool| {
        total += 1;
        if !ok {
            fails += 1;
            println!("  FAIL  {}", name);
        } else {
            println!("  ok    {}", name);
        }
    };

    t("all: empty list means every speaker", all_of(&s(&[])));
    t("all: the word all", all_of(&s(&["all"])));
    t("all: wildcard", all_of(&s(&["*"])));
    t("all: a named speaker is not all", !all_of(&s(&["Bedroom"])));

    // daemon routes to every speaker: all rows tick while routed
    let wanted_all = s(&["all"]);
    t("all routed: row ticked in sonos mode", picked(&s(&[]), &wanted_all, "sonos", "Bedroom"));
    t("all routed: row ticked in both mode", picked(&[], &wanted_all, "both", "Kitchen"));
    t("off: nothing ticked even though a target is stored", !picked(&s(&[]), &wanted_all, "off", "Bedroom"));
    t("local mode: sonos rows not ticked", !picked(&s(&[]), &wanted_all, "local", "Bedroom"));

    // daemon routes to one speaker: only that row ticks
    let wanted_one = s(&["Bedroom"]);
    t("one routed: that row ticked", picked(&s(&[]), &wanted_one, "sonos", "Bedroom"));
    t("one routed: other rows clear", !picked(&s(&[]), &wanted_one, "sonos", "Kitchen"));
    t("stored target still ticks when not routed", picked(&s(&[]), &wanted_one, "off", "Bedroom"));

    // a fresh click answers at once, before the poll comes back
    let sel = s(&["Kitchen"]);
    t("local pick ticks immediately", picked(&sel, &wanted_all, "sonos", "Kitchen"));
    t("local pick clears the others", !picked(&sel, &wanted_all, "sonos", "Bedroom"));
    t("local pick of all behaves like no pick", picked(&s(&["all"]), &s(&[]), "sonos", "Bedroom"));

    println!();
    if fails == 0 {
        println!("panel: all {} checks passed", total);
        0
    } else {
        println!("panel: {}/{} checks FAILED", fails, total);
        1
    }
}
