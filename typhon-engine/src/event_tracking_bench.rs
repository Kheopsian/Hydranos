//! Test-only alternatives. None installs hooks in the shipping engine.
use super::*;
use crate::bench_support as bench;
use crate::torrent::meta::{InfoHash, TorrentState};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Barrier, Mutex};
use std::time::Instant;
use tokio::sync::mpsc;

const CAPACITY: usize = 4096;
const SHARDS: usize = 32;
const STRATEGIES: &[&str] = &[
    "sampler",
    "fused_rate_pass",
    "embedded_flags",
    "fused_flags",
    "dense_flags",
    "bitmap",
    "mutex_set",
    "sharded_set",
    "raw_queue",
    "raw_queue_signal",
    "dedup_queue",
    "bitmap_queue",
];

#[derive(Default)]
struct Batch {
    ids: Vec<usize>,
    resync: bool,
    fallback: bool,
    client_resync: bool,
}
trait Dirty: Sync {
    fn mark(&self, id: usize);
    fn drain(&self) -> Batch;
    /// Payload allocation estimate, excluding locks, allocator and channel bookkeeping.
    fn bytes(&self) -> usize;
}

struct Embedded {
    rows: Arc<Vec<Arc<TorrentState>>>,
}
impl Dirty for Embedded {
    fn mark(&self, id: usize) {
        self.rows[id].bench_dirty.swap(true, Ordering::Release);
    }
    fn drain(&self) -> Batch {
        Batch {
            ids: self
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, t)| {
                    (t.bench_dirty.load(Ordering::Acquire)
                        && t.bench_dirty.swap(false, Ordering::AcqRel))
                    .then_some(i)
                })
                .collect(),
            resync: false,
            fallback: false,
            client_resync: false,
        }
    }
    fn bytes(&self) -> usize {
        self.rows.len()
    }
}
struct Flags {
    bits: Vec<AtomicBool>,
}
impl Flags {
    fn new(n: usize) -> Self {
        Self {
            bits: (0..n).map(|_| AtomicBool::new(false)).collect(),
        }
    }
    fn take(&self, id: usize) -> bool {
        self.bits[id].load(Ordering::Acquire) && self.bits[id].swap(false, Ordering::AcqRel)
    }
}
impl Dirty for Flags {
    fn mark(&self, id: usize) {
        self.bits[id].swap(true, Ordering::Release);
    }
    fn drain(&self) -> Batch {
        Batch {
            ids: (0..self.bits.len()).filter(|&i| self.take(i)).collect(),
            resync: false,
            fallback: false,
            client_resync: false,
        }
    }
    fn bytes(&self) -> usize {
        self.bits.capacity() * std::mem::size_of::<AtomicBool>()
    }
}
struct Bitmap {
    words: Vec<AtomicU64>,
    n: usize,
}
impl Bitmap {
    fn new(n: usize) -> Self {
        Self {
            words: (0..n.div_ceil(64)).map(|_| AtomicU64::new(0)).collect(),
            n,
        }
    }
    fn set(&self, id: usize) -> bool {
        self.words[id / 64].fetch_or(1u64 << (id % 64), Ordering::Release) & (1u64 << (id % 64))
            == 0
    }
    fn take(&self, id: usize) -> bool {
        self.words[id / 64].fetch_and(!(1u64 << (id % 64)), Ordering::AcqRel) & (1u64 << (id % 64))
            != 0
    }
}
impl Dirty for Bitmap {
    fn mark(&self, id: usize) {
        self.set(id);
    }
    fn drain(&self) -> Batch {
        let mut ids = Vec::new();
        for (i, word) in self.words.iter().enumerate() {
            if word.load(Ordering::Acquire) == 0 {
                continue;
            }
            let mut value = word.swap(0, Ordering::AcqRel);
            while value != 0 {
                let id = i * 64 + value.trailing_zeros() as usize;
                if id < self.n {
                    ids.push(id);
                }
                value &= value - 1;
            }
        }
        Batch {
            ids,
            resync: false,
            fallback: false,
            client_resync: false,
        }
    }
    fn bytes(&self) -> usize {
        self.words.capacity() * std::mem::size_of::<AtomicU64>()
    }
}
struct Sets {
    shards: Vec<Mutex<HashSet<InfoHash>>>,
    rows: Arc<Vec<Arc<TorrentState>>>,
}
impl Dirty for Sets {
    fn mark(&self, id: usize) {
        let mut hash = id as u64;
        hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
        hash ^= hash >> 31;
        self.shards[hash as usize % self.shards.len()]
            .lock()
            .unwrap()
            .insert(self.rows[id].info_hash);
    }
    fn drain(&self) -> Batch {
        let mut ids = Vec::new();
        for shard in &self.shards {
            ids.extend(
                shard
                    .lock()
                    .unwrap()
                    .drain()
                    .map(|hash| u64::from_be_bytes(hash[..8].try_into().unwrap()) as usize),
            );
        }
        Batch {
            ids,
            resync: false,
            fallback: false,
            client_resync: false,
        }
    }
    fn bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap().capacity() * std::mem::size_of::<InfoHash>())
            .sum()
    }
}
enum Pending {
    None,
    Flags(Flags),
    Bits(Bitmap),
}
struct Queue {
    tx: mpsc::Sender<usize>,
    rx: Mutex<mpsc::Receiver<usize>>,
    pending: Pending,
    overflow: AtomicBool,
    n: usize,
    cap: usize,
    signal_only: bool,
}
impl Queue {
    fn new(n: usize, cap: usize, pending: Pending) -> Self {
        let (tx, rx) = mpsc::channel(cap);
        Self {
            tx,
            rx: Mutex::new(rx),
            pending,
            overflow: AtomicBool::new(false),
            n,
            cap,
            signal_only: false,
        }
    }
}
impl Dirty for Queue {
    fn mark(&self, id: usize) {
        let fresh = match &self.pending {
            Pending::None => true,
            Pending::Flags(flags) => !flags.bits[id].swap(true, Ordering::AcqRel),
            Pending::Bits(bits) => bits.set(id),
        };
        if fresh && self.tx.try_send(id).is_err() {
            self.overflow.store(true, Ordering::Release);
        }
    }
    fn drain(&self) -> Batch {
        let mut ids = Vec::new();
        let mut receiver = self.rx.lock().unwrap();
        while let Ok(id) = receiver.try_recv() {
            let take = match &self.pending {
                Pending::None => true,
                Pending::Flags(flags) => flags.take(id),
                Pending::Bits(bits) => bits.take(id),
            };
            if take {
                ids.push(id);
            }
        }
        drop(receiver);
        let mut resync = false;
        let mut client_resync = false;
        let fallback = self.overflow.swap(false, Ordering::AcqRel);
        if fallback {
            match &self.pending {
                Pending::None => {
                    resync = true;
                    if self.signal_only {
                        ids.clear();
                        client_resync = true;
                    } else {
                        ids = (0..self.n).collect();
                    }
                }
                Pending::Flags(flags) => ids.extend(flags.drain().ids),
                Pending::Bits(bits) => ids.extend(bits.drain().ids),
            }
        }
        ids.sort_unstable();
        ids.dedup();
        Batch {
            ids,
            resync,
            fallback,
            client_resync,
        }
    }
    fn bytes(&self) -> usize {
        self.cap * std::mem::size_of::<usize>()
            + match &self.pending {
                Pending::None => 0,
                Pending::Flags(flags) => flags.bytes(),
                Pending::Bits(bits) => bits.bytes(),
            }
    }
}
fn dirty(name: &str, rows: Arc<Vec<Arc<TorrentState>>>, cap: usize) -> Option<Box<dyn Dirty>> {
    let n = rows.len();
    Some(match name {
        "sampler" | "fused_rate_pass" => return None,
        "embedded_flags" | "fused_flags" => Box::new(Embedded { rows }),
        "dense_flags" => Box::new(Flags::new(n)),
        "bitmap" => Box::new(Bitmap::new(n)),
        "mutex_set" | "sharded_set" => Box::new(Sets {
            shards: (0..if name == "mutex_set" { 1 } else { SHARDS })
                .map(|_| Mutex::new(HashSet::new()))
                .collect(),
            rows,
        }),
        "raw_queue" => Box::new(Queue::new(n, cap, Pending::None)),
        "raw_queue_signal" => {
            let mut queue = Queue::new(n, cap, Pending::None);
            queue.signal_only = true;
            Box::new(queue)
        }
        "dedup_queue" => Box::new(Queue::new(n, cap, Pending::Flags(Flags::new(n)))),
        "bitmap_queue" => Box::new(Queue::new(n, cap, Pending::Bits(Bitmap::new(n)))),
        _ => panic!("unknown strategy: {name}"),
    })
}
fn rows(manager: &TorrentManager) -> Arc<Vec<Arc<TorrentState>>> {
    let mut rows = manager.all();
    rows.sort_unstable_by_key(|t| t.info_hash);
    for (i, t) in rows.iter().enumerate() {
        assert_eq!(
            u64::from_be_bytes(t.info_hash[..8].try_into().unwrap()) as usize,
            i
        );
    }
    Arc::new(rows)
}

