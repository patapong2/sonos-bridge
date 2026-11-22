//! Encoder side: PCM in, MP3 (or FLAC) out, via ffmpeg, pushed into the
//! broadcast log that the Sonos players pull.
//!
//! Silence is fed to the encoder whenever nothing is playing: a live radio
//! stream that stops producing data makes Sonos give up after a few seconds,
//! and re-buffering is audible - so we keep the stream alive instead.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::log;
use crate::state::Shared;
use crate::sys;
use crate::util::{estr, Res};

fn codec_args(cfg: &Config, lame: bool) -> Vec<String> {
    if cfg.stream_format == "flac" {
        return vec![
            "-c:a".to_string(),
            "flac".to_string(),
            "-compression_level".to_string(),
            "0".to_string(),
        ];
    }
    if lame {
        vec![
            "-c:a".to_string(),
            "libmp3lame".to_string(),
            "-b:a".to_string(),
            cfg.stream_bitrate.clone(),
        ]
    } else {
        // ffmpeg's built-in mp3 encoder tops out around 160 kbit/s
        let raw = cfg.stream_bitrate.replace('k', "");
        let br: u32 = raw.parse().unwrap_or(128);
        vec![
            "-c:a".to_string(),
            "mp3".to_string(),
            "-b:a".to_string(),
            format!("{}k", br.min(160)),
        ]
    }
}

