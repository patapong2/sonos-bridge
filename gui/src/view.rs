//! The window: daemon lifecycle, route switch, player picker, live status.
//!
//! gpui is confined to this file and `main.rs`. The data model, the JSON
//! parser and the daemon client are std-only and covered by
//! `cargo run --bin apicheck -- selftest`, so the window can be changed without
//! touching anything that decides what the audio does.
//!
//! Rules we hold ourselves to:
//!   - nothing blocking on the UI thread: the daemon is polled from a plain
//!     thread into a mutex, and commands run on their own thread;
//!   - light theme only (matches the rest of the project's tooling);
//!   - the panel must be useful while the daemon is down, not blank.

use gpui::{
    div, prelude::*, px, rgb, App, ClickEvent, Context, Entity, FontWeight, IntoElement,
    MouseButton, ParentElement, Render, Timer, Window, WindowControlArea,
};
use sonos_bridge_gui::{
    checks as api_checks, command, locate, picked, routing_to_sonos, state, PlayerInfo, Snapshot,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BG: u32 = 0xf4f5f7;
const CARD: u32 = 0xffffff;
const INK: u32 = 0x14161a;
const MUTED: u32 = 0x6b7280;
const LINE: u32 = 0xe3e6ea;
const BAR: u32 = 0xe9ebef;
const ACCENT: u32 = 0x2e77f5;
const OK: u32 = 0x1c7c4a;
const WARN: u32 = 0xb45309;
const BAD: u32 = 0xb42318;

/// Every button does one of these; keeping them as data avoids closures that
/// capture the entity, which is the part of gpui that moves between releases.
#[derive(Clone, Copy, PartialEq)]
enum Act {
    Start,
    Stop,
    Scan,
    Checks,
    Mode(&'static str),
    Player(usize),
    VolDown,
    VolUp,
}

const MODES: [(&'static str, &'static str); 4] = [
    ("off", "Off"),
    ("local", "PC speakers"),
    ("sonos", "Sonos"),
    ("both", "Both"),
];

pub struct BridgeView {
    base: String,
    /// written by the poll thread, read by render
    latest: Arc<Mutex<Option<Snapshot>>>,
    /// results of commands posted from worker threads
    notes: Arc<Mutex<Vec<String>>>,
    diag: Arc<Mutex<Option<Vec<(bool, String, String)>>>>,
    snap: Snapshot,
    online: bool,
    selected: Vec<String>,
    vol: u32,
    status: String,
    show_diag: bool,
}

impl BridgeView {
    pub fn new(base: String, cx: &mut Context<Self>) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let notes = Arc::new(Mutex::new(Vec::new()));
        let diag = Arc::new(Mutex::new(None));

        // Poll the daemon off the UI thread. A missing daemon is a normal
        // state (this window is how you start it), so failures just mean
        // "offline" and are not logged as errors.
        {
            let latest = latest.clone();
            let poll_base = base.clone();
            std::thread::spawn(move || loop {
                let s = state(&poll_base).ok();
                if let Ok(mut g) = latest.lock() {
                    *g = s;
                }
                std::thread::sleep(Duration::from_millis(700));
            });
        }

        // Wake the view on a timer; render copies whatever the thread stored.
        {
            cx.spawn(async move |this, cx| loop {
                Timer::after(Duration::from_millis(400)).await;
                this.update(cx, |v, cx| {
                    v.pull();
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }

        BridgeView {
            base,
            latest,
            notes,
            diag,
            snap: Snapshot::default(),
            online: false,
            selected: Vec::new(),
            vol: 40,
            status: String::new(),
            show_diag: false,
        }
    }

    // ---- plumbing ------------------------------------------------------- //

    fn pull(&mut self) {
        if let Some(s) = self.latest.lock().unwrap().clone() {
            let first_for_us = !self.online;
            self.online = true;
            let players_changed = s.players.iter().map(|p| p.udn.clone()).collect::<Vec<_>>()
                != self
                    .snap
                    .players
                    .iter()
                    .map(|p| p.udn.clone())
                    .collect::<Vec<_>>();
            self.snap = s;
            if first_for_us || players_changed {
                // adopt what the daemon already routes, so the window shows
                // the truth instead of an empty selection
                if !self.snap.wanted.is_empty() {
                    self.selected = self.snap.wanted.clone();
                }
            }
            if let Some(v) = self.snap.players.iter().find_map(|p| p.volume) {
                if self.vol == 40 {
                    self.vol = v;
                }
            }
        } else {
            self.online = false;
        }
        if let Some(msg) = self.notes.lock().unwrap().pop() {
            self.status = msg;
        }
    }

    fn post(&mut self, line: &str) {
        let base = self.base.clone();
        let notes = self.notes.clone();
        let line = line.to_string();
        self.status = format!("sending: {line}");
        std::thread::spawn(move || {
            let msg = match command(&base, &line) {
                Ok(r) => r,
                Err(e) => format!("{line}: {e}"),
            };
            notes.lock().unwrap().push(msg);
        });
    }

    fn wanted(&self) -> String {
        if self.selected.is_empty() {
            return String::from("all");
        }
        self.selected.join(",")
    }

    /// Are the speakers currently being fed?
    fn routing(&self) -> bool {
        routing_to_sonos(&self.snap.mode)
    }

    /// Whether a speaker row shows as selected. The rules live in the std-only
    /// library (covered by `apicheck selftest`), not in the view.
    fn picked(&self, name: &str) -> bool {
        picked(&self.selected, &self.snap.wanted, &self.snap.mode, name)
    }

    fn start_daemon(&mut self) {
        // the same code path `apicheck start` runs, so a terminal can prove
        // whether the failure is the binary lookup or the daemon itself
        self.status = String::from("starting the daemon...");
        let notes = self.notes.clone();
        std::thread::spawn(move || {
            let msg = locate::start_daemon().unwrap_or_else(|e| e);
            notes.lock().unwrap().push(msg);
        });
    }

    fn act(&mut self, a: Act) {
        match a {
            Act::Start => self.start_daemon(),
            Act::Stop => self.post("shutdown"),
            Act::Scan => self.post("discover"),
            Act::Checks => {
                let base = self.base.clone();
                let boxx = self.diag.clone();
                *boxx.lock().unwrap() = None;
                std::thread::spawn(move || {
                    *boxx.lock().unwrap() = Some(api_checks(&base).unwrap_or_default());
                });
            }
            Act::Mode(m) => {
                if m == "off" {
                    self.post("route off");
                } else {
                    // no shell here: the API takes the name list as it is, and
                    // quoting it used to put a literal " into the speaker name
                    self.post(&format!("route {m} {}", self.wanted()));
                }
            }
            Act::Player(i) => {
                let Some(p) = self.snap.players.get(i) else {
                    return;
                };
                let name = p.name.clone();
                if let Some(at) = self.selected.iter().position(|s| *s == name) {
                    self.selected.remove(at);
                } else {
                    self.selected.push(name);
                }
                // while routed, re-route (that also stores the selection); when
                // routed elsewhere, just record the target so the tick marks
                // mean the same thing whatever the mode is
                if self.routing() {
                    self.post(&format!("route {} {}", self.snap.mode, self.wanted()));
                } else {
                    self.post(&format!("player {}", self.wanted()));
                }
            }
            Act::VolDown => {
                self.vol = self.vol.saturating_sub(5);
                self.post(&format!("volume {}", self.vol));
            }
            Act::VolUp => {
                self.vol = (self.vol + 5).min(100);
                self.post(&format!("volume {}", self.vol));
            }
        }
    }

    // ---- view ----------------------------------------------------------- //

    fn player_row(this: &Entity<BridgeView>, i: usize, p: &PlayerInfo, on: bool) -> impl IntoElement {
        let mut right = p.ip.clone();
        if !p.model.is_empty() {
            right = format!("{}  {}", p.model, right);
        }
        if p.satellite {
            right = format!("{right}  sub/surround");
        }
        if let Some(v) = p.volume {
            right = format!("{right}  vol {v}");
        }
        row(
            this,
            Act::Player(i),
            on,
            format!("{} {}", p.name, if p.coordinator { "" } else { "(member)" }),
            right,
        )
    }
}

impl Render for BridgeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity().clone();
        let snap = self.snap.clone();

        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .flex_1()
            .child(header(&this, &snap, self.online))
            .child(card(
                "Output",
                div()
                    .flex()
                    .gap_2()
                    .flex_wrap()
                    .children(MODES.iter().map(|(m, name)| {
                        btn(&this, Act::Mode(m), *m == snap.mode.as_str(), name)
                    })),
            ))
            .child(players_card(&this, self, &snap))
            .child(status_card(&snap))
            .child(diag_card(&this, self))
            .child(footer(&self.status, &self.base));

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(INK))
            .child(titlebar())
            .child(body)
    }
}

// ---- pieces ------------------------------------------------------------ //

/// The window bar, drawn by the app.
///
/// GNOME on Wayland provides no server-side titlebar and gpui does not draw one,
/// so a window built from `WindowOptions` alone has nothing to grab: it cannot
/// be moved, minimized or closed by hand. This strip supplies all three, by
/// calling into the compositor (`start_window_move`, `minimize_window`,
/// `zoom_window`) from real event handlers.
fn titlebar() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .w_full()
        .flex_none()
        .h(px(34.0))
        .bg(rgb(BAR))
        .border_b_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .id("titlebar-drag")
                .flex_1()
                .h(px(34.0))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .window_control_area(WindowControlArea::Drag)
                .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
                .child(
                    div()
                        .w(px(12.0))
                        .h(px(12.0))
                        .rounded_full()
                        .bg(rgb(ACCENT)),
                )
                .child(div().text_sm().font_weight(FontWeight::BOLD).child("sonos-bridge"))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(MUTED))
                        .child("drag here to move the window"),
                ),
        )
        .child(win_btn(
            "win-min",
            "\u{2013}",
            WindowControlArea::Min,
            |_, window, _| window.minimize_window(),
        ))
        .child(win_btn(
            "win-max",
            "\u{25a1}",
            WindowControlArea::Max,
            |_, window, _| window.zoom_window(),
        ))
        .child(win_btn(
            "win-close",
            "\u{2715}",
            WindowControlArea::Close,
            |_, _window, cx| cx.quit(),
        ))
}

