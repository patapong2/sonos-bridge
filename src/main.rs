#![allow(dead_code)] // small std-only helpers kept for completeness

//! sonos-bridge - register as a system audio output on Ubuntu and route that
//! audio to Sonos speakers, or straight back to the normal speakers.
//!
//! The daemon owns the audio graph; every front-end (this CLI, the web panel,
//! the GPUI desktop app) drives it through the same command handler.

mod api;
mod capture;
mod cmds;
mod config;
mod ctl;
mod daemon;
mod encoder;
mod json;
mod local;
mod log;
mod net;
mod pulse;
mod queues;
mod router;
mod selftest;
mod sonos;
mod state;
mod stream;
mod sys;
mod util;
mod web;
mod xml;

/// single source of truth: the crate version in Cargo.toml. A hand-written
/// literal here is how a release ends up reporting the wrong number.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `--sink foo` style options, mapped onto config keys.
const FLAGS: [(&str, &str); 14] = [
    ("--sink", "sink_name"),
    ("--desc", "sink_description"),
    ("--backend", "capture_backend"),
    ("--alsa", "alsa_capture"),
    ("--rate", "rate"),
    ("--format", "stream_format"),
    ("--bitrate", "stream_bitrate"),
    ("--local", "local_backend"),
    ("--route", "route"),
    ("--player", "player"),
    ("--ui-port", "ui_port"),
    ("--http-port", "http_port"),
    ("--http-bind", "http_bind"),
    ("--ffmpeg", "ffmpeg"),
];

fn split_args(args: &[String]) -> (Vec<(String, String)>, Vec<String>) {
    let mut opts: Vec<(String, String)> = Vec::new();
    let mut pos: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        if let Some((_, key)) = FLAGS.iter().find(|(flag, _)| *flag == a) {
            if i + 1 < args.len() {
                opts.push((key.to_string(), args[i + 1].clone()));
                i += 2;
            } else {
                eprintln!("{} needs a value", a);
                std::process::exit(2);
            }
        } else if a == "-v" || a == "--verbose" {
            opts.push(("verbose".to_string(), "2".to_string()));
            i += 1;
        } else if a == "-q" || a == "--quiet" {
            opts.push(("verbose".to_string(), "0".to_string()));
            i += 1;
        } else {
            pos.push(args[i].clone());
            i += 1;
        }
    }
    (opts, pos)
}

fn apply(cfg: &mut config::Config, opts: &[(String, String)]) -> Result<(), String> {
    for (k, v) in opts {
        if !cfg.set(k, v)? {
            return Err(format!("unknown option ({}); see: sonos-bridge help", k));
        }
    }
    cfg.validate()
}

fn foreground_config(opts: &[(String, String)]) -> Result<config::Config, String> {
    let mut cfg = config::Config::load()?;
    apply(&mut cfg, opts)?;
    Ok(cfg)
}

/// Commands that make sense to run even if the daemon has to be started first.
const STARTABLE: [&str; 12] = [
    "route", "play", "off", "player", "hw", "sink", "set", "save", "volume", "sinkvolume",
    "move", "moveall",
];

fn client(cmd: &str, rest: &[String]) -> i32 {
    let (opts, positional) = split_args(rest);
    if !ctl::reachable() {
        if !STARTABLE.contains(&cmd) {
            eprintln!(
                "the sonos-bridge daemon is not running.\n  start it:   sonos-bridge start   \
                 (or: systemctl --user start sonos-bridge)\n  then retry: sonos-bridge {}",
                cmd
            );
            return 1;
        }
        if let Err(e) = daemon::ensure_running(&rest.iter().filter(|a| a.starts_with("--")).cloned().collect::<Vec<String>>()) {
            eprintln!("could not start the daemon: {}", e);
            return 1;
        }
    }
    let mut line = String::from(cmd);
    for p in positional.iter() {
        line.push(' ');
        line.push_str(p);
    }
    // options given after a client command are applied as config changes first
    if !opts.is_empty() {
        let mut cfg = match config::Config::load() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}", e);
                return 1;
            }
        };
        if let Err(e) = apply(&mut cfg, &opts) {
            eprintln!("{}", e);
            return 1;
        }
        for (k, v) in opts.iter() {
            let _ = ctl::send(&format!("set {} {}", k, v), std::time::Duration::from_secs(5));
        }
        let _ = ctl::send("save", std::time::Duration::from_secs(5));
    }
    match ctl::send(&line, std::time::Duration::from_secs(25)) {
        Ok((ok, text)) => {
            println!("{}", text);
            if ok {
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("{}", e);
            1
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = match args.get(1) {
        Some(c) => c.clone(),
        None => {
            eprint!("{}", cmds::HELP);
            std::process::exit(2);
        }
    };
    let rest: Vec<String> = args[2..].to_vec();

    let code = match cmd.as_str() {
        "help" | "--help" | "-h" => {
            print!("{}", cmds::HELP);
            0
        }
        "version" | "--version" => {
            println!("sonos-bridge {} (rust, std only)", VERSION);
            0
        }
        "selftest" => selftest::run(),
        "daemon" | "run" => {
            let (opts, _) = split_args(&rest);
            match foreground_config(&opts) {
                Ok(cfg) => match daemon::start(cfg) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("daemon error: {}", e);
                        1
                    }
                },
                Err(e) => {
                    eprintln!("{}", e);
                    1
                }
            }
        }
        "start" => {
            if ctl::reachable() {
                println!("already running (socket {})", config::socket_path());
                0
            } else {
                let flag_only: Vec<String> = rest.iter().filter(|a| a.starts_with("--")).cloned().collect();
                match daemon::spawn_detached(&flag_only) {
                    Ok(pid) => {
                        println!("daemon started (pid {}), log: {}", pid, config::log_path());
                        0
                    }
                    Err(e) => {
                        eprintln!("could not start the daemon: {}", e);
                        1
                    }
                }
            }
        }
        "shutdown" => match daemon::stop_daemon() {
            Ok(msg) => {
                println!("{}", msg);
                0
            }
            Err(e) => {
                eprintln!("{}", e);
                1
            }
        },
        "open" => {
            println!("{}", web::panel_url());
            0
        }
        "gui" => {
            println!(
                "the desktop app is a separate crate (it needs the gpui toolkit):\n  \
                 cd gui && cargo run --release\n\
                 it talks to the daemon over {} and http://127.0.0.1:8688/api",
                config::socket_path()
            );
            0
        }
        other => client(other, &rest),
    };
    std::process::exit(code);
}
