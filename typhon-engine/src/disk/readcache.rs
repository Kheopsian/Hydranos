//! The serve-side read cache: 128 KiB slices of torrent data, read once from
//! disk and kept a minute, so the next requests for the same slice (a peer
//! takes 16 KiB at a time) come out of memory.
//!
//! One per process in production (`ReadCache::process`), shared by every
//! engine: the key is the info hash, and a hash names the same bytes whichever
//! engine reads them, so engines only share a memory budget, never data that
//! could differ. A `DiskManager::new` keeps a private one, which is what tests
//! want: two fixtures of the same content are the same info hash.
//!
//! What a slice holds is what the disk held when it was read. Nothing here
//! hashes it; a piece is checked when it is written and when it is rechecked.

use bytes::Bytes;
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::ReadFailure;

/// The unit the cache reads and keeps.
pub const SLICE: u64 = 128 << 10;
/// How long a slice is trusted before it is read again.
pub const TTL: Duration = Duration::from_secs(60);
/// `[daemon] read_cache_mb` when absent.
pub const DEFAULT_MB: u64 = 256;
/// The largest `read_cache_mb` taken: 64 GiB.
pub const MAX_MB: u64 = 64 << 10;
/// Locks the slices are spread over (see `a_shared_cache_holds_up_under_many_readers`).
pub const DEFAULT_SHARDS: usize = 1;

const STRIPES: usize = 256;

/// Requests answered from memory.
pub static HIT: AtomicU64 = AtomicU64::new(0);
/// Slices read from disk.
pub static MISS: AtomicU64 = AtomicU64::new(0);
/// Requests that found their slice older than `TTL`; each is also a miss or
/// a join.
pub static STALE: AtomicU64 = AtomicU64::new(0);
/// Requests that cross a slice boundary or exceed one: read alone, not kept.
pub static BYPASS: AtomicU64 = AtomicU64::new(0);
/// Requests that waited on a read another request had started for the same
/// slice instead of reading it again.
pub static JOINED: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Hash, Eq, PartialEq, Debug)]
pub(super) struct Key {
    pub info_hash: [u8; 20],
    /// Offset of the slice in the torrent, a multiple of `SLICE`.
    pub start: u64,
}

impl Key {
    fn mix(&self) -> usize {
        let mut h = [0u8; 8];
        h.copy_from_slice(&self.info_hash[..8]);
        (u64::from_le_bytes(h) ^ (self.start / SLICE)) as usize
    }
}

/// The slices from `first` (a slice start) up to `end`.
pub(super) fn keys(info_hash: [u8; 20], first: u64, end: u64) -> impl Iterator<Item = Key> {
    (first..end)
        .step_by(SLICE as usize)
        .map(move |start| Key { info_hash, start })
}

struct Entry {
    data: Bytes,
    at: Instant,
}

struct Shard {
    lru: LruCache<Key, Entry>,
    bytes: u64,
}

impl Shard {
    fn pop(&mut self, key: &Key) {
        if let Some(e) = self.lru.pop(key) {
            self.bytes -= e.data.len() as u64;
        }
    }

    /// Drop least-recently-used slices until `cap` holds, and expired ones
    /// at the cold end while there are any: a slice nobody asks for again
    /// would otherwise keep its memory until pushed out.
    fn trim(&mut self, cap: u64) {
        while let Some((_, e)) = self.lru.peek_lru() {
            if self.bytes <= cap && e.at.elapsed() < TTL {
                break;
            }
            if let Some((_, e)) = self.lru.pop_lru() {
                self.bytes -= e.data.len() as u64;
            }
        }
    }
}

/// One read of one slice, which every request for that slice arriving while
/// it runs waits on. A `OnceCell`: if the request running it is dropped
/// (its peer went away), the next one waiting runs it instead.
pub(super) type Flight = tokio::sync::OnceCell<Result<Bytes, ReadFailure>>;

pub struct ReadCache {
    shards: Box<[Mutex<Shard>]>,
    /// The whole budget in bytes; each shard gets its share.
    budget: AtomicU64,
    /// Bumped by every write, per stripe of slices. A read that saw another
    /// value before going to disk may hold bytes the write has since
    /// replaced: it serves them once but does not keep them.
    gen: Box<[AtomicU64]>,
    flights: Mutex<HashMap<Key, Arc<Flight>>>,
}

impl ReadCache {
    pub fn new(budget_bytes: u64, shards: usize) -> Self {
        let shards = shards.max(1);
        ReadCache {
            shards: (0..shards)
                .map(|_| {
                    Mutex::new(Shard {
                        lru: LruCache::unbounded(),
                        bytes: 0,
                    })
                })
                .collect(),
            budget: AtomicU64::new(budget_bytes),
            gen: (0..STRIPES).map(|_| AtomicU64::new(0)).collect(),
            flights: Mutex::new(HashMap::new()),
        }
    }

