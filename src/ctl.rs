//! Local control socket (the CLI and the desktop app talk to the daemon here).

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::Duration;

use crate::cmds;
use crate::config;
use crate::log;
use crate::state::Shared;
use crate::util::{estr, Res};

pub fn socket_exists() -> bool {
    std::path::Path::new(&config::socket_path()).exists()
}

pub fn reachable() -> bool {
    UnixStream::connect(config::socket_path()).is_ok()
}

pub fn send(line: &str, timeout: Duration) -> Res<(bool, String)> {
    let path = config::socket_path();
    let mut sock = UnixStream::connect(&path).map_err(|e| {
        estr(format!(
            "cannot reach the daemon on {} ({}). Start it with: sonos-bridge start",
            path, e
        ))
    })?;
    sock.set_read_timeout(Some(timeout)).ok();
    sock.set_write_timeout(Some(Duration::from_secs(10))).ok();
    sock.write_all(line.as_bytes()).map_err(estr)?;
    sock.write_all(b"\n").map_err(estr)?;
    sock.flush().map_err(estr)?;
    let _ = sock.shutdown(Shutdown::Write);
    let mut buf: Vec<u8> = Vec::new();
    sock.read_to_end(&mut buf).map_err(estr)?;
    let text = String::from_utf8_lossy(&buf).trim_end().to_string();
    let ok = !text.starts_with("error:");
    Ok((ok, text))
}

fn handle(shared: Arc<Shared>, mut sock: UnixStream) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(20)));
    let mut buf = [0u8; 8192];
    let n = match sock.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    if n == 0 {
        return;
    }
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let line = text.lines().next().unwrap_or("").to_string();
    let (ok, body) = cmds::run(&shared, &line);
    let out = if ok {
        format!("{}\n", body)
    } else {
        format!("error: {}\n", body)
    };
    let _ = sock.write_all(out.as_bytes());
    let _ = sock.flush();
}

pub fn run_server(shared: Arc<Shared>) {
    let path = config::socket_path();
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        use std::os::unix::fs::FileTypeExt;
        if meta.file_type().is_socket() || meta.file_type().is_fifo() {
            if UnixStream::connect(&path).is_ok() {
                shared.set_error("another sonos-bridge daemon is already running");
                return;
            }
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            shared.set_error(&format!("control socket {} : {}", path, e));
            return;
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = listener.set_nonblocking(true);
    log::info("ctl", &format!("control socket on {}", path));

    while shared.running() {
        match listener.accept() {
            Ok((sock, _)) => {
                let s = shared.clone();
                std::thread::spawn(move || handle(s, sock));
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(Duration::from_millis(100));
                } else {
                    log::debug("ctl", &format!("accept: {}", e));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    log::info("ctl", "stopped");
}
