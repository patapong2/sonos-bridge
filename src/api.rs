//! JSON snapshots + self-diagnostics, shared by the web UI, the CLI and the
//! GPUI front-end.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::json;
use crate::sonos::Player;
use crate::state::Shared;
use crate::util;

pub fn player_json(p: &Player) -> String {
    let mut o = json::Obj::new();
    o.str("udn", &p.udn);
    o.str("name", &p.name);
    o.str("model", &p.model);
    o.str("ip", &p.ip);
    o.num("port", p.port as i64);
    o.bool("coordinator", p.is_coordinator);
    o.str("group", &p.group_id);
    o.num("members", p.members.len() as i64);
    o.bool("satellite", p.satellite);
    if let Some(v) = p.volume {
        o.num("volume", v as i64);
    } else {
        o.raw("volume", "null");
    }
    o.done()
}

pub fn state_json(shared: &Arc<Shared>) -> String {
    let cfg = shared.cfg();
    let meter = shared.meter.snapshot();
    let started = shared.stats.started_ms.load(Ordering::Relaxed);
    let mut o = json::Obj::new();
    o.str("app", "sonos-bridge");
    o.str("version", crate::VERSION);
    o.str("mode", &shared.mode());
    o.str("sink", &cfg.sink_name);
    o.str("sink_description", &cfg.sink_description);
    o.bool("sink_loaded", shared.sink_loaded.load(Ordering::Relaxed));
    o.str("hw_sink", &shared.hw_sink.read().unwrap().clone());
    o.bool("hw_auto", shared.hw_auto.load(Ordering::Relaxed));
    o.str("local_kind", &shared.local_kind());
    o.str("sonos_state", &shared.sonos_state());
    o.str("capture_backend", &cfg.capture_backend);
    o.str("stream_format", &format!("{} {}", cfg.stream_format, cfg.stream_bitrate));
    o.raw(
        "players",
        &json::arr(
            &shared
                .players()
                .iter()
                .map(|p| player_json(p))
                .collect::<Vec<String>>(),
        ),
    );
    o.raw(
        "wanted",
        &json::arr(
            &shared
                .wanted()
                .iter()
                .map(|w| json::s(w))
                .collect::<Vec<String>>(),
        ),
    );
    o.raw(
        "active",
        &json::arr(
            &shared
                .active()
                .iter()
                .map(|p| json::s(&p.udn))
                .collect::<Vec<String>>(),
        ),
    );
    o.str(
        "url",
        &util::redact(&shared.stream_url(), &cfg.stream_token),
    );
    o.num("stream_port", shared.stream_port.load(Ordering::Relaxed) as i64);
    o.float("peak", meter.peak as f64);
    o.float("rms", meter.rms as f64);
    o.bool("audio_active", shared.meter.active());
    o.num(
        "bytes_captured",
        shared.stats.bytes_captured.load(Ordering::Relaxed) as i64,
    );
    o.num(
        "bytes_streamed",
        shared.stats.bytes_streamed.load(Ordering::Relaxed) as i64,
    );
    o.num("dropped_bytes", shared.log.dropped() as i64);
    // audio the encoder never saw: this is the one that causes audible
    // stutter, so it is worth putting in front of a person rather than in a log
    o.num("feed_lost", shared.enc_q.lost() as i64);
    o.num("enc_fed", shared.stats.enc_fed.load(Ordering::Relaxed) as i64);
    o.num(
        "enc_restarts",
        shared.stats.enc_restarts.load(Ordering::Relaxed) as i64,
    );
    o.num(
        "sonos_plays",
        shared.stats.sonos_plays.load(Ordering::Relaxed) as i64,
    );
    o.num(
        "stream_clients",
        shared.stats.stream_clients.load(Ordering::Relaxed) as i64,
    );
    o.num("capture_pid", shared.capture_pid.load(Ordering::Relaxed) as i64);
    o.num("encoder_pid", shared.encoder_pid.load(Ordering::Relaxed) as i64);
    o.num("local_pid", shared.local_pid.load(Ordering::Relaxed) as i64);
    o.num("uptime_s", ((util::now_ms() - started) / 1000) as i64);
    o.str("error", &shared.error());
    o.str("note", &shared.last_note.read().unwrap().clone());
    o.done()
}

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn new(name: &str, ok: bool, detail: String) -> Check {
        Check {
            name: name.to_string(),
            ok,
            detail,
        }
    }
}

