//! One command parser for every front-end: CLI, unix socket, web API and the
//! GPUI app all land here, so behaviour can never drift between them.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::api;
use crate::log;
use crate::pulse;
use crate::router;
use crate::sonos;
use crate::state::Shared;
use crate::util;

pub const HELP: &str = "\
sonos-bridge - system output device that routes audio to Sonos

usage: sonos-bridge <command> [args]

  status                     what is running right now
  check                      diagnose sink/capture/encoder/speakers
  route <off|sonos|local|both> [player]
                             where audio goes; local = the normal speakers
  play [player]              shorthand for: route sonos [player]
  stop                       shorthand for: route off
  shutdown                   stop the daemon itself (releases the sink and the
                             capture/encoder children); front-ends that talk to
                             the API rather than sending a signal use this
  player <name|udn|all>      speaker(s) to send to, comma separated
  players                    discovered speakers (-d to re-scan)
  discover                   scan the LAN for speakers now
  hw [auto|<sink>]           which real output 'local' uses
  hwsinks | sinks            list outputs known to the sound server
  inputs                     list running app streams (sink inputs)
  move <index> <sink>        send one app stream to another output
  moveall <sink>             send every app stream to another output
  volume <0-100>             Sonos volume when playing, else our sink volume
  sinkvolume <0-100>         level of our own virtual sink
  url                        the stream URL handed to the speakers
  sink <ensure|remove|recreate>
  set <key> <value>          change a config key (see: sonos-bridge config)
  save                       write the config file
  defaults                   print sink/capture defaults
  json                       machine readable state (used by UIs)
";

fn fmt_bytes(n: u64) -> String {
    if n > 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n > 1_000 {
        format!("{:.0} kB", n as f64 / 1_000.0)
    } else {
        format!("{} B", n)
    }
}