/// One window-control button.
///
/// `window_control_area` is kept for macOS/Windows, where gpui hit-tests it and
/// performs the window operation itself. On Linux it is a no-op (the platform's
/// `on_hit_test_window_control` is empty), so the `on_click` handler is what
/// actually moves, minimizes or closes the window - do not remove it.
/// Likewise the drag strip needs `start_window_move`, which the Wayland backend
/// implements as `xdg_toplevel.move` using the serial of the last mouse press,
/// so it only works if called from a mouse-down handler.
fn win_btn(
    id: &'static str,
    glyph: &'static str,
    area: WindowControlArea,
    f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(38.0))
        .h(px(34.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .text_base()
        .text_color(rgb(INK))
        .bg(rgb(if id == "win-close" { CARD } else { BAR }))
        .window_control_area(area)
        .on_click(f)
        .child(glyph)
}

fn header(this: &Entity<BridgeView>, snap: &Snapshot, online: bool) -> impl IntoElement {
    let (dot, label) = if online {
        (OK, format!("daemon running  ·  up {}", hm(snap.uptime_s)))
    } else {
        (BAD, "daemon not running".to_string())
    };
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .w(px(10.0))
                        .h(px(10.0))
                        .rounded_full()
                        .bg(rgb(dot)),
                )
                .child(
                    div()
                        .text_base()
                        .font_weight(FontWeight::BOLD)
                        .child(label),
                ),
        )
        .child(div().flex_1())
        .child(if online {
            btn(this, Act::Stop, false, "Stop daemon")
        } else {
            btn(this, Act::Start, true, "Start daemon")
        })
}

