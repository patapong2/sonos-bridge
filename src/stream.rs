//! The HTTP end point the Sonos players pull: a chunked live MP3/FLAC stream,
//! guarded by a shared token and restricted to private addresses.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config;
use crate::log;
use crate::net;
use crate::state::Shared;
use crate::util;

pub fn run_server(shared: Arc<Shared>) {
    let cfg = shared.cfg();
    let listener = match TcpListener::bind((cfg.http_host.as_str(), cfg.http_port)) {
        Ok(l) => l,
        Err(e) => {
            shared.set_error(&format!(
                "stream server could not bind {}:{} ({})",
                cfg.http_host, cfg.http_port, e
            ));
            return;
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    shared.stream_port.store(port as u32, Ordering::Relaxed);
    let _ = std::fs::write(config::port_file_path(), format!("{}\n", port));
    log::info(
        "stream",
        &format!(
            "listening on {}:{}{} (token protected)",
            cfg.http_host,
            port,
            shared.stream_path()
        ),
    );
    let _ = listener.set_nonblocking(true);

    while shared.running() {
        match listener.accept() {
            Ok((sock, addr)) => {
                let s = shared.clone();
                std::thread::spawn(move || handle(s, sock, addr));
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(Duration::from_millis(80));
                } else {
                    log::debug("stream", &format!("accept: {}", e));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    log::info("stream", "stopped");
}

fn handle(shared: Arc<Shared>, sock: TcpStream, addr: SocketAddr) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(6)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(20)));
    let _ = sock.set_nodelay(true);

    let peer = addr.ip().to_string();
    let clone = match sock.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut reader = net::buffered(clone);
    let req = match net::read_request(&mut reader) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut w = match sock.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };

    if req.path == "/check" {
        let _ = net::write_response(&mut w, 200, "OK", "text/plain", "sonos-bridge\n");
        return;
    }
    let cfg = shared.cfg();
    let is_stream = req.path == shared.stream_path()
        || req.path == "/stream.mp3"
        || req.path == "/stream.flac";
    if !is_stream {
        let _ = net::write_response(&mut w, 404, "Not Found", "text/plain", "not found\n");
        return;
    }
    let pairs = util::parse_query(&req.query);
    let token = util::query_get(&pairs, "t").unwrap_or_default();
    if token != cfg.stream_token {
        log::info("stream", &format!("rejected {}: wrong token", peer));
        let _ = net::write_response(&mut w, 403, "Forbidden", "text/plain", "forbidden\n");
        return;
    }
    if !util::is_rfc1918(&peer) {
        log::info(
            "stream",
            &format!("rejected {}: not a private/lan address", peer),
        );
        let _ = net::write_response(&mut w, 403, "Forbidden", "text/plain", "forbidden\n");
        return;
    }
    let need_align = !req.path.ends_with(".flac");
    serve(shared, &mut w, &req.path, &peer, need_align);
}

fn serve(shared: Arc<Shared>, w: &mut TcpStream, path: &str, peer: &str, need_align: bool) {
    let ctype = if path.ends_with(".flac") {
        "audio/flac"
    } else {
        "audio/mpeg"
    };
    if let Err(e) = net::write_stream_head(w, ctype) {
        log::debug("stream", &format!("header to {}: {}", peer, e));
        return;
    }
    let clients = shared.stats.stream_clients.fetch_add(1, Ordering::SeqCst) + 1;
    log::info(
        "stream",
        &format!("{} is pulling {} ({} client(s))", peer, path, clients),
    );

    let mut pos = shared.log.total();
    let mut synced = !need_align;
    let mut last_data = Instant::now();
    let mut sent: u64 = 0;

    while shared.running() {
        let (np, data) = shared.log.read_from(pos, 8192);
        if data.is_empty() {
            if np != pos {
                // This reader fell off the end of the ring: it stopped reading
                // for longer than the whole buffer holds. Jumping forward would
                // splice a hole into the middle of the audio it is already
                // playing, which is the stutter we are trying to avoid - so end
                // the connection instead and let the player re-buffer cleanly.
                // If it never comes back, the sonos engine re-arms the URI.
                log::info(
                    "stream",
                    &format!(
                        "{} fell {} bytes behind the live edge, closing so it re-buffers",
                        peer,
                        np - pos
                    ),
                );
                break;
            }
            if last_data.elapsed() > Duration::from_secs(45) {
                log::info("stream", &format!("{} went idle, closing", peer));
                break;
            }
            shared.log.wait_new(pos, Duration::from_millis(250));
            continue;
        }
        let mut out: &[u8] = &data[..];
        if !synced {
            match net::first_frame(&data) {
                Some(off) => {
                    out = &data[off..];
                    synced = true;
                }
                None => {
                    // nothing decodable in this window - drop it
                    pos = np;
                    continue;
                }
            }
        }
        if !out.is_empty() {
            if let Err(e) = net::write_chunk(w, out) {
                // worth seeing at default verbosity: this is the speaker
                // hanging up on us, which is what a silent room looks like
                log::info("stream", &format!("write to {} failed: {}", peer, e));
                break;
            }
            sent += out.len() as u64;
            shared
                .stats
                .bytes_streamed
                .fetch_add(out.len() as u64, Ordering::Relaxed);
            last_data = Instant::now();
        }
        pos = np;
    }
    net::write_last_chunk(w);
    shared.stats.stream_clients.fetch_sub(1, Ordering::SeqCst);
    log::info("stream", &format!("{} disconnected after {} bytes", peer, sent));
}
