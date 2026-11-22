//! Process plumbing: signal handling and a registry of the child processes we
//! spawn (parec/ffmpeg/...), so shutdown can unblock the reader threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> Option<extern "C" fn(i32)>;
}

pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

pub static STOP: AtomicBool = AtomicBool::new(false);
static PIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

pub fn track(pid: u32) {
    PIDS.lock().unwrap().push(pid as i32);
}

pub fn untrack(pid: u32) {
    let mut v = PIDS.lock().unwrap();
    v.retain(|p| *p != pid as i32);
}

pub fn kill_pid(pid: u32, sig: i32) {
    unsafe {
        kill(pid as i32, sig);
    }
}

pub fn tracked_children() -> usize {
    PIDS.lock().unwrap().len()
}

/// Ask every tracked child to exit; this is what unblocks readers that sit in
/// a blocking `read()` on the child's stdout.
pub fn kill_children() {
    let pids = PIDS.lock().unwrap().clone();
    for pid in pids {
        unsafe {
            kill(pid, SIGTERM);
        }
    }
}

extern "C" fn on_signal(_sig: i32) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn install_signal_handlers() {
    unsafe {
        signal(SIGINT, on_signal);
        signal(SIGTERM, on_signal);
    }
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::SeqCst)
}