fn players_card(this: &Entity<BridgeView>, view: &BridgeView, snap: &Snapshot) -> impl IntoElement {
    let body = if snap.players.is_empty() {
        div()
            .text_sm()
            .text_color(rgb(MUTED))
            .child("no speakers yet - they show up a few seconds after the daemon starts, or press Scan")
    } else {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .children(snap.players.iter().enumerate().map(|(i, p)| {
                let on = view.picked(&p.name);
                BridgeView::player_row(this, i, p, on)
            }))
    };
    let head = div()
        .flex()
        .gap_2()
        .child(div().text_sm().text_color(rgb(MUTED)).child("volume"))
        .child(btn(this, Act::VolDown, false, "-"))
        .child(div().text_sm().child(format!("{}%", view.vol)))
        .child(btn(this, Act::VolUp, false, "+"))
        .child(div().flex_1())
        .child(btn(this, Act::Scan, false, "Scan"));
    let hint = div()
        .text_xs()
        .text_color(rgb(MUTED))
        .child("click a row to choose who receives audio - nothing ticked means every speaker");
    card(
        "Speakers",
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(body)
            .child(head)
            .child(hint),
    )
}

fn status_card(snap: &Snapshot) -> impl IntoElement {
    let body = div()
        .flex()
        .flex_col()
        .gap_1()
        .child(kv(
            "virtual output",
            if snap.sink_loaded {
                format!("{}  (default: {})", snap.sink, snap.hw_sink)
            } else {
                "not registered".into()
            },
        ))
        .child(kv(
            "capture",
            format!(
                "{}  pid {}",
                if snap.capture_backend.is_empty() {
                    "-"
                } else {
                    &snap.capture_backend
                },
                snap.capture_pid
            ),
        ))
        .child(kv("encoder", format!("pid {}", snap.encoder_pid)))
        .child(kv(
            "stream",
            format!("{}  {} client(s)", snap.url, snap.stream_clients),
        ))
        .child(kv(
            "audio",
            format!(
                "{}  peak {:.2}  rms {:.2}  ring recycled {}",
                if snap.audio_active {
                    "live"
                } else {
                    "silent"
                },
                snap.peak,
                snap.rms,
                bytes(snap.dropped_bytes)
            ),
        ))
        .child(feed_line(snap.feed_lost))
        .child(level(snap.peak));
    card("Live", body)
}

