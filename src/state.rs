//! The single shared state object every engine thread reads/writes.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::config::Config;
use crate::queues::{AudioLog, ByteQueue, MeterCell};
use crate::sonos::Player;

/// How far behind the live edge a player may fall before we call it dead,
/// measured in seconds of audio.
///
/// Radio players buffer: they pull ahead, then stop reading for a while. While
/// a player is not reading, our writer blocks and the broadcast ring keeps
/// filling; if the ring holds less time than the pause, that reader's position
/// is overrun and it gets a hole spliced into the middle of the stream. That is
/// the stutter, and it repeats - each time worse - until the player gives up on
/// the URI. Sized in seconds so it stays correct when the format or bitrate
/// changes; 60 s costs ~1.4 MB at 192 kbit/s, and no healthy player pauses that
/// long.
pub const LAG_SECONDS: u64 = 60;

/// Ring capacity in bytes for the configured stream.
pub(crate) fn log_cap(cfg: &Config) -> usize {
    (stream_bytes_per_s(cfg) * LAG_SECONDS).clamp(256_000, 4_000_000) as usize
}

/// Encoded bytes per second of audio. Exact for CBR MP3, an estimate for
/// level-0 FLAC of 48 kHz stereo.
pub(crate) fn stream_bytes_per_s(cfg: &Config) -> u64 {
    if cfg.stream_format == "flac" {
        return 100_000;
    }
    let kbps: u64 = cfg
        .stream_bitrate
        .trim_end_matches('k')
        .parse()
        .unwrap_or(192);
    kbps * 1000 / 8
}
/// Shock absorber between a bursty capture and a paced encoder.
///
/// Size here is *not* a latency budget in normal operation: the consumer
/// drains hundreds of times faster than realtime, so the queue sits empty and
/// the cap only ever matters when the consumer stalls. What a small cap does
/// guarantee is lost audio - the sound server delivers the monitor source in
/// bursts, and anything that does not fit before the next drain is dropped.
/// 2 s of 48 kHz stereo s16le; the steady-state occupancy stays near zero.
const PCM_CAP: usize = 400_000;

#[derive(Default)]
pub struct Stats {
    pub bytes_captured: AtomicU64,
    pub bytes_streamed: AtomicU64,
    /// bytes of PCM written into the encoder's stdin, including the
    /// silence we add to keep the stream's bitrate continuous
    pub enc_fed: AtomicU64,
    pub enc_restarts: AtomicU64,
    /// silence we injected to keep the stream continuous
    pub pad_bytes: AtomicU64,
    pub sonos_plays: AtomicU64,
    pub stream_clients: AtomicU32,
    pub started_ms: AtomicU64,
}

pub struct Shared {
    pub cfg: RwLock<Config>,
    pub running: AtomicBool,

    // routing
    pub mode: RwLock<String>,
    pub hw_sink: RwLock<String>,
    pub hw_auto: AtomicBool,
    pub sink_loaded: AtomicBool,
    pub sink_mod: Mutex<Option<u32>>,
    pub loop_mod: Mutex<Option<u32>>,
    pub local_kind: RwLock<String>,

    // sonos
    pub players: RwLock<Vec<Player>>,
    pub wanted: RwLock<Vec<String>>,
    pub active: RwLock<Vec<Player>>,
    pub sonos_state: RwLock<String>,
    pub sonos_uri: RwLock<String>,

    // diagnostics
    pub last_error: RwLock<String>,
    pub last_note: RwLock<String>,

    // audio plumbing
    pub log: AudioLog,
    pub enc_q: ByteQueue,
    pub local_q: ByteQueue,
    pub meter: MeterCell,

    // stream server
    pub stream_port: AtomicU32,
    pub encoder_pid: AtomicU32,
    pub capture_pid: AtomicU32,
    pub local_pid: AtomicU32,

    pub stats: Stats,
}