#[derive(Clone, Copy)]
struct Work {
    name: &'static str,
    unique: usize,
    updates: usize,
    writers: usize,
    active: bool,
    batched: bool,
    spread: bool,
}
fn work(n: usize) -> Vec<Work> {
    vec![
        Work {
            name: "idle",
            unique: 0,
            updates: 0,
            writers: 1,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "no_subscribers",
            unique: 100,
            updates: 200_000,
            writers: 8,
            active: false,
            batched: false,
            spread: false,
        },
        Work {
            name: "sparse",
            unique: 100,
            updates: 100,
            writers: 1,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "duplicate_updates_1w",
            unique: 100,
            updates: 200_000,
            writers: 1,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "duplicate_updates_8w",
            unique: 100,
            updates: 200_000,
            writers: 8,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "spread_duplicate_updates_8w",
            unique: 100,
            updates: 200_000,
            writers: 8,
            active: true,
            batched: false,
            spread: true,
        },
        Work {
            name: "one_hot_16w",
            unique: 1,
            updates: 200_000,
            writers: 16,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "batched_notifications",
            unique: 100,
            updates: 200_000,
            writers: 8,
            active: true,
            batched: true,
            spread: false,
        },
        Work {
            name: "overflow_burst",
            unique: 10_000.min(n),
            updates: 10_000.min(n),
            writers: 8,
            active: true,
            batched: false,
            spread: false,
        },
        Work {
            name: "whole_catalogue",
            unique: n,
            updates: n,
            writers: 8,
            active: true,
            batched: false,
            spread: false,
        },
    ]
}
fn produce(
    rows: &[Arc<TorrentState>],
    dirty: Option<&dyn Dirty>,
    enabled: &AtomicBool,
    w: Work,
) -> (f64, f64) {
    if w.updates == 0 {
        return (0.0, 0.0);
    }
    let start = Barrier::new(w.writers + 1);
    let end = Barrier::new(w.writers + 1);
    let (elapsed, cpu) = std::thread::scope(|scope| {
        for writer in 0..w.writers {
            let start = &start;
            let end = &end;
            scope.spawn(move || {
                start.wait();
                for op in (writer..w.updates).step_by(w.writers) {
                    let logical = op % w.unique;
                    let id = if w.spread {
                        logical * (rows.len() / w.unique)
                    } else {
                        logical
                    };
                    rows[id].total_uploaded.fetch_add(1, Ordering::Relaxed);
                    if !w.batched {
                        if let Some(dirty) = dirty {
                            if enabled.load(Ordering::Acquire) {
                                dirty.mark(id);
                            }
                        }
                    }
                }
                end.wait();
            });
        }
        let cpu = bench::cpu_ms();
        let at = Instant::now();
        start.wait();
        end.wait();
        (at.elapsed().as_secs_f64() * 1000.0, bench::cpu_ms() - cpu)
    });
    let (mut elapsed, mut cpu) = (elapsed, cpu);
    if w.batched && enabled.load(Ordering::Acquire) {
        let before = bench::cpu_ms();
        let at = Instant::now();
        if let Some(dirty) = dirty {
            for id in 0..w.unique {
                dirty.mark(id);
            }
        }
        elapsed += at.elapsed().as_secs_f64() * 1000.0;
        cpu += bench::cpu_ms() - before;
    }
    (elapsed, cpu)
}