fn diag_card(this: &Entity<BridgeView>, view: &BridgeView) -> impl IntoElement {
    let mut body = div().flex().flex_col().gap_1();
    if view.show_diag {
        if let Some(rows) = view.diag.lock().unwrap().clone() {
            if rows.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(rgb(MUTED))
                        .child("no diagnostics (daemon offline?)"),
                );
            }
            for (ok, name, detail) in rows {
                body = body.child(check_row(ok, name, detail));
            }
        } else {
            body = body.child(div().text_sm().text_color(rgb(MUTED)).child("running checks…"));
        }
    }
    card(
        "Diagnostics",
        div().flex().flex_col().gap_2().child(
            div().flex().gap_2().child(btn(
                this,
                Act::Checks,
                view.show_diag,
                if view.show_diag { "hide" } else { "run checks" },
            )),
        ).child(body),
    )
}

fn footer(status: &str, base: &str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .text_sm()
                .text_color(rgb(if status.is_empty() { MUTED } else { INK }))
                .child(if status.is_empty() {
                    base.to_string()
                } else {
                    status.to_string()
                }),
        )
}

fn row(
    this: &Entity<BridgeView>,
    act: Act,
    on: bool,
    left: String,
    right: String,
) -> impl IntoElement {
    let this = this.clone();
    div()
        .id(("playerrow", act_id(&act)))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if on { 0xe8f0fe } else { CARD }))
        .border_1()
        .border_color(rgb(LINE))
        .child(
            div()
                .w(px(16.0))
                .h(px(16.0))
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(0xffffff))
                .rounded_sm()
                .border_1()
                .border_color(rgb(if on { ACCENT } else { MUTED }))
                .bg(rgb(if on { ACCENT } else { CARD }))
                .child(if on { "\u{2713}" } else { "" }),
        )
        .child(div().font_weight(FontWeight::BOLD).child(left))
        .child(div().flex_1())
        .child(div().text_sm().text_color(rgb(MUTED)).child(right))
        .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            this.update(cx, |v, cx| {
                v.act(act);
                cx.notify();
            });
        })
}