impl Shared {
    pub fn new(cfg: Config) -> Arc<Shared> {
        let cap = log_cap(&cfg);
        Arc::new(Shared {
            mode: RwLock::new(cfg.route.clone()),
            hw_sink: RwLock::new(String::new()),
            hw_auto: AtomicBool::new(true),
            sink_loaded: AtomicBool::new(false),
            sink_mod: Mutex::new(None),
            loop_mod: Mutex::new(None),
            local_kind: RwLock::new("none".to_string()),
            players: RwLock::new(Vec::new()),
            wanted: RwLock::new(if cfg.player.is_empty() {
                Vec::new()
            } else {
                vec![cfg.player.clone()]
            }),
            active: RwLock::new(Vec::new()),
            sonos_state: RwLock::new("idle".to_string()),
            sonos_uri: RwLock::new(String::new()),
            last_error: RwLock::new(String::new()),
            last_note: RwLock::new(String::new()),
            cfg: RwLock::new(cfg),
            running: AtomicBool::new(true),
            log: AudioLog::new(cap),
            enc_q: ByteQueue::new(PCM_CAP),
            local_q: ByteQueue::new(PCM_CAP),
            meter: MeterCell::new(),
            stream_port: AtomicU32::new(0),
            encoder_pid: AtomicU32::new(0),
            capture_pid: AtomicU32::new(0),
            local_pid: AtomicU32::new(0),
            stats: Stats {
                started_ms: AtomicU64::new(crate::util::now_ms()),
                ..Default::default()
            },
        })
    }

    // ---- config ---------------------------------------------------------- //
    pub fn cfg(&self) -> Config {
        self.cfg.read().unwrap().clone()
    }
    pub fn set_cfg_now(&self, c: Config) {
        *self.cfg.write().unwrap() = c.clone();
        crate::log::set_level(c.verbose.min(3) as usize);
    }
    pub fn update_cfg<F: FnOnce(&mut Config)>(&self, f: F) {
        {
            let mut c = self.cfg.write().unwrap();
            f(&mut c);
        }
        let c = self.cfg();
        crate::log::set_level(c.verbose.min(3) as usize);
    }

    // ---- mode ------------------------------------------------------------ //
    pub fn mode(&self) -> String {
        self.mode.read().unwrap().clone()
    }
    pub fn set_mode(&self, mode: &str) {
        *self.mode.write().unwrap() = mode.to_string();
    }
    pub fn has_sonos(&self) -> bool {
        matches!(self.mode().as_str(), "sonos" | "both")
    }
    pub fn has_local(&self) -> bool {
        matches!(self.mode().as_str(), "local" | "both")
    }
    pub fn capture_needed(&self) -> bool {
        self.mode() != "off"
    }

    // ---- players --------------------------------------------------------- //
    pub fn wanted(&self) -> Vec<String> {
        self.wanted.read().unwrap().clone()
    }
    pub fn set_wanted(&self, list: Vec<String>) {
        *self.wanted.write().unwrap() = list;
    }
    pub fn players(&self) -> Vec<Player> {
        self.players.read().unwrap().clone()
    }
    pub fn set_players(&self, list: Vec<Player>) {
        *self.players.write().unwrap() = list;
    }
    pub fn active(&self) -> Vec<Player> {
        self.active.read().unwrap().clone()
    }
    pub fn set_active(&self, list: Vec<Player>) {
        *self.active.write().unwrap() = list;
    }
    pub fn set_sonos_state(&self, s: &str) {
        *self.sonos_state.write().unwrap() = s.to_string();
    }
    pub fn sonos_state(&self) -> String {
        self.sonos_state.read().unwrap().clone()
    }

    // ---- misc ------------------------------------------------------------ //
    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    pub fn set_error(&self, e: &str) {
        if e.is_empty() {
            return;
        }
        *self.last_error.write().unwrap() = e.to_string();
        crate::log::info("error", e);
    }
    pub fn error(&self) -> String {
        self.last_error.read().unwrap().clone()
    }
    pub fn note(&self, n: &str) {
        *self.last_note.write().unwrap() = n.to_string();
        crate::log::info("note", n);
    }
    pub fn set_local_kind(&self, k: &str) {
        *self.local_kind.write().unwrap() = k.to_string();
    }
    pub fn local_kind(&self) -> String {
        self.local_kind.read().unwrap().clone()
    }

    // ---- urls ------------------------------------------------------------ //
    pub fn stream_path(&self) -> String {
        format!("/stream.{}", self.cfg.read().unwrap().stream_format)
    }

    /// The URL handed to the speakers (contains the secret token).
    pub fn stream_url(&self) -> String {
        let cfg = self.cfg();
        let port = self.stream_port.load(Ordering::SeqCst);
        if port == 0 {
            return String::new();
        }
        let host = crate::util::lan_address(&cfg.http_bind);
        format!(
            "http://{}:{}{}?t={}",
            host,
            port,
            self.stream_path(),
            cfg.stream_token
        )
    }

    pub fn is_our_url(&self, url: &str) -> bool {
        let port = self.stream_port.load(Ordering::SeqCst);
        if port == 0 || url.is_empty() {
            return false;
        }
        let needle = format!(":{}", port);
        url.contains(&needle) || url.contains("sonos-bridge")
    }
}
