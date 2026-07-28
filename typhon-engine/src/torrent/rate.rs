use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Simple rate counter: stores total bytes and computes rate from delta over time.
pub struct RateTracker {
    last_total: AtomicU64,
    last_time: std::sync::Mutex<Instant>,
    rate: AtomicU64, // bytes per second
    seeded: std::sync::atomic::AtomicBool,
}

impl RateTracker {
    pub fn new() -> Self {
        Self {
            last_total: AtomicU64::new(0),
            last_time: std::sync::Mutex::new(Instant::now()),
            rate: AtomicU64::new(0),
            seeded: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Update rate based on new total. Call periodically (e.g. every 2s).
    ///
    /// First call after construction only seeds last_total — it does NOT
    /// compute a rate. Otherwise a torrent loaded from fastresume with, say,
    /// 64 GB of cumulative upload would produce `64 GB / 2 s = 32 GB/s` on
    /// the first tick, blowing up aggregates downstream.
    pub fn update(&self, current_total: u64) {
        let prev = self.last_total.swap(current_total, Ordering::Relaxed);
        let mut last_time = self.last_time.lock().unwrap();
        let now = Instant::now();
        if !self.seeded.swap(true, Ordering::Relaxed) {
            // first tick — just seed the reference point
            *last_time = now;
            return;
        }
        let elapsed = now.duration_since(*last_time).as_secs_f64();
        if elapsed > 0.5 {
            let delta = current_total.saturating_sub(prev);
            let rate = (delta as f64 / elapsed) as u64;
            self.rate.store(rate, Ordering::Relaxed);
            *last_time = now;
        }
    }

    /// Get current rate in bytes/sec.
    pub fn get(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }
}
