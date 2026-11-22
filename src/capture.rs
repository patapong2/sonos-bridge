//! Capture side: read raw PCM out of the virtual sink's monitor.
//!
//! We do not link libpulse: `parec` / `pw-record` / `ffmpeg` all give us the
//! same s16le stream and are present on essentially every Ubuntu desktop.

use std::io::{BufReader, Read};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::config::Config;
use crate::log;
use crate::state::Shared;
use crate::sys;
use crate::util::{estr, Res};

struct Candidate {
    name: &'static str,
    bin: String,
    args: Vec<String>,
}

fn pcm_out_tail(cfg: &Config) -> Vec<String> {
    vec![
        "-vn".to_string(),
        "-map".to_string(),
        "0:a".to_string(),
        "-ac".to_string(),
        cfg.channels.to_string(),
        "-ar".to_string(),
        cfg.rate.to_string(),
        "-f".to_string(),
        "s16le".to_string(),
        "-".to_string(),
    ]
}

fn candidates(cfg: &Config) -> Vec<Candidate> {
    let monitor = cfg.monitor_source();
    let mut out: Vec<Candidate> = Vec::new();

    let parec = || Candidate {
        name: "parec",
        bin: "parec".to_string(),
        args: vec![
            "--format=s16le".to_string(),
            format!("--rate={}", cfg.rate),
            format!("--channels={}", cfg.channels),
            format!("--device={}", monitor),
            "--client-name=sonos-bridge".to_string(),
            "--stream-name=sonos-bridge-capture".to_string(),
        ],
    };
    let ff_pulse = || Candidate {
        name: "ffmpeg(pulse)",
        bin: cfg.ffmpeg.clone(),
        args: {
            let mut a = vec![
                "-hide_banner".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
                "-f".to_string(),
                "pulse".to_string(),
                "-i".to_string(),
                monitor.clone(),
            ];
            a.extend(pcm_out_tail(cfg));
            a
        },
    };
    let pwrec = || Candidate {
        name: "pw-record",
        bin: "pw-record".to_string(),
        args: {
            let mut a = vec![
                "--rate".to_string(),
                cfg.rate.to_string(),
                "--channels".to_string(),
                cfg.channels.to_string(),
                "--format".to_string(),
                "s16le".to_string(),
                "--target".to_string(),
                monitor.clone(),
            ];
            a.extend(vec!["--file".to_string(), "-".to_string()]);
            a
        },
    };
    let alsa = || Candidate {
        name: "alsa",
        bin: cfg.ffmpeg.clone(),
        args: {
            let mut a = vec![
                "-hide_banner".to_string(),
                "-loglevel".to_string(),
                "error".to_string(),
                "-f".to_string(),
                "alsa".to_string(),
                "-i".to_string(),
                cfg.alsa_capture.clone(),
            ];
            a.extend(pcm_out_tail(cfg));
            a
        },
    };

    match cfg.capture_backend.as_str() {
        "parec" => out.push(parec()),
        "ffmpeg" => out.push(ff_pulse()),
        "pw-record" => out.push(pwrec()),
        "alsa" => {
            if !cfg.alsa_capture.is_empty() {
                out.push(alsa())
            }
        }
        _ => {
            if !cfg.alsa_capture.is_empty() {
                out.push(alsa());
            }
            out.push(parec());
            out.push(ff_pulse());
            out.push(pwrec());
        }
    }
    out
}

fn drain_stderr(pipe: Option<ChildStderr>, label: &'static str) {
    if let Some(mut err) = pipe {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        for line in text.lines() {
                            if !line.trim().is_empty() {
                                log::debug("capture", &format!("{}: {}", label, line.trim()));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
}

fn spawn(c: &Candidate) -> Res<Child> {
    let mut cmd = Command::new(&c.bin);
    cmd.args(&c.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        estr(format!(
            "{} could not be started ({}). Install: sudo apt install pulseaudio-utils ffmpeg",
            c.bin, e
        ))
    })?;
    let err = child.stderr.take();
    drain_stderr(err, c.name);
    sys::track(child.id());
    Ok(child)
}

fn pump(shared: &Arc<Shared>, mut child: Child, frame_bytes: usize) {
    let out = match child.stdout.take() {
        Some(o) => o,
        None => return,
    };
    let mut reader = BufReader::with_capacity(65536, out);
    let mut acc: Vec<u8> = Vec::with_capacity(16384);
    let mut tmp = [0u8; 8192];

    // stop as soon as the route no longer needs us, so the child can go away
    while shared.running() && shared.capture_needed() {
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                acc.extend_from_slice(&tmp[..n]);
                let complete = acc.len() - (acc.len() % frame_bytes);
                if complete == 0 {
                    continue;
                }
                let slice = &acc[..complete];
                let mode = shared.mode();
                if mode == "sonos" || mode == "both" {
                    shared.enc_q.push(slice);
                }
                if shared.local_kind() == "pipe" {
                    shared.local_q.push(slice);
                }
                shared
                    .stats
                    .bytes_captured
                    .fetch_add(complete as u64, Ordering::Relaxed);
                shared.meter.sample(slice, shared.log.dropped());
                acc.drain(..complete);
            }
            Err(e) => {
                log::debug("capture", &format!("read error: {}", e));
                break;
            }
        }
    }
    sys::untrack(child.id());
    sys::kill_pid(child.id(), sys::SIGTERM);
    let _ = child.wait();
}

pub fn run(shared: Arc<Shared>) {
    let mut backoff: u64 = 1000;
    while shared.running() {
        if !shared.capture_needed() {
            shared.capture_pid.store(0, Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(400));
            continue;
        }
        let cfg = shared.cfg();
        let cands = candidates(&cfg);
        if cands.is_empty() {
            shared.set_error("no capture backend available: install pulseaudio-utils or ffmpeg");
            std::thread::sleep(std::time::Duration::from_millis(5000));
            continue;
        }
        let mut ran = false;
        for c in cands.iter() {
            match spawn(c) {
                Ok(child) => {
                    log::info(
                        "capture",
                        &format!("{}: {} {}", c.name, c.bin, c.args.join(" ")),
                    );
                    shared.capture_pid.store(child.id(), Ordering::Relaxed);
                    ran = true;
                    pump(&shared, child, 2 * cfg.channels as usize);
                    shared.capture_pid.store(0, Ordering::Relaxed);
                    break;
                }
                Err(e) => log::debug("capture", &format!("{}: {}", c.name, e)),
            }
        }
        if !ran && shared.running() {
            shared.set_error("could not start any capture backend (is the sink present?)");
        }
        if !shared.running() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(backoff));
        backoff = (backoff * 2).min(10_000);
    }
    log::info("capture", "engine stopped");
}