fn btn(
    this: &Entity<BridgeView>,
    act: Act,
    active: bool,
    text: &str,
) -> impl IntoElement {
    let this = this.clone();
    let label = text.to_string();
    div()
        .id(("btn", act_id(&act)))
        .px_3()
        .py_2()
        .rounded_sm()
        .cursor_pointer()
        .bg(rgb(if active { ACCENT } else { CARD }))
        .text_color(rgb(if active { 0xffffff } else { INK }))
        .border_1()
        .border_color(rgb(if active { ACCENT } else { LINE }))
        .child(label)
        .on_click(move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            this.update(cx, |v, cx| {
                v.act(act);
                cx.notify();
            });
        })
}

fn card(title: &'static str, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .rounded_sm()
        .bg(rgb(CARD))
        .border_1()
        .border_color(rgb(LINE))
        .child(div().text_sm().font_weight(FontWeight::BOLD).child(title))
        .child(body)
}

fn kv(k: &'static str, v: String) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(120.0))
                .text_sm()
                .text_color(rgb(MUTED))
                .child(k),
        )
        .child(div().text_sm().child(v))
}

/// The encoder feed must not lose audio: if it does, the player's buffer drains
/// and playback stutters, then stops. Show it in red rather than leaving the
/// user to discover it by ear.
fn feed_line(lost: i64) -> impl IntoElement {
    let (col, text) = if lost == 0 {
        (OK, "encoder feed: no audio dropped".to_string())
    } else {
        (
            BAD,
            format!(
                "encoder feed: {} of desktop audio dropped - playback will stutter. \
                 raise the capture buffer (PCM_CAP) or lower the capture rate",
                bytes(lost)
            ),
        )
    };
    div()
        .text_xs()
        .font_weight(FontWeight::BOLD)
        .text_color(rgb(col))
        .child(text)
}

fn check_row(ok: bool, name: String, detail: String) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(
            div()
                .w(px(120.0))
                .text_sm()
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(if ok { OK } else { BAD }))
                .child(if ok { "ok" } else { "fail" }),
        )
        .child(div().w(px(150.0)).text_sm().child(name))
        .child(div().text_sm().text_color(rgb(MUTED)).child(detail))
}

/// input level, so a silent desktop is obvious rather than looked for
fn level(peak: f64) -> impl IntoElement {
    let w: f32 = (peak.clamp(0.0, 1.0) * 320.0).max(2.0) as f32;
    div()
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .w(px(320.0))
                .h(px(8.0))
                .rounded_sm()
                .bg(rgb(LINE))
                .child(div().w(px(w)).h(px(8.0)).rounded_sm().bg(rgb(if peak > 0.02 { OK } else { WARN }))),
        )
        .child(div().text_sm().text_color(rgb(MUTED)).child(format!("{:.2}", peak)))
}

fn act_id(a: &Act) -> usize {
    // stable small number for the element id; gpui takes (name, usize)
    match a {
        Act::Start => 1,
        Act::Stop => 2,
        Act::Scan => 3,
        Act::Checks => 4,
        Act::Mode(m) => 10 + MODES.iter().position(|(x, _)| *x == *m).unwrap_or(0),
        Act::Player(i) => 100 + i,
        Act::VolDown => 200,
        Act::VolUp => 201,
    }
}

#[allow(dead_code)]
fn act_name(a: &Act) -> String {
    match a {
        Act::Start => "start".into(),
        Act::Stop => "stop".into(),
        Act::Scan => "scan".into(),
        Act::Checks => "checks".into(),
        Act::Mode(m) => format!("mode{m}"),
        Act::Player(i) => format!("player{i}"),
        Act::VolDown => "voldn".into(),
        Act::VolUp => "volup".into(),
    }
}

fn hm(secs: i64) -> String {
    format!("{}m{:02}s", secs / 60, secs % 60)
}

fn bytes(n: i64) -> String {
    if n > 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n > 1_000 {
        format!("{} kB", n / 1000)
    } else {
        format!("{n} B")
    }
}
