//! Everything that talks to the sound server goes through here.
//!
//! `pactl` is the one interface that behaves identically on PulseAudio and on
//! PipeWire's Pulse emulation, which is exactly what we need: register a
//! virtual sink, watch the default output, move app streams around.

use std::process::Command;

use crate::util::{estr, Res};

#[derive(Clone, Debug, Default)]
pub struct Sink {
    pub name: String,
    pub description: String,
    pub state: String,
    pub is_default: bool,
    pub volume_pct: u32,
    pub mute: bool,
    pub active_port: String,
    pub transient: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Source {
    pub name: String,
    pub description: String,
    pub volume_pct: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SinkInput {
    pub index: u32,
    pub app: String,
    pub media: String,
    pub sink: u32,
    pub mute: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Module {
    pub index: u32,
    pub name: String,
    pub args: String,
}

/// Run pactl; Err only if pactl itself is missing.
fn pactl(args: &[&str]) -> Res<(String, String, bool)> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| {
            estr(format!(
                "pactl could not be run ({}). Install it with: sudo apt install pulseaudio-utils",
                e
            ))
        })?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.success(),
    ))
}

fn pactl_ok(args: &[&str]) -> Res<String> {
    let (stdout, stderr, ok) = pactl(args)?;
    if !ok {
        return Err(estr(format!(
            "pactl {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(stdout)
}

fn first_percent(line: &str) -> u32 {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut best: Option<u32> = None;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'%' {
                if let Ok(v) = line[i..j].parse::<u32>() {
                    best = Some(v);
                    break;
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    best.unwrap_or(0)
}

fn block_prop(block: &str, key: &str) -> Option<String> {
    let needle = format!("{} = \"", key);
    let start = block.find(&needle)? + needle.len();
    let rest = &block[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Split `pactl list <what>` output into entity blocks.
fn blocks(text: &str, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        if line.starts_with(prefix) {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push_str(line);
            cur.push('\n');
        } else if !cur.is_empty() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn field(block: &str, key: &str) -> Option<String> {
    let needle = format!("{}:", key);
    for line in block.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix(&needle) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

pub fn list_sinks() -> Res<Vec<Sink>> {
    let text = pactl_ok(&["list", "sinks"])?;
    let def = default_sink();
    let mut out = Vec::new();
    for block in blocks(&text, "Sink #") {
        let mut s = Sink::default();
        s.name = field(&block, "Name").unwrap_or_default();
        s.description = field(&block, "Description").unwrap_or_default();
        s.state = field(&block, "State").unwrap_or_default();
        s.active_port = field(&block, "Active Port").unwrap_or_default();
        s.mute = field(&block, "Mute").unwrap_or_default() == "yes";
        s.transient = field(&block, "Transient").unwrap_or_default() == "yes";
        if let Some(vol) = block.lines().find(|l| l.trim_start().starts_with("Volume:")) {
            s.volume_pct = first_percent(vol);
        }
        s.is_default = Some(s.name.clone()) == def;
        if !s.name.is_empty() {
            out.push(s);
        }
    }
    Ok(out)
}

pub fn list_sources() -> Res<Vec<Source>> {
    let text = pactl_ok(&["list", "sources"])?;
    let mut out = Vec::new();
    for block in blocks(&text, "Source #") {
        let mut s = Source::default();
        s.name = field(&block, "Name").unwrap_or_default();
        s.description = field(&block, "Description").unwrap_or_default();
        if let Some(vol) = block.lines().find(|l| l.trim_start().starts_with("Volume:")) {
            s.volume_pct = first_percent(vol);
        }
        if !s.name.is_empty() {
            out.push(s);
        }
    }
    Ok(out)
}

pub fn list_sink_inputs() -> Res<Vec<SinkInput>> {
    let text = pactl_ok(&["list", "sink-inputs"])?;
    let mut out = Vec::new();
    for block in blocks(&text, "Sink Input #") {
        let mut si = SinkInput::default();
        si.index = field(&block, "Index")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        si.sink = field(&block, "Sink")
            .and_then(|v| v.parse().ok())
            .unwrap_or(u32::MAX);
        si.mute = field(&block, "Mute").unwrap_or_default() == "yes";
        si.app = block_prop(&block, "application.name").unwrap_or_default();
        si.media = block_prop(&block, "media.name").unwrap_or_default();
        out.push(si);
    }
    Ok(out)
}

pub fn list_modules() -> Vec<Module> {
    let mut out = Vec::new();
    if let Ok(text) = pactl_ok(&["list", "short", "modules"]) {
        for line in text.lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.is_empty() {
                continue;
            }
            let mut m = Module::default();
            m.index = cols[0].trim().parse().unwrap_or(0);
            if cols.len() > 1 {
                m.name = cols[1].to_string();
            }
            if cols.len() > 2 {
                m.args = cols[2..].join(" ");
            }
            out.push(m);
        }
    }
    out
}

pub fn server_name() -> Option<String> {
    let (stdout, _, ok) = pactl(&["info"]).ok()?;
    if !ok {
        return None;
    }
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Server Name:") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

pub fn default_sink() -> Option<String> {
    let (stdout, _, ok) = pactl(&["info"]).ok()?;
    if !ok {
        return None;
    }
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("Default Sink:") {
            let v = rest.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// (index, name) for every sink - used to label app streams.
pub fn short_sinks() -> Vec<(u32, String)> {
    match pactl_ok(&["list", "short", "sinks"]) {
        Ok(text) => text
            .lines()
            .filter_map(|l| {
                let cols: Vec<&str> = l.split('\t').collect();
                if cols.len() >= 2 {
                    Some((cols[0].trim().parse().unwrap_or(0), cols[1].trim().to_string()))
                } else {
                    None
                }
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn sink_exists(name: &str) -> bool {
    match pactl_ok(&["list", "short", "sinks"]) {
        Ok(text) => text
            .lines()
            .any(|l| l.split('\t').nth(1).map(|s| s.trim()) == Some(name)),
        Err(_) => false,
    }
}

pub fn source_exists(name: &str) -> bool {
    match pactl_ok(&["list", "short", "sources"]) {
        Ok(text) => text
            .lines()
            .any(|l| l.split('\t').nth(1).map(|s| s.trim()) == Some(name)),
        Err(_) => false,
    }
}

/// `pactl load-module <name> <arg> <arg> ...`
///
/// The module arguments must be separate argv tokens (pactl joins them and the
/// sound server does its own quote-aware parsing), so a value with spaces has
/// to be embedded in double quotes inside its own token.
pub fn load_module(name: &str, args: &[&str]) -> Res<u32> {
    let mut argv: Vec<&str> = vec!["load-module", name];
    argv.extend_from_slice(args);
    let out = pactl_ok(&argv)?;
    out.trim()
        .parse::<u32>()
        .map_err(|_| estr(format!("unexpected load-module output: {:?}", out.trim())))
}

/// helper: Vec<String> -> Vec<&str>
pub fn refs(v: &[String]) -> Vec<&str> {
    v.iter().map(|s| s.as_str()).collect()
}

pub fn unload_module(index: u32) -> Res<()> {
    pactl_ok(&["unload-module", &index.to_string()])?;
    Ok(())
}

pub fn find_modules(name_part: &str, arg_part: &str) -> Vec<u32> {
    list_modules()
        .into_iter()
        .filter(|m| {
            m.name.contains(name_part)
                && (arg_part.is_empty() || m.args.contains(arg_part) || m.args.contains(&format!("={}", arg_part)))
        })
        .map(|m| m.index)
        .collect()
}

pub fn set_default_sink(name: &str) -> Res<()> {
    pactl_ok(&["set-default-sink", name])?;
    Ok(())
}

pub fn set_sink_volume(name: &str, pct: u32) -> Res<()> {
    pactl_ok(&["set-sink-volume", name, &format!("{}%", pct)])?;
    Ok(())
}

pub fn set_sink_mute(name: &str, mute: bool) -> Res<()> {
    pactl_ok(&["set-sink-mute", name, if mute { "1" } else { "0" }])?;
    Ok(())
}

pub fn move_sink_input(index: u32, sink_name: &str) -> Res<()> {
    pactl_ok(&["move-sink-input", &index.to_string(), sink_name])?;
    Ok(())
}

/// Real playback devices (skip our own sink and anything virtual).
pub fn hardware_sinks(own_sink: &str) -> Res<Vec<String>> {
    let short = pactl_ok(&["list", "short", "sinks"])?;
    let mods = list_modules();
    let blacklist = [
        "module-null-sink",
        "module-loopback",
        "module-combine-sink",
        "module-remap-sink",
        "module-always-sink",
        "module-dump",
    ];
    let mut out: Vec<String> = Vec::new();
    for line in short.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 {
            continue;
        }
        let name = cols[1].trim().to_string();
        if name.is_empty() || name == own_sink || name.ends_with(".monitor") {
            continue;
        }
        let mod_index: Option<u32> = cols.get(2).and_then(|c| c.trim().parse().ok());
        if let Some(mi) = mod_index {
            if let Some(m) = mods.iter().find(|m| m.index == mi) {
                if blacklist.contains(&m.name.as_str()) || m.args.contains("sink_name=") {
                    continue;
                }
            }
        }
        out.push(name);
    }
    Ok(out)
}

/// Human readable one-liner per sink, for CLI output.
pub fn describe_sinks(sinks: &[Sink]) -> Vec<String> {
    sinks
        .iter()
        .map(|s| {
            format!(
                "{}{}  vol={}{}  [{}]",
                if s.is_default { "* " } else { "  " },
                s.name,
                s.volume_pct,
                if s.mute { " MUTED" } else { "" },
                if s.description.is_empty() {
                    s.state.as_str()
                } else {
                    s.description.as_str()
                }
            )
        })
        .collect()
}
