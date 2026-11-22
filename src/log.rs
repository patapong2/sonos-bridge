//! Timestamped stderr logging with a runtime level.

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};

pub const L_QUIET: usize = 0;
pub const L_INFO: usize = 1;
pub const L_DEBUG: usize = 2;
pub const L_TRACE: usize = 3;

static LEVEL: AtomicUsize = AtomicUsize::new(L_INFO);

pub fn set_level(level: usize) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> usize {
    LEVEL.load(Ordering::Relaxed)
}

pub fn at(lvl: usize, tag: &str, msg: &str) {
    if lvl > level() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now % 86_400;
    let line = format!(
        "[{:02}:{:02}:{:02}] {:<8} {}\n",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        tag,
        msg
    );
    let mut err = std::io::stderr();
    let _ = err.write_all(line.as_bytes());
    let _ = err.flush();
}

pub fn quiet(tag: &str, msg: &str) {
    at(L_QUIET, tag, msg);
}
pub fn info(tag: &str, msg: &str) {
    at(L_INFO, tag, msg);
}
pub fn debug(tag: &str, msg: &str) {
    at(L_DEBUG, tag, msg);
}
pub fn trace(tag: &str, msg: &str) {
    at(L_TRACE, tag, msg);
}