    /// The one every engine of this process serves through.
    pub fn process() -> Arc<ReadCache> {
        static P: OnceLock<Arc<ReadCache>> = OnceLock::new();
        P.get_or_init(|| Arc::new(ReadCache::new(DEFAULT_MB << 20, DEFAULT_SHARDS)))
            .clone()
    }

    /// A new budget, now: a smaller one drops the coldest slices at once, 0
    /// empties the cache and keeps nothing more.
    pub fn set_budget(&self, bytes: u64) {
        self.budget.store(bytes, Ordering::Relaxed);
        let cap = self.shard_cap();
        for s in self.shards.iter() {
            s.lock().trim(cap);
        }
    }

    pub fn budget(&self) -> u64 {
        self.budget.load(Ordering::Relaxed)
    }

    /// Bytes held now, every shard together.
    pub fn bytes(&self) -> u64 {
        self.shards.iter().map(|s| s.lock().bytes).sum()
    }

    /// Slices held now.
    pub fn slices(&self) -> usize {
        self.shards.iter().map(|s| s.lock().lru.len()).sum()
    }

    /// A shard's share, never below one slice while there is a budget: a
    /// budget split thinner than that would keep nothing at all.
    fn shard_cap(&self) -> u64 {
        let b = self.budget();
        if b == 0 {
            return 0;
        }
        (b / self.shards.len() as u64).max(SLICE)
    }

    fn shard(&self, key: &Key) -> &Mutex<Shard> {
        &self.shards[key.mix() % self.shards.len()]
    }

    pub(super) fn generation(&self, key: &Key) -> u64 {
        self.gen[key.mix() % STRIPES].load(Ordering::SeqCst)
    }

