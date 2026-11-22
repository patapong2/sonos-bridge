//! The routing brain: keep the registered virtual sink and the system default
//! output in sync with the selected mode, and apply volume.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::config::{Config, ROUTES};
use crate::log;
use crate::pulse;
use crate::sonos;
use crate::state::Shared;
use crate::util::{estr, Res};

pub fn sink_args(cfg: &Config) -> Vec<String> {
    let desc = cfg.sink_description.replace('"', "'");
    vec![
        format!("sink_name={}", cfg.sink_name),
        format!("sink_properties=device.description=\"{}\"", desc),
    ]
}

pub fn ensure_sink(shared: &Arc<Shared>, cfg: &Config) -> Res<()> {
    if pulse::sink_exists(&cfg.sink_name) {
        shared.sink_loaded.store(true, Ordering::Relaxed);
        // adopt a sink left behind by an earlier run so we can still clean up
        let held = shared.sink_mod.lock().unwrap().take();
        if held.is_none() {
            let needle = format!("sink_name={}", cfg.sink_name);
            if let Some(idx) = pulse::find_modules("module-null-sink", &needle).first().copied() {
                *shared.sink_mod.lock().unwrap() = Some(idx);
            }
        } else {
            *shared.sink_mod.lock().unwrap() = held;
        }
        return Ok(());
    }
    let args = sink_args(cfg);
    let idx = pulse::load_module("module-null-sink", &pulse::refs(&args))?;
    *shared.sink_mod.lock().unwrap() = Some(idx);
    shared.sink_loaded.store(true, Ordering::Relaxed);
    log::info(
        "router",
        &format!(
            "registered output \"{}\" as {} (#{}); pick it in Sound Settings or set it as default",
            cfg.sink_description, cfg.sink_name, idx
        ),
    );
    Ok(())
}

pub fn drop_sink(shared: &Arc<Shared>) {
    let idx = shared.sink_mod.lock().unwrap().take();
    if let Some(idx) = idx {
        let _ = pulse::unload_module(idx);
        log::info("router", &format!("unregistered sink module #{}", idx));
    }
    shared.sink_loaded.store(false, Ordering::Relaxed);
}

/// The real playback device we hand audio to.
pub fn pick_hw(shared: &Arc<Shared>) -> String {
    let stored = shared.hw_sink.read().unwrap().clone();
    if !shared.hw_auto.load(Ordering::Relaxed) && !stored.is_empty() {
        return stored;
    }
    let cfg = shared.cfg();
    match pulse::hardware_sinks(&cfg.sink_name) {
        Ok(list) => {
            if list.is_empty() {
                return stored;
            }
            let def = pulse::default_sink().unwrap_or_default();
            let pick = if list.iter().any(|s| s == &def) {
                def
            } else {
                list[0].clone()
            };
            *shared.hw_sink.write().unwrap() = pick.clone();
            pick
        }
        Err(_) => stored,
    }
}

/// Single place where the mode changes, so the CLI and the UI behave the same.
pub fn set_route(shared: &Arc<Shared>, mode: &str, player: Option<&str>) -> Res<String> {
    if !ROUTES.contains(&mode) {
        return Err(estr(format!("route must be one of {:?}", ROUTES)));
    }
    let cfg = shared.cfg();
    if mode != "off" {
        ensure_sink(shared, &cfg)?;
    }
    if let Some(p) = player {
        if !p.is_empty() {
            let list: Vec<String> = p
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            shared.set_wanted(list);
            shared.update_cfg(|c| c.player = p.to_string());
        }
    }
    if mode == "off" {
        // hand the default output back before the sink disappears, and say so:
        // a silent failure here leaves the desktop with no audible output.
        let hw = pick_hw(shared);
        let def = pulse::default_sink();
        log::debug(
            "router",
            &format!("route off: default={:?} hand-back-target={:?}", def, hw),
        );
        if def.as_deref() == Some(cfg.sink_name.as_str()) {
            if hw.is_empty() {
                log::quiet(
                    "router",
                    "no hardware output to hand the default back to - check Sound Settings",
                );
            } else {
                match pulse::set_default_sink(&hw) {
                    Ok(()) => log::info("router", &format!("default output back to {}", hw)),
                    Err(e) => log::quiet(
                        "router",
                        &format!("could not hand the default back to {}: {}", hw, e),
                    ),
                }
            }
        }
        crate::local::teardown(shared);
        sonos::stop_all(shared);
        drop_sink(shared);
        shared.enc_q.clear();
        shared.local_q.clear();
    }
    shared.set_mode(mode);
    shared.update_cfg(|c| c.route = mode.to_string());
    Ok(format!("route = {}", mode))
}

/// Volume semantics: Sonos player volume when Sonos is playing, otherwise the
/// level of our own sink (which is also what feeds the encoder).
pub fn apply_volume(shared: &Arc<Shared>, pct: u32) -> Vec<String> {
    let pct = pct.min(100);
    let mut notes = Vec::new();
    let active = shared.active();
    if shared.has_sonos() && !active.is_empty() {
        for p in active.iter() {
            match sonos::set_volume(p, pct) {
                Ok(()) => notes.push(format!("{} volume -> {}%", p.name, pct)),
                Err(e) => notes.push(format!("{}: {}", p.name, e)),
            }
        }
    } else {
        let cfg = shared.cfg();
        match pulse::set_sink_volume(&cfg.sink_name, pct) {
            Ok(()) => notes.push(format!("sink {} volume -> {}%", cfg.sink_name, pct)),
            Err(e) => notes.push(e),
        }
    }
    notes
}

pub fn apply_sink_volume(shared: &Arc<Shared>, pct: u32) -> Res<()> {
    let cfg = shared.cfg();
    pulse::set_sink_volume(&cfg.sink_name, pct)
}

pub fn run(shared: Arc<Shared>) {
    while shared.running() {
        let cfg = shared.cfg();
        let mode = shared.mode();
        if mode == "off" {
            if shared.sink_loaded.load(Ordering::Relaxed) {
                drop_sink(&shared);
            }
            std::thread::sleep(std::time::Duration::from_millis(1200));
            continue;
        }
        if let Err(e) = ensure_sink(&shared, &cfg) {
            shared.set_error(&e);
        } else if cfg.default_capture_only {
            if let Some(def) = pulse::default_sink() {
                if def != cfg.sink_name {
                    match pulse::set_default_sink(&cfg.sink_name) {
                        Ok(()) => log::info(
                            "router",
                            &format!("default output -> {} (was {})", cfg.sink_name, def),
                        ),
                        Err(e) => shared.set_error(&e),
                    }
                }
            }
        }
        if shared.hw_auto.load(Ordering::Relaxed) {
            pick_hw(&shared);
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    drop_sink(&shared);
    log::info("router", "engine stopped");
}