fn fmt_uptime(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Split the argument half of a command line, honouring double quotes.
///
/// Only the command line front-end has a shell in front of it. The unix socket,
/// the web API and the desktop panel hand us the raw text, so quotes have to be
/// understood here - otherwise `route sonos "Bedroom Beam"` leaves a literal
/// quote in the speaker name and the router cannot find the player.
///
/// Unquoted words keep the old behaviour: `route sonos Living Room` still joins
/// into one speaker name.
pub(crate) fn split_args(rest: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    for ch in rest.chars() {
        if ch == '"' {
            quoted = !quoted;
            started = true;
        } else if ch.is_whitespace() && !quoted {
            if started {
                out.push(std::mem::take(&mut cur));
                started = false;
            }
        } else {
            cur.push(ch);
            started = true;
        }
    }
    if started {
        out.push(cur);
    }
    out
}

pub fn run(shared: &Arc<Shared>, line: &str) -> (bool, String) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return (true, String::from("ok"));
    }
    let mut parts = trimmed.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.next().unwrap_or("").trim().to_string();
    let toks = split_args(&rest);
    let args: Vec<&str> = toks.iter().map(String::as_str).collect();

    match cmd.as_str() {
        "help" | "-h" | "--help" => return (true, HELP.to_string()),
        "version" => return (true, format!("sonos-bridge {}", crate::VERSION)),

        "json" => return (true, api::state_json(shared)),

        "status" => {
            let cfg = shared.cfg();
            let meter = shared.meter.snapshot();
            let started = shared.stats.started_ms.load(Ordering::Relaxed);
            let active = shared.active();
            let mut out = String::new();
            out.push_str(&format!(
                "sonos-bridge {}  up {}  mode={}\n",
                crate::VERSION,
                fmt_uptime(util::now_ms() - started),
                shared.mode()
            ));
            out.push_str(&format!(
                "output device : {} \"{}\" {} default={}\n",
                cfg.sink_name,
                cfg.sink_description,
                if shared.sink_loaded.load(Ordering::Relaxed) {
                    "registered"
                } else {
                    "not registered"
                },
                pulse::default_sink().unwrap_or_else(|| "?".to_string())
            ));
            out.push_str(&format!(
                "local path    : {} -> {}\n",
                shared.local_kind(),
                shared.hw_sink.read().unwrap().clone()
            ));
            out.push_str(&format!(
                "sonos         : {}{}\n",
                shared.sonos_state(),
                if active.is_empty() {
                    String::new()
                } else {
                    format!(
                        " -> {}",
                        active
                            .iter()
                            .map(|p| p.label())
                            .collect::<Vec<String>>()
                            .join(", ")
                    )
                }
            ));
            out.push_str(&format!(
                "wanted        : {}\n",
                if shared.wanted().is_empty() {
                    "(none)".to_string()
                } else {
                    shared.wanted().join(", ")
                }
            ));
            out.push_str(&format!(
                "stream        : {} ({} {}, {} client(s), produced {} s)\n",
                util::redact(&shared.stream_url(), &cfg.stream_token),
                cfg.stream_format,
                cfg.stream_bitrate,
                shared.stats.stream_clients.load(Ordering::Relaxed),
                // how much audio the encoder has produced in total, in seconds
                // of playback: the byte log divided by the encoded byte rate
                shared.log.total() as f64 / crate::state::stream_bytes_per_s(&cfg) as f64
            ));
            out.push_str(&format!(
                "capture       : backend={} pid={} queue={} dropped={} -> encoder fed={} (silence {} s)\n",
                cfg.capture_backend,
                shared.capture_pid.load(Ordering::Relaxed),
                fmt_bytes(shared.enc_q.len() as u64),
                fmt_bytes(shared.enc_q.lost()),
                fmt_bytes(shared.stats.enc_fed.load(Ordering::Relaxed)),
                shared.stats.pad_bytes.load(Ordering::Relaxed) as f64
                    / (crate::state::stream_bytes_per_s(&cfg) as f64 * 8.0)
            ));
            out.push_str(&format!(
                "audio         : peak={:.2} rms={:.2} active={} captured={} ring recycled={}\n",
                meter.peak,
                meter.rms,
                shared.meter.active(),
                fmt_bytes(shared.stats.bytes_captured.load(Ordering::Relaxed)),
                fmt_bytes(shared.log.dropped())
            ));
            out.push_str(&format!(
                "encoder       : pid={} restarts={}\n",
                shared.encoder_pid.load(Ordering::Relaxed),
                shared.stats.enc_restarts.load(Ordering::Relaxed)
            ));
            let err = shared.error();
            if !err.is_empty() {
                out.push_str(&format!("last error    : {}\n", err));
            }
            let note = shared.last_note.read().unwrap().clone();
            if !note.is_empty() {
                out.push_str(&format!("last note     : {}\n", note));
            }
            out.push_str(&format!(
                "ui            : http://{}:{}/\n",
                cfg.ui_host, cfg.ui_port
            ));
            return (true, out.trim_end().to_string());
        }

        "check" => {
            let checks = api::diagnostics(shared);
            let mut out = String::new();
            for c in checks.iter() {
                out.push_str(&format!(
                    "[{}] {:<16} {}\n",
                    if c.ok { " ok " } else { "FAIL" },
                    c.name,
                    c.detail
                ));
            }
            return (true, out.trim_end().to_string());
        }

        "discover" | "players" => {
            let wait = if cmd == "discover" { 2500 } else { 900 };
            let found = sonos::discover(std::time::Duration::from_millis(wait));
            if !found.is_empty() {
                shared.set_players(found.clone());
            }
            let list = if found.is_empty() { shared.players() } else { found };
            if list.is_empty() {
                return (
                    false,
                    "no Sonos speakers found. Check that this machine and the speakers are on the \
                     same LAN/subnet, and that nothing blocks multicast (udp 1900)."
                        .to_string(),
                );
            }
            let active = shared.active();
            let mut out = String::new();
            for p in list.iter() {
                // the room name alone is ambiguous (a Beam and a Sub in the
                // same room share it), so always show the address and model
                out.push_str(&format!(
                    "{} {:<22} {:<16} {}{}",
                    if active.iter().any(|a| a.udn == p.udn) {
                        ">"
                    } else {
                        " "
                    },
                    if p.name.is_empty() { "(unnamed)" } else { p.name.as_str() },
                    p.ip,
                    if p.model.is_empty() {
                        ""
                    } else {
                        p.model.as_str()
                    },
                    if p.satellite {
                        "  sub/surround - driven by its zone".to_string()
                    } else if p.members.len() > 1 {
                        format!(
                            "  {} of a {}-player group",
                            if p.is_coordinator { "coordinator" } else { "member" },
                            p.members.len()
                        )
                    } else {
                        String::new()
                    }
                ));
                out.push('\n');
            }
            return (true, out.trim_end().to_string());
        }

        "route" => {
            if args.is_empty() {
                return (false, format!("usage: route <off|sonos|local|both> [player]"));
            }
            let player = if args.len() > 1 { Some(args[1..].join(" ")) } else { None };
            match router::set_route(shared, args[0], player.as_deref()) {
                Ok(msg) => (true, msg),
                Err(e) => (false, e),
            }
        }

        "play" => {
            let player = if args.is_empty() { None } else { Some(args.join(" ")) };
            match router::set_route(shared, "sonos", player.as_deref()) {
                Ok(msg) => (true, format!("{} (stream url is in: sonos-bridge url)", msg)),
                Err(e) => (false, e),
            }
        }
        "stop" | "off" => match router::set_route(shared, "off", None) {
            Ok(msg) => (true, msg),
            Err(e) => (false, e),
        },

        "player" => {
            if args.is_empty() {
                return (
                    true,
                    format!(
                        "wanted: {}\nactive: {}",
                        if shared.wanted().is_empty() {
                            "(none)".to_string()
                        } else {
                            shared.wanted().join(", ")
                        },
                        shared
                            .active()
                            .iter()
                            .map(|p| p.label())
                            .collect::<Vec<String>>()
                            .join(", ")
                    ),
                );
            }
            let list: Vec<String> = rest.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            shared.set_wanted(list.clone());
            shared.update_cfg(|c| c.player = rest.clone());
            (true, format!("target speaker(s) = {}", list.join(", ")))
        }

        "hw" => {
            if args.is_empty() {
                return (
                    true,
                    format!(
                        "local output = {} ({})",
                        shared.hw_sink.read().unwrap().clone(),
                        if shared.hw_auto.load(Ordering::Relaxed) { "auto" } else { "manual" }
                    ),
                );
            }
            if args[0] == "auto" {
                shared.hw_auto.store(true, Ordering::Relaxed);
                *shared.hw_sink.write().unwrap() = String::new();
                let pick = router::pick_hw(shared);
                return (true, format!("local output (auto) = {}", pick));
            }
            let name = args.join(" ");
            if !pulse::sink_exists(&name) {
                return (false, format!("no such sink: {} (see: sonos-bridge sinks)", name));
            }
            shared.hw_auto.store(false, Ordering::Relaxed);
            *shared.hw_sink.write().unwrap() = name.clone();
            (true, format!("local output = {}", name))
        }

        "hwsinks" => {
            let cfg = shared.cfg();
            match pulse::hardware_sinks(&cfg.sink_name) {
                Ok(list) if list.is_empty() => (
                    false,
                    "no non-virtual outputs found".to_string(),
                ),
                Ok(list) => (true, list.join("\n")),
                Err(e) => (false, e),
            }
        }
        "sinks" => match pulse::list_sinks() {
            Ok(list) => (true, pulse::describe_sinks(&list).join("\n")),
            Err(e) => (false, e),
        },
        "sources" => match pulse::list_sources() {
            Ok(list) => (
                true,
                list.iter()
                    .map(|s| format!("{} [{}]", s.name, s.description))
                    .collect::<Vec<String>>()
                    .join("\n"),
            ),
            Err(e) => (false, e),
        },

        "inputs" => match pulse::list_sink_inputs() {
            Ok(list) => {
                let map = pulse::short_sinks();
                let mut out = String::new();
                for si in list.iter() {
                    let sink_name = map
                        .iter()
                        .find(|(i, _)| *i == si.sink)
                        .map(|(_, n)| n.clone())
                        .unwrap_or_else(|| format!("#{}", si.sink));
                    out.push_str(&format!(
                        "#{}\t{}\t-> {}\t{}\n",
                        si.index,
                        if si.app.is_empty() { "?" } else { si.app.as_str() },
                        sink_name,
                        si.media
                    ));
                }
                if out.is_empty() {
                    out.push_str("no app streams right now");
                }
                (true, out.trim_end().to_string())
            }
            Err(e) => (false, e),
        },
        "move" => {
            if args.len() < 2 {
                return (false, "usage: move <input-index> <sink-name>".to_string());
            }
            let idx: u32 = match args[0].parse() {
                Ok(v) => v,
                Err(_) => return (false, "input index must be a number".to_string()),
            };
            let sink = args[1..].join(" ");
            match pulse::move_sink_input(idx, &sink) {
                Ok(()) => (true, format!("stream #{} -> {}", idx, sink)),
                Err(e) => (false, e),
            }
        }
        "moveall" => {
            if args.is_empty() {
                return (false, "usage: moveall <sink-name>".to_string());
            }
            let sink = args.join(" ");
            match pulse::list_sink_inputs() {
                Ok(list) => {
                    let mut moved = 0usize;
                    for si in list.iter() {
                        if pulse::move_sink_input(si.index, &sink).is_ok() {
                            moved += 1;
                        }
                    }
                    (true, format!("moved {} stream(s) -> {}", moved, sink))
                }
                Err(e) => (false, e),
            }
        }

        "volume" => {
            if args.is_empty() {
                return (false, "usage: volume <0-100>".to_string());
            }
            let pct: u32 = match args[0].parse() {
                Ok(v) => v,
                Err(_) => return (false, "usage: volume <0-100>".to_string()),
            };
            if pct > 100 {
                return (false, "volume must be 0-100".to_string());
            }
            (true, router::apply_volume(shared, pct).join("\n"))
        }
        "sinkvolume" => {
            if args.is_empty() {
                return (false, "usage: sinkvolume <0-100>".to_string());
            }
            let pct: u32 = match args[0].parse() {
                Ok(v) => v,
                Err(_) => return (false, "usage: sinkvolume <0-100>".to_string()),
            };
            if pct > 100 {
                return (false, "volume must be 0-100".to_string());
            }
            match router::apply_sink_volume(shared, pct) {
                Ok(()) => (true, format!("sink volume = {}%", pct)),
                Err(e) => (false, e),
            }
        }

        "url" => {
            let cfg = shared.cfg();
            let url = shared.stream_url();
            if url.is_empty() {
                return (false, "stream server is not listening".to_string());
            }
            (
                true,
                format!(
                    "{}\n(redacted: {})\nmanual fallback - Sonos app > Music Services > Add a \
                     Radio Station > Add a station by URL, then use the URL above",
                    url,
                    util::redact(&url, &cfg.stream_token)
                ),
            )
        }

        "sink" => {
            let cfg = shared.cfg();
            let what = args.first().copied().unwrap_or("ensure");
            match what {
                "ensure" => match router::ensure_sink(shared, &cfg) {
                    Ok(()) => (true, format!("sink {} is registered", cfg.sink_name)),
                    Err(e) => (false, e),
                },
                "remove" => {
                    router::drop_sink(shared);
                    (true, "sink unregistered".to_string())
                }
                "recreate" => {
                    router::drop_sink(shared);
                    match router::ensure_sink(shared, &cfg) {
                        Ok(()) => (true, format!("sink {} re-registered", cfg.sink_name)),
                        Err(e) => (false, e),
                    }
                }
                _ => (false, "usage: sink <ensure|remove|recreate>".to_string()),
            }
        }

        "set" => {
            if args.len() < 2 {
                return (false, "usage: set <key> <value>".to_string());
            }
            let key = args[0].to_ascii_lowercase();
            let value = args[1..].join(" ");
            let (ok, result) = {
                let mut cfg = shared.cfg();
                let known = cfg.set(&key, &value);
                match known {
                    Ok(true) => {
                        let res = cfg.validate();
                        match res {
                            Ok(()) => {
                                shared.set_cfg_now(cfg.clone());
                                (true, format!("{} = {}", key, value))
                            }
                            Err(e) => (false, e),
                        }
                    }
                    Ok(false) => (false, format!("unknown key: {}", key)),
                    Err(e) => (false, e),
                }
            };
            if ok && matches!(key.as_str(), "ui_port" | "http_port" | "sink_name" | "capture_backend" | "stream_format") {
                shared.note("config change may need a daemon restart to take full effect");
            }
            (ok, result)
        }

        "save" => (true, format!("saved {}", crate::config::Config::save(&shared.cfg()))),
        "config" => {
            let cfg = shared.cfg();
            (
                true,
                format!(
                    "file: {}\nsink_name = {}\ncapture_backend = {}\nrate = {}  channels = {}\n\
                     stream = {}@{} ({} Hz)\nhttp_port = {}  local_backend = {}\nplayer = {}\ndefault_capture_only = {}",
                    crate::config::config_path(),
                    cfg.sink_name,
                    cfg.capture_backend,
                    cfg.rate,
                    cfg.channels,
                    cfg.stream_format,
                    cfg.stream_bitrate,
                    cfg.stream_rate,
                    cfg.http_port,
                    cfg.local_backend,
                    cfg.player,
                    cfg.default_capture_only
                ),
            )
        }
        "defaults" => (
            true,
            "sink_name=sonos_bridge  capture_backend=auto  rate=48000  channels=2\n\
             stream=mp3@192k resampled to 44100 Hz (Sonos friendly)\n\
             local_backend=auto (sound-server loopback, userspace fallback)\n\
             http_port=0 (random free port, published in the runtime dir)"
                .to_string(),
        ),
        // daemon lifecycle for front-ends that cannot send a signal (the web
        // panel and the desktop app talk HTTP only). stopping is deferred so
        // the reply is flushed to the caller before the process comes down;
        // the daemon loop then runs the same cleanup as SIGTERM.
        "shutdown" | "quit" | "exit" => {
            let s = Arc::clone(shared);
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                s.stop();
            });
            (
                true,
                String::from("shutting down: capture and encoder stopped, sink released"),
            )
        }

        "ungroup" => {
            let mut notes = Vec::new();
            for p in shared.active() {
                match sonos::become_standalone(&p) {
                    Ok(()) => notes.push(format!("{} ungrouped", p.name)),
                    Err(e) => notes.push(format!("{}: {}", p.name, e)),
                }
            }
            (true, notes.join("\n"))
        }
        "mute" => {
            let on = util::truthy(args.first().copied().unwrap_or("1"));
            let mut notes = Vec::new();
            let cfg = shared.cfg();
            for p in shared.active() {
                let _ = sonos::set_mute(&p, on);
            }
            match pulse::set_sink_mute(&cfg.sink_name, on) {
                Ok(()) => notes.push(format!("sink mute = {}", on)),
                Err(e) => notes.push(e),
            }
            (true, notes.join("\n"))
        }
        "loglevel" => {
            let lvl: usize = args
                .first()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            log::set_level(lvl.min(3));
            (true, format!("log level = {}", lvl))
        }
        other => (
            false,
            format!("unknown command '{}'. Try: sonos-bridge help", other),
        ),
    }
}
