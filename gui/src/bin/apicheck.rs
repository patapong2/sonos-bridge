//! Command line check for the GUI's std-only half: parse the daemon's JSON
//! without any GPU stack involved.
//!
//!   cargo run --bin apicheck              # print the daemon state
//!   cargo run --bin apicheck -- selftest  # offline parser + locate checks
//!   cargo run --bin apicheck -- check     # daemon diagnostics
//!   cargo run --bin apicheck -- daemon    # where the daemon binary was found
//!   cargo run --bin apicheck -- start     # start it, as the panel button does
//!   cargo run --bin apicheck -- "route both Living Room"

use sonos_bridge_gui::{base_url, checks, command, state};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = args.get(1).cloned().unwrap_or_default();
    let base = base_url("");

    if arg == "selftest" {
        // every std-only part of the GUI, no GPU stack involved
        let mut rc = sonos_bridge_gui::jsonlite::selftest();
        println!();
        rc |= sonos_bridge_gui::locate::selftest();
        println!();
        rc |= sonos_bridge_gui::selftest();
        std::process::exit(rc);
    }

    if arg == "start" {
        // exactly what the panel's "Start daemon" button does
        match sonos_bridge_gui::locate::start_daemon() {
            Ok(msg) => {
                println!("{msg}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    if arg == "daemon" {
        // answers "why did Start daemon say it could not find the binary?"
        match sonos_bridge_gui::locate::find_daemon() {
            Ok(p) => {
                println!("daemon binary: {p}");
                std::process::exit(0);
            }
            Err(tried) => {
                println!("no sonos-bridge binary found. searched, in order:");
                for t in tried {
                    println!("  {t}");
                }
                println!("install it (./install.sh) or set SONOS_BRIDGE_BIN=/path/to/sonos-bridge");
                std::process::exit(1);
            }
        }
    }

    println!("daemon api: {}", base);
    if arg == "check" {
        match checks(&base) {
            Ok(list) => {
                let mut bad = 0;
                for (ok, name, detail) in list.iter() {
                    println!(
                        "[{}] {:<16} {}",
                        if *ok { " ok " } else { "FAIL" },
                        name,
                        detail
                    );
                    if !ok {
                        bad += 1;
                    }
                }
                std::process::exit(if bad == 0 { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("cannot reach the daemon: {}", e);
                std::process::exit(1);
            }
        }
    }

    if !arg.is_empty() && arg != "status" {
        match command(&base, &arg) {
            Ok(text) => {
                println!("{}", text);
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("command failed: {}", e);
                std::process::exit(1);
            }
        }
    }

    match state(&base) {
        Ok(s) => {
            println!("mode          {}", s.mode);
            println!("device        {} \"{}\" {}", s.sink, s.sink_description, if s.sink_loaded { "registered" } else { "absent" });
            println!("local path    {} -> {}", s.local_kind, s.hw_sink);
            println!("sonos         {} ({} client(s))", s.sonos_state, s.stream_clients);
            println!("stream        {}", s.url);
            println!("level         peak={:.2} active={}", s.peak, s.audio_active);
            println!("players       {}", s.players.iter().map(|p| format!("{}@{}", p.name, p.ip)).collect::<Vec<String>>().join(", "));
            if !s.error.is_empty() {
                println!("error         {}", s.error);
            }
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("cannot read daemon state: {}", e);
            std::process::exit(1);
        }
    }
}