#[test]
fn event_tracking_deduplicates_and_recovers_from_overflow() {
    let (manager, root) = super::live_tests::manager("tracking-correctness", 128);
    let rows = rows(&manager);
    for name in STRATEGIES.iter().skip(2) {
        let tracker = dirty(name, rows.clone(), 4).unwrap();
        for _ in 0..3 {
            for id in 0..100 {
                tracker.mark(id);
            }
        }
        let batch = tracker.drain();
        let seen: HashSet<_> = batch.ids.iter().copied().collect();
        assert_eq!(seen.len(), batch.ids.len(), "duplicates: {name}");
        if !batch.client_resync {
            assert!(
                (0..100).all(|i| seen.contains(&i)),
                "lost overflow changes: {name}"
            );
        } else {
            assert_eq!(*name, "raw_queue_signal");
        }
        assert_eq!(
            batch.resync,
            matches!(*name, "raw_queue" | "raw_queue_signal")
        );
        assert!(tracker.drain().ids.is_empty(), "not drained: {name}");
    }
    drop(rows);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

fn remember(batch: Batch, rows: &[Arc<TorrentState>], seen: &mut [u64]) {
    let ids = if batch.client_resync {
        (0..rows.len()).collect()
    } else {
        batch.ids
    };
    for id in ids {
        seen[id] = rows[id].total_uploaded.load(Ordering::Acquire);
    }
}

#[test]
fn event_tracking_preserves_final_updates_during_concurrent_drains() {
    let (manager, root) = super::live_tests::manager("tracking-races", 128);
    let rows = rows(&manager);
    for name in STRATEGIES.iter().skip(2) {
        let tracker = dirty(name, rows.clone(), 8).unwrap();
        let finished = AtomicUsize::new(0);
        let mut seen = vec![0u64; rows.len()];
        std::thread::scope(|scope| {
            for writer in 0..4 {
                let tracker = &tracker;
                let rows = &rows;
                let finished = &finished;
                scope.spawn(move || {
                    for op in 0..10_000 {
                        let id = (op * 4 + writer) % 128;
                        rows[id].total_uploaded.fetch_add(1, Ordering::Relaxed);
                        tracker.mark(id);
                    }
                    finished.fetch_add(1, Ordering::Release);
                });
            }
            while finished.load(Ordering::Acquire) != 4 {
                remember(tracker.drain(), &rows, &mut seen);
                std::thread::yield_now();
            }
        });
        remember(tracker.drain(), &rows, &mut seen);
        for (id, t) in rows.iter().enumerate() {
            assert_eq!(
                seen[id],
                t.total_uploaded.load(Ordering::Relaxed),
                "lost final update: {name}, {id}"
            );
        }
    }
    drop(rows);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn event_tracking_can_notify_rate_decay_and_state_without_new_bytes() {
    let (manager, root) = super::live_tests::manager("tracking-fields", 4);
    let rows = rows(&manager);
    for name in STRATEGIES.iter().skip(2) {
        let tracker = dirty(name, rows.clone(), 4).unwrap();
        rows[0].is_paused.store(true, Ordering::Relaxed);
        tracker.mark(0);
        rows[1].rechecking.store(true, Ordering::Relaxed);
        tracker.mark(1);
        // A rate observer must issue this notification even if totals did not move.
        tracker.mark(2);
        let batch = tracker.drain();
        assert_eq!(batch.ids.len(), 3, "{name}");
        let latest: Vec<_> = batch
            .ids
            .into_iter()
            .map(|i| StatsEmitter::snapshot(rows[i].clone()))
            .collect();
        assert!(latest.iter().any(|r| r.state == "paused"));
        assert!(latest.iter().any(|r| r.state == "checking_files"));
        assert!(latest.iter().any(|r| r.upload_rate == 0));
        rows[0].is_paused.store(false, Ordering::Relaxed);
        rows[1].rechecking.store(false, Ordering::Relaxed);
    }
    drop(rows);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}

#[test]
#[ignore]
fn benchmark_event_tracking() {
    bench::start();
    let n = bench::count();
    let name = std::env::var("HYDRANOS_EVENT_STRATEGY").unwrap();
    assert!(STRATEGIES.contains(&name.as_str()));
    let (manager, root) = super::live_tests::manager(&format!("event-{name}"), n);
    let rows = rows(&manager);
    let rss_before = bench::resident_bytes();
    let tracker = dirty(&name, rows.clone(), CAPACITY);
    let enabled = AtomicBool::new(true);
    let mut emitter = StatsEmitter::default();
    let mut listener = Some(manager.bus().subscribe());
    manager.update_rates();
    if tracker.is_none() {
        emitter.collect(&manager);
    }
    let rss_primed = bench::resident_bytes();
    for w in work(n) {
        enabled.store(w.active, Ordering::Release);
        if !w.active {
            listener = None;
        } else if listener.is_none() {
            listener = Some(manager.bus().subscribe());
        }
        // Prime the sampler outside measurements after a subscriber returns.
        if tracker.is_none() {
            emitter.collect(&manager);
        }
        let mut samples = Vec::new();
        for _ in 0..3 {
            let (producer_ms, producer_cpu) = produce(&rows, tracker.as_deref(), &enabled, w);
            let before = bench::cpu_ms();
            let at = Instant::now();
            let mut batch = Batch::default();
            let snapshots;
            let rate_phase_ms;
            if name == "fused_flags" && w.active {
                let mut changed = Vec::new();
                manager.update_rates_observed(|t| {
                    if t.bench_dirty.load(Ordering::Acquire)
                        && t.bench_dirty.swap(false, Ordering::AcqRel)
                    {
                        changed.push(t.info_hash);
                    }
                });
                rate_phase_ms = at.elapsed().as_secs_f64() * 1000.0;
                snapshots = changed
                    .into_iter()
                    .filter_map(|hash| manager.get(&hash))
                    .map(StatsEmitter::snapshot)
                    .collect();
            } else if name == "fused_rate_pass" && w.active {
                assert!(emitter.begin(&manager));
                let mut changed = Vec::new();
                manager.update_rates_observed(|t| emitter.observe(t, &mut changed));
                rate_phase_ms = at.elapsed().as_secs_f64() * 1000.0;
                snapshots = emitter.finish(&manager, changed);
            } else {
                manager.update_rates();
                rate_phase_ms = at.elapsed().as_secs_f64() * 1000.0;
                if let Some(tracker) = tracker.as_ref().filter(|_| w.active) {
                    batch = tracker.drain();
                    snapshots = batch
                        .ids
                        .iter()
                        .map(|&id| {
                            StatsEmitter::snapshot(manager.get(&rows[id].info_hash).unwrap())
                        })
                        .collect();
                } else if tracker.is_some() {
                    snapshots = Vec::new();
                } else {
                    snapshots = emitter.collect(&manager);
                }
            }
            let consumer_ms = at.elapsed().as_secs_f64() * 1000.0;
            let consumer_cpu = bench::cpu_ms() - before;
            let changed = snapshots.len();
            assert_eq!(
                changed,
                if !w.active || batch.client_resync {
                    0
                } else if batch.resync {
                    n
                } else {
                    w.unique
                },
                "{name}: {}",
                w.name
            );
            let encode = Instant::now();
            let json_bytes = if batch.client_resync {
                serde_json::to_vec(&serde_json::json!({"event":"resync_required"}))
                    .unwrap()
                    .len()
            } else if changed == 0 {
                0
            } else {
                serde_json::to_vec(&snapshots).unwrap().len()
            };
            let encode_ms = encode.elapsed().as_secs_f64() * 1000.0;
            samples.push(serde_json::json!({"producer_ms":producer_ms,"producer_cpu_ms":producer_cpu,
                "consumer_ms":consumer_ms,"consumer_cpu_ms":consumer_cpu,"rate_phase_ms":rate_phase_ms,
                "tracking_and_snapshot_ms":consumer_ms-rate_phase_ms,"overflow_recovery":batch.fallback,
                "client_resync_required":batch.client_resync,
                "json_encode_ms":encode_ms,"json_bytes":json_bytes,"rows":changed,"resync":batch.resync}));
        }
        bench::emit(
            "tracking",
            serde_json::json!({"strategy":name,"catalogue":n,"workload":w.name,
            "unique":w.unique,"updates":w.updates,"writers":w.writers,"active":w.active,"batched":w.batched,"spread_ids":w.spread,
            "queue_capacity":CAPACITY,"shards":SHARDS,"reserved_payload_bytes":tracker.as_ref().map_or(
                emitter.last.capacity()*std::mem::size_of::<(InfoHash,(Stamp,u64))>(),|t|t.bytes()),
            "rss_before":rss_before,"rss_primed":rss_primed,"samples":samples}),
        );
    }
    drop(listener);
    drop(tracker);
    drop(rows);
    drop(manager);
    std::fs::remove_dir_all(root).ok();
}
