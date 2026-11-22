//! Daemon lifecycle: start every engine, run until a signal arrives, then
//! tear the whole audio graph down in the right order.

use std::sync::Arc;

use crate::config::{self, Config};
use crate::state::Shared;
use crate::util::{estr, Res};
use crate::{capture, ctl, encoder, local, log, router, sonos, stream, sys, web};

pub fn pid_path() -> String {
    format!("{}/daemon.pid", config::runtime_dir())
}

pub fn read_pid() -> Option<u32> {
    let text = std::fs::read_to_string(pid_path()).ok()?;
    let pid: u32 = text.trim().parse().ok()?;
    if std::path::Path::new(&format!("/proc/{}", pid)).exists() {
        Some(pid)
    } else {
        None
    }
}

pub fn start(cfg: Config) -> Res<()> {
    sys::install_signal_handlers();
    log::set_level(cfg.verbose.min(3) as usize);
    let shared = Shared::new(cfg);
    let _ = std::fs::write(pid_path(), format!("{}\n", std::process::id()));

    log::info(
        "daemon",
        &format!(
            "sonos-bridge {} starting (pid {}, ui {}, sink {})",
            crate::VERSION,
            std::process::id(),
            {
                let c = shared.cfg();
                format!("http://{}:{}", c.ui_host, c.ui_port)
            },
            shared.cfg().sink_name
        ),
    );

    let engines: [fn(Arc<Shared>); 8] = [
        router::run,
        capture::run,
        encoder::run,
        local::run,
        sonos::run,
        stream::run_server,
        web::run_server,
        ctl::run_server,
    ];
    let mut jobs: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for &f in engines.iter() {
        let s = shared.clone();
        jobs.push(std::thread::spawn(move || f(s)));
    }

    while shared.running() && !sys::stop_requested() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    log::info("daemon", "shutting down");
    shared.stop();
    sys::kill_children();
    for job in jobs.into_iter() {
        let _ = job.join();
    }
    let _ = std::fs::remove_file(pid_path());
    log::info("daemon", "stopped cleanly");
    Ok(())
}

pub fn stop_daemon() -> Res<String> {
    let pid = match read_pid() {
        Some(p) => p,
        None => {
            let _ = std::fs::remove_file(pid_path());
            return Err(estr("no running daemon found"));
        }
    };
    sys::kill_pid(pid, sys::SIGTERM);
    for _ in 0..40 {
        if !ctl::reachable() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    let _ = std::fs::remove_file(pid_path());
    Ok(format!("stopped daemon (pid {})", pid))
}

/// Launch the daemon in the background (detached via setsid when available).
pub fn spawn_detached(extra: &[String]) -> Res<u32> {
    let exe = std::env::current_exe().map_err(estr)?;
    let logf = config::log_path();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&logf)
        .map_err(estr)?;
    let err_file = file.try_clone().map_err(estr)?;

    let child = if std::process::Command::new("setsid")
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
    {
        let mut cmd = std::process::Command::new("setsid");
        cmd.arg(&exe).arg("daemon");
        for e in extra {
            cmd.arg(e);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(err_file));
        cmd.spawn().map_err(estr)?
    } else {
        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("daemon");
        for e in extra {
            cmd.arg(e);
        }
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(err_file));
        cmd.spawn().map_err(estr)?
    };
    let pid = child.id();
    // give it a moment to create the control socket
    for _ in 0..40 {
        if ctl::reachable() {
            return Ok(pid);
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    log::info(
        "daemon",
        &format!(
            "started pid {} but the control socket is not up yet - check {}",
            pid, logf
        ),
    );
    Ok(pid)
}

pub fn ensure_running(extra: &[String]) -> Res<()> {
    if ctl::reachable() {
        return Ok(());
    }
    let pid = spawn_detached(extra)?;
    log::info("daemon", &format!("started background daemon pid {}", pid));
    Ok(())
}