fn which(bin: &str) -> bool {
    std::process::Command::new("sh")
        .args(["-c", &format!("command -v {} >/dev/null 2>&1", bin)])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn diagnostics(shared: &Arc<Shared>) -> Vec<Check> {
    let cfg = shared.cfg();
    let mut out: Vec<Check> = Vec::new();

    let server = crate::pulse::server_name();
    out.push(Check::new(
        "sound server",
        server.is_some(),
        server.unwrap_or_else(|| "pactl cannot reach PulseAudio/PipeWire".to_string()),
    ));

    out.push(Check::new(
        "virtual sink",
        crate::pulse::sink_exists(&cfg.sink_name),
        format!("{} ({})", cfg.sink_name, cfg.sink_description),
    ));
    out.push(Check::new(
        "monitor source",
        crate::pulse::source_exists(&cfg.monitor_source()),
        cfg.monitor_source(),
    ));
    out.push(Check::new(
        "default sink",
        true,
        crate::pulse::default_sink().unwrap_or_else(|| "unknown".to_string()),
    ));
    out.push(Check::new(
        "hardware outputs",
        true,
        crate::pulse::hardware_sinks(&cfg.sink_name)
            .unwrap_or_default()
            .join(", "),
    ));

    let cap_ok = shared.capture_pid.load(Ordering::Relaxed) != 0;
    out.push(Check::new(
        "capture",
        cap_ok,
        format!(
            "backend={} pid={} tools: parec={} pw-record={} ffmpeg={}",
            cfg.capture_backend,
            shared.capture_pid.load(Ordering::Relaxed),
            which("parec"),
            which("pw-record"),
            which(&cfg.ffmpeg)
        ),
    ));

    let enc_ok = shared.encoder_pid.load(Ordering::Relaxed) != 0;
    let lame = std::process::Command::new(&cfg.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("libmp3lame"))
        .unwrap_or(false);
    out.push(Check::new(
        "encoder",
        enc_ok || lame,
        format!(
            "pid={} libmp3lame={} format={}@{}",
            shared.encoder_pid.load(Ordering::Relaxed),
            lame,
            cfg.stream_format,
            cfg.stream_bitrate
        ),
    ));

    let port = shared.stream_port.load(Ordering::Relaxed) as u16;
    let http_ok = if port == 0 {
        false
    } else {
        match crate::net::http_get("127.0.0.1", port, "/check", std::time::Duration::from_millis(800))
        {
            Ok(r) => r.status == 200,
            Err(_) => false,
        }
    };
    out.push(Check::new(
        "stream server",
        http_ok,
        format!(
            "{}:{}{} reachable={}",
            cfg.http_host,
            port,
            shared.stream_path(),
            http_ok
        ),
    ));

    let players = shared.players();
    // the first SSDP scan takes a second or two after the daemon comes up, so
    // an empty list that early is not "no speakers"
    let warming_up = players.is_empty()
        && (util::now_ms() - shared.stats.started_ms.load(Ordering::Relaxed)) < 8_000;
    out.push(Check::new(
        "sonos speakers",
        !players.is_empty(),
        if players.is_empty() {
            if warming_up {
                "the first SSDP scan is still running - try again in a moment".to_string()
            } else {
                "none discovered".to_string()
            }
        } else {
            players
                .iter()
                .map(|p| format!("{} @ {}", p.name, p.ip))
                .collect::<Vec<String>>()
                .join(", ")
        },
    ));
    out.push(Check::new(
        "lan address",
        true,
        util::lan_address(&cfg.http_bind),
    ));
    let ufw = std::process::Command::new("ufw")
        .args(["status", "numbered"])
        .output();
    if let Ok(o) = ufw {
        let text = String::from_utf8_lossy(&o.stdout).to_string();
        let line = text
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        out.push(Check::new("firewall", !line.contains("active"), line));
    }
    out
}

/// Outputs known to the sound server, flagged as hardware or virtual.
pub fn sinks_json(shared: &Arc<Shared>) -> String {
    let cfg = shared.cfg();
    let hw = crate::pulse::hardware_sinks(&cfg.sink_name).unwrap_or_default();
    let sinks = crate::pulse::list_sinks().unwrap_or_default();
    json::arr(
        &sinks
            .iter()
            .map(|s| {
                let mut o = json::Obj::new();
                o.str("name", &s.name);
                o.str("description", &s.description);
                o.str("state", &s.state);
                o.bool("is_default", s.is_default);
                o.bool("mute", s.mute);
                o.num("volume", s.volume_pct as i64);
                o.str("port", &s.active_port);
                o.bool("hardware", hw.iter().any(|h| h == &s.name));
                o.bool("is_ours", s.name == cfg.sink_name);
                o.done()
            })
            .collect::<Vec<String>>(),
    )
}

pub fn checks_json(checks: &[Check]) -> String {
    json::arr(
        &checks
            .iter()
            .map(|c| {
                let mut o = json::Obj::new();
                o.str("name", &c.name);
                o.bool("ok", c.ok);
                o.str("detail", &c.detail);
                o.done()
            })
            .collect::<Vec<String>>(),
    )
}
