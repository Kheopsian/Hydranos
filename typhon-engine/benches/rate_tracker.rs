//! Bench RateTracker::update called every 2s against 13k torrents.
//! Flamegraph 2026-04-19 showed ~218M samples on update_rates in prod.
//! Compares current impl (Mutex<Instant>) vs a hypothetical AtomicU64-only variant.
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// --- Current impl (copy from src/torrent/rate.rs) ---
pub struct RateTrackerMutex {
    last_total: AtomicU64,
    last_time: std::sync::Mutex<Instant>,
    rate: AtomicU64,
}
impl RateTrackerMutex {
    pub fn new() -> Self {
        Self {
            last_total: AtomicU64::new(0),
            last_time: std::sync::Mutex::new(Instant::now()),
            rate: AtomicU64::new(0),
        }
    }
    pub fn update(&self, current_total: u64) {
        let prev = self.last_total.swap(current_total, Ordering::Relaxed);
        let mut last_time = self.last_time.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(*last_time).as_secs_f64();
        if elapsed > 0.5 {
            let delta = current_total.saturating_sub(prev);
            let rate = (delta as f64 / elapsed) as u64;
            self.rate.store(rate, Ordering::Relaxed);
            *last_time = now;
        }
    }
}

// --- Lockless variant (AtomicU64 epoch nanos) ---
pub struct RateTrackerAtomic {
    last_total: AtomicU64,
    last_time_ns: AtomicU64, // monotonic ns since origin
    rate: AtomicU64,
}
use std::sync::OnceLock;
static ORIGIN: OnceLock<Instant> = OnceLock::new();
fn now_ns() -> u64 {
    let origin = ORIGIN.get_or_init(Instant::now);
    origin.elapsed().as_nanos() as u64
}
impl RateTrackerAtomic {
    pub fn new() -> Self {
        Self {
            last_total: AtomicU64::new(0),
            last_time_ns: AtomicU64::new(now_ns()),
            rate: AtomicU64::new(0),
        }
    }
    pub fn update(&self, current_total: u64) {
        let now = now_ns();
        let prev_time = self.last_time_ns.load(Ordering::Relaxed);
        let elapsed_ns = now.saturating_sub(prev_time);
        if elapsed_ns < 500_000_000 {
            self.last_total.store(current_total, Ordering::Relaxed);
            return;
        }
        if self.last_time_ns
            .compare_exchange(prev_time, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        let prev_total = self.last_total.swap(current_total, Ordering::Relaxed);
        let delta = current_total.saturating_sub(prev_total);
        let rate = if elapsed_ns > 0 {
            ((delta as u128 * 1_000_000_000u128) / elapsed_ns as u128) as u64
        } else { 0 };
        self.rate.store(rate, Ordering::Relaxed);
    }
}

fn bench_mutex_13k(c: &mut Criterion) {
    let trackers: Vec<RateTrackerMutex> = (0..13_000).map(|_| RateTrackerMutex::new()).collect();
    let mut counter: u64 = 0;
    c.bench_function("mutex_13k_update_pass", |b| {
        b.iter(|| {
            for t in &trackers {
                counter = counter.wrapping_add(16384);
                t.update(black_box(counter));
            }
        });
    });
}

fn bench_atomic_13k(c: &mut Criterion) {
    let trackers: Vec<RateTrackerAtomic> = (0..13_000).map(|_| RateTrackerAtomic::new()).collect();
    let mut counter: u64 = 0;
    c.bench_function("atomic_13k_update_pass", |b| {
        b.iter(|| {
            for t in &trackers {
                counter = counter.wrapping_add(16384);
                t.update(black_box(counter));
            }
        });
    });
}

criterion_group!(benches, bench_mutex_13k, bench_atomic_13k);
criterion_main!(benches);