    /// The slice, if held and younger than `TTL`. Counts the hit; an expired
    /// one is dropped and counted stale.
    pub(super) fn get(&self, key: &Key) -> Option<Bytes> {
        let mut s = self.shard(key).lock();
        let fresh = match s.lru.get(key) {
            None => return None,
            Some(e) if e.at.elapsed() < TTL => Some(e.data.clone()),
            Some(_) => None,
        };
        match fresh {
            Some(d) => {
                drop(s);
                HIT.fetch_add(1, Ordering::Relaxed);
                Some(d)
            }
            None => {
                s.pop(key);
                drop(s);
                STALE.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Keep a slice read from disk, unless a write went by since the read
    /// looked at `seen`. Checked under the shard lock, which `invalidate`
    /// takes after the write's bump: either this lands first and the write
    /// pops it, or the write came first and this read may predate it.
    pub(super) fn put(&self, key: Key, data: Bytes, seen: u64) {
        let cap = self.shard_cap();
        let len = data.len() as u64;
        if len > cap {
            return;
        }
        let mut s = self.shard(&key).lock();
        if self.generation(&key) != seen {
            return;
        }
        s.pop(&key);
        s.lru.put(
            key,
            Entry {
                data,
                at: Instant::now(),
            },
        );
        s.bytes += len;
        s.trim(cap);
    }

    /// Mark the slices from `first` to `end` as written over: a read already
    /// on its way to disk for one of them will not keep what it reads.
    pub(super) fn bump(&self, info_hash: [u8; 20], first: u64, end: u64) {
        for key in keys(info_hash, first, end) {
            self.gen[key.mix() % STRIPES].fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Forget the slices from `first` to `end`, and the reads under way for
    /// them: a request arriving after this must read the disk again, not join
    /// a read that may have started before the write.
    pub(super) fn invalidate(&self, info_hash: [u8; 20], first: u64, end: u64) {
        for key in keys(info_hash, first, end) {
            self.shard(&key).lock().pop(&key);
        }
        let mut f = self.flights.lock();
        for key in keys(info_hash, first, end) {
            f.remove(&key);
        }
    }

    /// The read under way for this slice, or a new one to run.
    pub(super) fn flight(&self, key: Key) -> FlightGuard<'_> {
        let flight = self.flights.lock().entry(key).or_default().clone();
        FlightGuard {
            cache: self,
            key,
            flight,
        }
    }

    #[cfg(test)]
    pub(super) fn holds(&self, key: &Key) -> Option<Bytes> {
        self.shard(key).lock().lru.peek(key).map(|e| e.data.clone())
    }

    /// Whether the slice is held and nothing else refers to its memory.
    #[cfg(test)]
    pub(super) fn holds_alone(&self, key: &Key) -> bool {
        self.shard(key)
            .lock()
            .lru
            .peek(key)
            .is_some_and(|e| e.data.is_unique())
    }
}

/// A request's hold on a flight. Dropped -- done or cancelled -- it takes the
/// flight off the list if it is still the one listed, so a finished read is
/// never joined and a newer one is never removed.
pub(super) struct FlightGuard<'a> {
    cache: &'a ReadCache,
    key: Key,
    pub flight: Arc<Flight>,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        if !self.flight.initialized() {
            // Not run, or its runner was dropped mid-read: anyone still
            // waiting runs it. Listed or not, leave it to them.
            if Arc::strong_count(&self.flight) > 2 {
                return;
            }
        }
        let mut f = self.cache.flights.lock();
        if f.get(&self.key)
            .is_some_and(|l| Arc::ptr_eq(l, &self.flight))
        {
            f.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(t: u8, slice: u64) -> Key {
        Key {
            info_hash: [t; 20],
            start: slice * SLICE,
        }
    }

    fn slice_of(b: u8) -> Bytes {
        Bytes::from(vec![b; SLICE as usize])
    }

    /// The budget is bytes, not slices, and a smaller one applies at once.
    #[test]
    fn the_budget_holds_and_a_smaller_one_drops_the_coldest_at_once() {
        let c = ReadCache::new(4 * SLICE, 1);
        for i in 0..10 {
            c.put(key(1, i), slice_of(i as u8), c.generation(&key(1, i)));
        }
        assert_eq!(c.bytes(), 4 * SLICE);
        assert_eq!(c.slices(), 4);
        // The four most recent are the ones kept.
        assert!(c.get(&key(1, 9)).is_some() && c.get(&key(1, 6)).is_some());
        assert!(c.get(&key(1, 5)).is_none());

        c.set_budget(2 * SLICE);
        assert_eq!(c.bytes(), 2 * SLICE);
        c.set_budget(0);
        assert_eq!(c.bytes(), 0);
        c.put(key(1, 0), slice_of(0), c.generation(&key(1, 0)));
        assert_eq!(c.slices(), 0, "0 keeps nothing");
    }

    /// The last slice of a torrent is short, and counts for what it holds.
    #[test]
    fn a_short_slice_counts_for_its_own_length() {
        let c = ReadCache::new(SLICE, 1);
        c.put(key(1, 0), Bytes::from(vec![1u8; 1000]), 0);
        c.put(key(1, 1), Bytes::from(vec![1u8; 2000]), 0);
        assert_eq!(c.bytes(), 3000);
        assert_eq!(c.slices(), 2);
    }

    /// A write between the look at the generation and the put: not kept.
    #[test]
    fn a_slice_read_across_a_write_is_not_kept() {
        let c = ReadCache::new(4 * SLICE, 1);
        let seen = c.generation(&key(1, 0));
        c.bump([1; 20], 0, SLICE);
        c.put(key(1, 0), slice_of(1), seen);
        assert!(c.get(&key(1, 0)).is_none());
    }

    /// Several shards share the budget, and each still keeps at least a slice.
    #[test]
    fn shards_split_the_budget() {
        let c = ReadCache::new(16 * SLICE, 4);
        for t in 0..8u8 {
            for i in 0..16 {
                c.put(key(t, i), slice_of(t), 0);
            }
        }
        assert!(c.bytes() <= 16 * SLICE, "{}", c.bytes());
        let thin = ReadCache::new(SLICE, 16);
        thin.put(key(1, 0), slice_of(1), 0);
        assert_eq!(thin.slices(), 1);
    }

    /// ⭐ Item 6 of the read-cache review: does one lock hold up when every
    /// peer of every engine goes through it? Ignored: a measurement, not a
    /// check. Run in release, alone:
    ///
    ///   cargo test --release --lib a_shared_cache_holds_up_under_many_readers -- --ignored --nocapture
    ///
    /// Each thread asks for random slices over a full cache (2048 slices,
    /// 64 torrents), every 8th call a put, as a miss would. What it prints is
    /// lookups per second for 1 and 16 shards. A lookup is what one 16 KiB
    /// request costs the lock: 10 Gbit/s of upload is ~76 000 of them a second.
    #[test]
    #[ignore]
    fn a_shared_cache_holds_up_under_many_readers() {
        use std::time::Instant;
        const OPS: u64 = 400_000;
        let slices = 2048u64;
        println!("threads  shards  Mlookups/s  ns/lookup(per thread)");
        for threads in [1usize, 4, 16, 64] {
            for shards in [1usize, 16] {
                let c = Arc::new(ReadCache::new(slices * SLICE, shards));
                let data = slice_of(7);
                for i in 0..slices {
                    c.put(key((i % 64) as u8, i / 64), data.clone(), 0);
                }
                let per = OPS / threads as u64;
                let t0 = Instant::now();
                let hs: Vec<_> = (0..threads)
                    .map(|n| {
                        let (c, data) = (c.clone(), data.clone());
                        std::thread::spawn(move || {
                            let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ n as u64;
                            for op in 0..per {
                                x ^= x << 13;
                                x ^= x >> 7;
                                x ^= x << 17;
                                let i = x % slices;
                                let k = key((i % 64) as u8, i / 64);
                                if op % 8 == 0 {
                                    c.put(k, data.clone(), 0);
                                } else {
                                    std::hint::black_box(c.get(&k));
                                }
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.join().unwrap();
                }
                let s = t0.elapsed().as_secs_f64();
                let total = per * threads as u64;
                println!(
                    "{threads:>7}  {shards:>6}  {:>10.2}  {:>8.0}",
                    total as f64 / s / 1e6,
                    s * 1e9 / per as f64
                );
            }
        }
    }
}