fn spawn_encoder(cfg: &Config, lame: bool) -> Res<Child> {
    let mut args: Vec<String> = vec![
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
    ];
    args.extend(codec_args(cfg, lame));
    args.extend(vec![
        "-af".to_string(),
        format!("aresample={}", cfg.stream_rate),
        "-ac".to_string(),
        cfg.stream_channels.to_string(),
        "-write_id3v1".to_string(),
        "0".to_string(),
        "-id3v2_version".to_string(),
        "0".to_string(),
        "-fflags".to_string(),
        "+bitexact".to_string(),
        "-f".to_string(),
        cfg.stream_format.clone(),
        "-".to_string(),
    ]);

    let mut child = Command::new(&cfg.ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            estr(format!(
                "{} could not be started ({}). Install: sudo apt install ffmpeg",
                cfg.ffmpeg, e
            ))
        })?;
    if let Some(err) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut err = err;
            let mut buf = [0u8; 4096];
            loop {
                match err.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        for line in text.lines() {
                            if !line.trim().is_empty() {
                                log::debug("encoder", &format!("ffmpeg: {}", line.trim()));
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    sys::track(child.id());
    Ok(child)
}

/// How long the feed waits for real capture data before it calls the
/// source stalled. Bursts from parec/pw-record arrive in coarse pieces, so a
/// short wait would pad the stream with silence that was never played.
pub(crate) const STALL_WAIT: Duration = Duration::from_millis(1000);

fn write_pcm(shared: Arc<Shared>, stdin: Option<ChildStdin>, stop: Arc<AtomicBool>, cfg: Config) {
    let mut stdin = match stdin {
        Some(s) => s,
        None => return,
    };
    let frame_bytes = 2 * cfg.channels as usize;
    // Drain generously. A 20 ms chunk costs a syscall storm per burst and
    // leaves the queue unable to absorb the capture's bursts; 100 ms is still
    // far below the queue capacity, so nothing extra accumulates on the wire.
    let chunk = (cfg.rate as usize / 10).max(1) * frame_bytes; // 100 ms
    let zeros = vec![0u8; chunk];
    let mut buf: Vec<u8> = Vec::with_capacity(chunk * 4);

    while shared.running() && shared.has_sonos() && !stop.load(Ordering::Relaxed) {
        // Wait for the capture rather than padding: the capture produces at
        // the live rate, so an empty queue usually just means the next burst
        // has not arrived. Padding on every empty poll invented ~50 chunks of
        // silence per second, which drove ffmpeg at double speed - the speaker
        // ran ahead of the desktop and every gap was a click.
        let n = shared.enc_q.pop_wait(&mut buf, chunk, STALL_WAIT);
        let written = if n == 0 {
            // only a genuinely stalled source is papered over with silence
            if stdin.write_all(&zeros).is_err() {
                break;
            }
            zeros.len()
        } else {
            if stdin.write_all(&buf[..n]).is_err() {
                break;
            }
            n
        };
        if n == 0 {
            shared.stats.pad_bytes.fetch_add(written as u64, Ordering::Relaxed);
        }
        shared.stats.enc_fed.fetch_add(written as u64, Ordering::Relaxed);
        if stdin.flush().is_err() {
            break;
        }
    }
    let _ = stdin.flush();
}

fn read_out(shared: &Arc<Shared>, child: &mut Child) {
    let out = match child.stdout.take() {
        Some(o) => o,
        None => return,
    };
    let mut reader = std::io::BufReader::with_capacity(32768, out);
    let mut buf = [0u8; 16384];
    // leaving sonos/both mode must also end the encoder: we stop reading and
    // stop feeding stdin, ffmpeg then exits and the outer loop re-evaluates.
    while shared.running() && shared.has_sonos() {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => shared.log.push(&buf[..n]),
            Err(e) => {
                log::debug("encoder", &format!("read error: {}", e));
                break;
            }
        }
    }
}

pub fn run(shared: Arc<Shared>) {
    let mut lame = true;
    let mut backoff: u64 = 1000;
    let mut last_format = String::new();

    while shared.running() {
        if !shared.has_sonos() {
            shared.encoder_pid.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        let cfg = shared.cfg();
        let child = match spawn_encoder(&cfg, lame) {
            Ok(c) => c,
            Err(e) => {
                shared.set_error(&e);
                std::thread::sleep(Duration::from_millis(backoff));
                backoff = (backoff * 2).min(10_000);
                continue;
            }
        };
        let pid = child.id();
        let mut child = child;
        shared.encoder_pid.store(pid, Ordering::Relaxed);
        // Stale PCM must not survive a restart, but the broadcast log must:
        // players are reading out of it right now, and emptying it under them
        // is a guaranteed hole in what they are playing. Only a container
        // change makes the old bytes unusable.
        shared.enc_q.clear();
        if !last_format.is_empty() && last_format != cfg.stream_format {
            shared.log.clear();
        }
        last_format = cfg.stream_format.clone();
        log::info(
            "encoder",
            &format!(
                "{} {} -> {} {}",
                cfg.rate,
                cfg.channels,
                cfg.stream_format,
                cfg.stream_bitrate
            ),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stdin = child.stdin.take();
        let wshared = shared.clone();
        let wstop = stop.clone();
        let wcfg = cfg.clone();
        let writer = std::thread::spawn(move || write_pcm(wshared, stdin, wstop, wcfg));
        let started = Instant::now();

        read_out(&shared, &mut child);

        stop.store(true, Ordering::SeqCst);
        let _ = writer.join();
        sys::untrack(pid);
        sys::kill_pid(pid, sys::SIGTERM);
        let _ = child.wait();
        shared.encoder_pid.store(0, Ordering::Relaxed);

        // only count/ restart when we still *want* an encoder (a mode change
        // tearing it down is not a failure)
        if shared.running() && shared.has_sonos() {
            shared.stats.enc_restarts.fetch_add(1, Ordering::Relaxed);
            if started.elapsed() < Duration::from_secs(3) {
                if lame && cfg.stream_format == "mp3" {
                    lame = false;
                    backoff = 300;
                    log::info(
                        "encoder",
                        "ffmpeg has no libmp3lame; falling back to the built-in mp3 encoder",
                    );
                } else {
                    std::thread::sleep(Duration::from_millis(backoff));
                    backoff = (backoff * 2).min(10_000);
                }
            } else {
                backoff = 1000;
            }
        }
    }
    log::info("encoder", "engine stopped");
}
