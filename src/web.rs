//! Tiny HTTP server: the web control panel + the JSON API every front-end
//! (browser, GPUI app, scripts) uses.

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use crate::api;
use crate::cmds;
use crate::config;
use crate::log;
use crate::net;
use crate::state::Shared;
use crate::util;

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/app.js");

fn send(w: &mut std::net::TcpStream, status: u16, reason: &str, ctype: &str, body: &str) {
    let _ = net::write_response(w, status, reason, ctype, body);
}

pub fn run_server(shared: Arc<Shared>) {
    let cfg = shared.cfg();
    let listener = match TcpListener::bind((cfg.ui_host.as_str(), cfg.ui_port)) {
        Ok(l) => l,
        Err(e) => {
            shared.set_error(&format!(
                "web UI could not bind {}:{} ({})",
                cfg.ui_host, cfg.ui_port, e
            ));
            return;
        }
    };
    log::info(
        "ui",
        &format!("control panel on http://{}:{}/", cfg.ui_host, cfg.ui_port),
    );
    let _ = listener.set_nonblocking(true);
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
                    log::debug("ui", &format!("accept: {}", e));
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
    log::info("ui", "stopped");
}

fn handle(shared: Arc<Shared>, sock: std::net::TcpStream) {
    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));
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
    let cfg = shared.cfg();
    let pairs = util::parse_query(&req.query);

    // if the panel is exposed beyond localhost, require the shared token
    let local_bind = cfg.ui_host == "127.0.0.1" || cfg.ui_host == "localhost" || cfg.ui_host == "::1";
    if !local_bind {
        let token = util::query_get(&pairs, "t").unwrap_or_default();
        if token != cfg.stream_token {
            send(&mut w, 403, "Forbidden", "text/plain", "add ?t=<token>\n");
            return;
        }
    }

    match req.path.as_str() {
        "/" | "/index.html" => send(&mut w, 200, "OK", "text/html; charset=utf-8", INDEX),
        "/style.css" => send(&mut w, 200, "OK", "text/css; charset=utf-8", STYLE),
        "/app.js" => send(&mut w, 200, "OK", "text/javascript; charset=utf-8", SCRIPT),
        "/api/state" => send(&mut w, 200, "OK", "application/json", &api::state_json(&shared)),
        "/api/check" => send(
            &mut w,
            200,
            "OK",
            "application/json",
            &api::checks_json(&api::diagnostics(&shared)),
        ),
        "/api/players" => send(
            &mut w,
            200,
            "OK",
            "application/json",
            &crate::json::arr(
                &shared
                    .players()
                    .iter()
                    .map(|p| api::player_json(p))
                    .collect::<Vec<String>>(),
            ),
        ),
        "/api/cmd" => {
            let line = util::query_get(&pairs, "line").unwrap_or_default();
            let (ok, text) = cmds::run(&shared, &line);
            send(
                &mut w,
                if ok { 200 } else { 400 },
                if ok { "OK" } else { "Bad Request" },
                "text/plain; charset=utf-8",
                &format!("{}\n", text),
            );
        }
        "/api/route" => {
            let mode = util::query_get(&pairs, "mode").unwrap_or_else(|| "off".to_string());
            let player = util::query_get(&pairs, "player");
            let (ok, text) = cmds::run(&shared, &format!("route {} {}", mode, player.unwrap_or_default()));
            send(
                &mut w,
                if ok { 200 } else { 400 },
                if ok { "OK" } else { "Bad Request" },
                "text/plain; charset=utf-8",
                &format!("{}\n", text),
            );
        }
        "/api/volume" => {
            let v = util::query_get(&pairs, "v").unwrap_or_default();
            let (ok, text) = cmds::run(&shared, &format!("volume {}", v));
            send(
                &mut w,
                if ok { 200 } else { 400 },
                if ok { "OK" } else { "Bad Request" },
                "text/plain; charset=utf-8",
                &format!("{}\n", text),
            );
        }
        "/api/discover" => {
            let (ok, text) = cmds::run(&shared, "discover");
            send(
                &mut w,
                if ok { 200 } else { 400 },
                if ok { "OK" } else { "Bad Request" },
                "text/plain; charset=utf-8",
                &format!("{}\n", text),
            );
        }
        "/api/url" => {
            let full = util::query_get(&pairs, "full").unwrap_or_default();
            let url = shared.stream_url();
            let body = if util::truthy(&full) {
                url
            } else {
                util::redact(&url, &cfg.stream_token)
            };
            send(&mut w, 200, "OK", "text/plain; charset=utf-8", &format!("{}\n", body));
        }
        "/api/sinks" => send(
            &mut w,
            200,
            "OK",
            "application/json",
            &api::sinks_json(&shared),
        ),
        other => send(
            &mut w,
            404,
            "Not Found",
            "text/plain",
            &format!("no such path: {}\n", other),
        ),
    }
}

pub fn panel_url() -> String {
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(_) => config::Config::default(),
    };
    format!("http://{}:{}/", cfg.ui_host, cfg.ui_port)
}
