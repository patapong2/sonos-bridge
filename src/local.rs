//! Local path: send the captured monitor audio to the real speakers.
//!
//! Preferred route is a sound-server loopback (zero copy, no extra latency).
//! If the server refuses that, we do the same thing in userspace through a
//! second ffmpeg process feeding the sink directly.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::log;
use crate::pulse;
use crate::state::Shared;
use crate::sys;
use crate::util::{estr, Res};

fn ensure_loopback(shared: &Shared, cfg: &Config, hw: &str) -> Res<()> {
    let monitor = cfg.monitor_source();
    if !pulse::source_exists(&monitor) {
        return Err(estr(format!("source {} does not exist", monitor)));
    }
    if !pulse::sink_exists(hw) {
        return Err(estr(format!("sink {} does not exist", hw)));
    }
    {
        let held = *shared.loop_mod.lock().unwrap();
        if let Some(idx) = held {
            if pulse::list_modules().iter().any(|m| m.index == idx) {
                return Ok(());
            }
        }
    }
    if let Some(idx) = pulse::find_modules("module-loopback", &monitor).first().copied() {
        *shared.loop_mod.lock().unwrap() = Some(idx);
        return Ok(());
    }
    let args = vec![
        format!("source={}", monitor),
        format!("sink={}", hw),
        "latency_msec=20".to_string(),
        "rate_msec_max=40".to_string(),
    ];
    let idx = pulse::load_module("module-loopback", &pulse::refs(&args))?;
    *shared.loop_mod.lock().unwrap() = Some(idx);
    log::info(
        "local",
        &format!("loopback module #{}: {} -> {}", idx, monitor, hw),
    );
    Ok(())
}

fn drop_loopback(shared: &Shared) {
    let idx = shared.loop_mod.lock().unwrap().take();
    if let Some(idx) = idx {
        let _ = pulse::unload_module(idx);
        log::info("local", &format!("unloaded loopback module #{}", idx));
    }
}

fn write_pcm(shared: Arc<Shared>, stdin: Option<ChildStdin>, stop: Arc<AtomicBool>, cfg: Config) {
    let mut stdin = match stdin {
        Some(s) => s,
        None => return,
    };
    let frame_bytes = 2 * cfg.channels as usize;
    let chunk = (cfg.rate as usize / 50).max(1) * frame_bytes;
    let zeros = vec![0u8; chunk];
    let mut buf: Vec<u8> = Vec::with_capacity(chunk * 4);
    while shared.running() && shared.has_local() && !stop.load(Ordering::Relaxed) {
        // wait for the capture instead of padding it, so the local path stays
        // in step with the clock instead of running ahead of it
        let n = shared.local_q.pop_wait(&mut buf, chunk, crate::encoder::STALL_WAIT);
        if n == 0 {
            if stdin.write_all(&zeros).is_err() {
                break;
            }
        } else if stdin.write_all(&buf[..n]).is_err() {
            break;
        }
        if stdin.flush().is_err() {
            break;
        }
    }
    let _ = stdin.flush();
}

fn pipe_session(shared: &Arc<Shared>, cfg: &Config, hw: &str) {
    let args: Vec<String> = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
        "-nostdin".to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-ar".to_string(),
        cfg.rate.to_string(),
        "-ac".to_string(),
        cfg.channels.to_string(),
        "-i".to_string(),
        "pipe:0".to_string(),
        "-vn".to_string(),
        "-map".to_string(),
        "0:a".to_string(),
        "-af".to_string(),
        format!("aresample={}:async=1:first_ok=1", cfg.rate),
        "-f".to_string(),
        "pulse".to_string(),
        hw.to_string(),
    ];
    let mut child: Child = match Command::new(&cfg.ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            shared.set_error(&estr(format!(
                "local playback via {} failed: {} (install ffmpeg or enable loopback)",
                cfg.ffmpeg, e
            )));
            return;
        }
    };
    if let Some(mut err) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        for line in text.lines() {
                            if !line.trim().is_empty() {
                                log::debug("local", &format!("ffmpeg: {}", line.trim()));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    let pid = child.id();
    sys::track(pid);
    shared.local_pid.store(pid, Ordering::Relaxed);
    shared.local_q.clear();
    log::info("local", &format!("feeding {} directly (loopback unavailable)", hw));

    let stop = Arc::new(AtomicBool::new(false));
    let stdin = child.stdin.take();
    let wshared = shared.clone();
    let wstop = stop.clone();
    let wcfg = cfg.clone();
    let writer = std::thread::spawn(move || write_pcm(wshared, stdin, wstop, wcfg));

    while shared.running() && shared.has_local() && shared.local_kind() == "pipe" {
        match child.try_wait() {
            Ok(Some(status)) => {
                log::info("local", &format!("playback process exited: {}", status));
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => {
                log::debug("local", &format!("try_wait: {}", e));
                break;
            }
        }
    }
    stop.store(true, Ordering::SeqCst);
    let _ = writer.join();
    sys::untrack(pid);
    sys::kill_pid(pid, sys::SIGTERM);
    let _ = child.wait();
    shared.local_pid.store(0, Ordering::Relaxed);
}

pub fn teardown(shared: &Shared) {
    drop_loopback(shared);
    shared.set_local_kind("none");
    shared.local_q.clear();
}

pub fn run(shared: Arc<Shared>) {
    while shared.running() {
        if !shared.has_local() {
            teardown(&shared);
            std::thread::sleep(Duration::from_millis(400));
            continue;
        }
        let cfg = shared.cfg();
        if cfg.local_backend == "none" {
            teardown(&shared);
            std::thread::sleep(Duration::from_millis(1000));
            continue;
        }
        if !pulse::source_exists(&cfg.monitor_source()) {
            shared.set_local_kind("none");
            std::thread::sleep(Duration::from_millis(1200));
            continue;
        }
        let hw = crate::router::pick_hw(&shared);
        if hw.is_empty() {
            shared.set_error("no hardware output sink found to pass audio through to");
            std::thread::sleep(Duration::from_millis(3000));
            continue;
        }

        let order: &[&str] = match cfg.local_backend.as_str() {
            "loopback" => &["loopback"],
            "pipe" => &["pipe"],
            _ => &["loopback", "pipe"],
        };
        let mut active = false;
        for kind in order.iter() {
            if *kind == "loopback" {
                match ensure_loopback(&shared, &cfg, &hw) {
                    Ok(()) => {
                        shared.set_local_kind("loopback");
                        active = true;
                        break;
                    }
                    Err(e) => log::debug("local", &format!("loopback not usable: {}", e)),
                }
            } else {
                shared.set_local_kind("pipe");
                pipe_session(&shared, &cfg, &hw);
                active = true;
                break;
            }
        }
        if !active {
            shared.set_local_kind("none");
            std::thread::sleep(Duration::from_millis(3000));
        } else {
            std::thread::sleep(Duration::from_millis(1200));
        }
    }
    teardown(&shared);
    log::info("local", "engine stopped");
}
