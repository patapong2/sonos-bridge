//! Bounded, drop-oldest byte plumbing + the broadcast log that the Sonos
//! stream readers tail, plus the level meter.

use std::collections::VecDeque;
use std::sync::Condvar;
use std::sync::Mutex;
use std::time::Duration;

fn drain(q: &mut VecDeque<u8>, out: &mut Vec<u8>, max: usize) -> usize {
    let n = max.min(q.len());
    out.clear();
    out.reserve(n);
    for _ in 0..n {
        match q.pop_front() {
            Some(b) => out.push(b),
            None => break,
        }
    }
    n
}

/// Queue for "process A feeds process B" pipes: if the consumer falls behind
/// we drop the *oldest* bytes so latency never grows.
pub struct ByteQueue {
    inner: Mutex<VecDeque<u8>>,
    cv: Condvar,
    cap: usize,
    /// bytes thrown away because the consumer was not quick enough. Without
    /// this the pipeline can lose audio while every counter looks healthy.
    lost: Mutex<u64>,
}

impl ByteQueue {
    pub fn new(cap_bytes: usize) -> ByteQueue {
        ByteQueue {
            inner: Mutex::new(VecDeque::with_capacity(cap_bytes.min(65536))),
            cv: Condvar::new(),
            cap: cap_bytes,
            lost: Mutex::new(0),
        }
    }

    pub fn push(&self, data: &[u8]) {
        {
            let mut q = self.inner.lock().unwrap();
            q.extend(data.iter().copied());
            let over = q.len().saturating_sub(self.cap);
            if over > 0 {
                q.drain(..over);
                *self.lost.lock().unwrap() += over as u64;
            }
        }
        self.cv.notify_all();
    }

    /// Bytes dropped (oldest-first) since start.
    pub fn lost(&self) -> u64 {
        *self.lost.lock().unwrap()
    }

    pub fn clear(&self) {
        let mut q = self.inner.lock().unwrap();
        q.clear();
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Move up to `max` bytes into `out`; returns how many were taken.
    pub fn pop_up_to(&self, out: &mut Vec<u8>, max: usize) -> usize {
        let mut q = self.inner.lock().unwrap();
        drain(&mut q, out, max)
    }

    /// `pop_up_to`, but first waits up to `budget` for data to arrive.
    ///
    /// A live capture hands over bytes at the rate the audio plays, so an
    /// empty queue normally only means the next burst has not landed yet.
    /// A consumer that pads instead of waiting invents samples and ends up
    /// running faster than real time, so the waiting belongs here.
    pub fn pop_wait(&self, out: &mut Vec<u8>, max: usize, budget: Duration) -> usize {
        let mut q = self.inner.lock().unwrap();
        let mut left = budget;
        while q.is_empty() && !left.is_zero() {
            let step = Duration::from_millis(20).min(left);
            let (g, _) = self
                .cv
                .wait_timeout(q, step)
                .unwrap_or_else(|e| e.into_inner());
            q = g;
            left = left.saturating_sub(step);
        }
        drain(&mut q, out, max)
    }

    pub fn wait_data(&self, timeout: Duration) {
        let q = self.inner.lock().unwrap();
        if !q.is_empty() {
            return;
        }
        let _ = self.cv.wait_timeout(q, timeout);
    }
}

/// Append-only circular log with absolute byte offsets.  Every Sonos speaker
/// (and the debugging reader) keeps its own read position, so one encoder can
/// serve several players and a lagging reader just resyncs instead of stalling
/// the pipeline.
pub struct AudioLog {
    inner: Mutex<LogInner>,
    cv: Condvar,
}

struct LogInner {
    buf: VecDeque<u8>,
    cap: usize,
    oldest: u64,
    total: u64,
    dropped: u64,
}

impl AudioLog {
    pub fn new(cap_bytes: usize) -> AudioLog {
        AudioLog {
            inner: Mutex::new(LogInner {
                buf: VecDeque::with_capacity(cap_bytes.min(65536)),
                cap: cap_bytes,
                oldest: 0,
                total: 0,
                dropped: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn push(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.buf.extend(data.iter().copied());
            g.total += data.len() as u64;
            let over = g.buf.len().saturating_sub(g.cap);
            if over > 0 {
                g.buf.drain(..over);
                g.oldest += over as u64;
                g.dropped += over as u64;
            }
        }
        self.cv.notify_all();
    }

    pub fn total(&self) -> u64 {
        self.inner.lock().unwrap().total
    }

    pub fn dropped(&self) -> u64 {
        self.inner.lock().unwrap().dropped
    }

    pub fn clear(&self) {
        let mut g = self.inner.lock().unwrap();
        g.buf.clear();
        g.oldest = g.total;
    }

    /// `(new_pos, bytes)`.  If `pos` points at data that was already dropped,
    /// the returned position is the oldest available one and `bytes` is empty:
    /// that is the signal for the reader to re-sync.
    pub fn read_from(&self, pos: u64, max: usize) -> (u64, Vec<u8>) {
        let g = self.inner.lock().unwrap();
        let pos = pos;
        if pos < g.oldest {
            return (g.oldest, Vec::new());
        }
        if pos >= g.total {
            return (pos, Vec::new());
        }
        let start = (pos - g.oldest) as usize;
        let n = max.min((g.total - pos) as usize);
        let data = g.buf.iter().skip(start).take(n).copied().collect();
        (pos + n as u64, data)
    }

    pub fn wait_new(&self, pos: u64, timeout: Duration) {
        let g = self.inner.lock().unwrap();
        if g.total > pos {
            return;
        }
        let _ = self.cv.wait_timeout(g, timeout);
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MeterSnapshot {
    pub peak: f32,
    pub rms: f32,
    pub last_sound_ms: u64,
    pub frames: u64,
    pub dropped_bytes: u64,
}

pub struct MeterCell {
    inner: Mutex<MeterSnapshot>,
    last_drop: Mutex<u64>,
}

impl MeterCell {
    pub fn new() -> MeterCell {
        MeterCell {
            inner: Mutex::new(MeterSnapshot::default()),
            last_drop: Mutex::new(0),
        }
    }

    /// Feed s16le PCM; keeps a fast-decaying peak and a smoothed RMS.
    pub fn sample(&self, pcm: &[u8], dropped_total: u64) {
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        let mut n = 0u64;
        let mut i = 0usize;
        while i + 1 < pcm.len() {
            let v = i16::from_le_bytes([pcm[i], pcm[i + 1]]);
            let f = v as f32 / 32768.0;
            let a = if f < 0.0 { -f } else { f };
            if a > peak {
                peak = a;
            }
            sum += (f as f64) * (f as f64);
            n += 1;
            i += 2;
        }
        let rms = if n > 0 {
            (sum / n as f64).sqrt() as f32
        } else {
            0.0
        };
        let now = crate::util::now_ms();
        {
            let mut g = self.inner.lock().unwrap();
            let decayed = g.peak * 0.55;
            g.peak = if peak > decayed { peak } else { decayed };
            g.rms = g.rms * 0.7 + rms * 0.3;
            if rms > 0.002 {
                g.last_sound_ms = now;
            }
            g.frames += 1;
            g.dropped_bytes = dropped_total;
        }
        *self.last_drop.lock().unwrap() = dropped_total;
    }

    pub fn snapshot(&self) -> MeterSnapshot {
        *self.inner.lock().unwrap()
    }

    pub fn active(&self) -> bool {
        crate::util::now_ms().saturating_sub(self.snapshot().last_sound_ms) < 1200
    }
}

impl Default for MeterCell {
    fn default() -> MeterCell {
        MeterCell::new()
    }
}
