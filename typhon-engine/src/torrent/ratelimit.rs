//! Byte-rate limits: token buckets in front of what a peer session sends and
//! asks for.
//!
//! `rate.rs` only MEASURES. Until this module, `upload_limit` / `download_limit`
//! were read by nobody, `set_upload_limit` answered `{"ok":true}` and did
//! nothing, and the UI offered an upload cap that capped nothing.
//!
//! Three levels, every one optional, and a block has to clear all of them:
//!   * the client -- what qBittorrent calls the global limit, one for the whole
//!     process (`transfer/setUploadLimit`), owned by the binary and handed to
//!     every engine;
//!   * the engine -- `upload_rate_limit` / `download_rate_limit` of a
//!     `[race]` / `[hoard]` / `[[engine]]` section;
//!   * the torrent -- `torrents/setUploadLimit`, persisted with its resume
//!     record.
//!
//! ## The bucket
//!
//! GCRA ("virtual scheduling"), which is a token bucket written as ONE number:
//! the theoretical arrival time (TAT) of the next byte. A bucket is three
//! atomics and a reservation is one `fetch_update`, so there is no lock on the
//! path every block takes, and changing the rate is a store the next block
//! sees.
//!
//! A reservation always COMMITS and answers the instant the bytes may go. That
//! is deliberate. The alternative -- "try, and come back when there is a token"
//! -- has every waiting session wake at the same refill instant, one of them
//! win and the rest sleep again: a thousand peers behind a 1 MB/s cap would be
//! ~60k wake-ups a second to move 60 blocks. Reserving hands each waiter its
//! own slot, in arrival order, and it sleeps exactly once.
//!
//! ## Cost at a million torrents
//!
//! A torrent with no limit of its own carries one empty `OnceLock` (8 bytes);
//! its buckets are allocated the first time a limit is set on it and never
//! otherwise. A session that is not limited pays three atomic loads per block
//! and keeps no timer. Nothing here runs on a tick: an idle torrent costs
//! nothing at all.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

use tokio::time::Instant;

/// Burst a bucket tolerates, as a slice of its rate. A quarter of a second
/// keeps the overshoot over any window of a few seconds under ~10%, while
/// still letting a pipelined peer receive several blocks back to back.
const BURST_WINDOW_NS: u128 = 250_000_000;
/// The burst never goes below two blocks: under it, a slow cap would space
/// every 16 KiB block out on its own and a peer would see a trickle of single
/// blocks, which some clients read as a stalled connection.
const MIN_BURST_BYTES: u128 = 32 * 1024;
/// Longest a session sleeps on one reservation before looking again. A rate
/// lifted (or lowered) while a session waits for a slot booked under the old
/// one is noticed within this, instead of after a wait the new rate no longer
/// justifies.
pub const MAX_WAIT: Duration = Duration::from_secs(1);

/// The origin of every TAT. A monotonic clock read once, so a bucket's state
/// fits in a u64 of nanoseconds.
///
/// Set a day in the past, so no "now" can ever saturate to zero -- which would
/// make every bucket look idle -- whatever runtime or clock first reads it.
static BASE: LazyLock<Instant> = LazyLock::new(|| {
    let now = Instant::now();
    now.checked_sub(Duration::from_secs(86_400)).unwrap_or(now)
});

fn now_ns() -> u64 {
    Instant::now().saturating_duration_since(*BASE).as_nanos() as u64
}

fn instant_of(ns: u64) -> Instant {
    *BASE + Duration::from_nanos(ns)
}

/// Which way the bytes move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
}

/// One token bucket, in bytes per second.
pub struct RateBucket {
    /// Bytes per second. 0 = unlimited.
    rate: AtomicU64,
    /// Theoretical arrival time, in ns since `BASE`. 0 = idle (a full burst
    /// available).
    tat: AtomicU64,
    /// Bumped on every change of rate, so a session holding a reservation made
    /// under the old rate knows to drop it.
    generation: AtomicU64,
}

impl Default for RateBucket {
    fn default() -> Self {
        Self::new()
    }
}

impl RateBucket {
    pub const fn new() -> Self {
        Self { rate: AtomicU64::new(0), tat: AtomicU64::new(0), generation: AtomicU64::new(0) }
    }

    /// The cap, in bytes per second. 0 = unlimited.
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn is_limited(&self) -> bool {
        self.rate() != 0
    }

    /// Set the cap. 0 = unlimited. Live: the next block reads it.
    ///
    /// A change also forgets the reservations booked under the old rate. Kept,
    /// a cap lifted from 10 KiB/s to unlimited would still make every waiting
    /// session sit out a queue minted at 10 KiB/s.
    pub fn set_rate(&self, bytes_per_sec: u64) {
        if self.rate.swap(bytes_per_sec, Ordering::Relaxed) != bytes_per_sec {
            self.tat.store(0, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Book `n` bytes no earlier than `not_before` (ns since `BASE`), and
    /// answer when they may go. Unlimited answers `not_before`.
    ///
    /// GCRA: the bytes may go at `max(not_before, TAT - tau)`, and the TAT then
    /// moves `n / rate` past whichever is later of itself and that instant.
    /// `tau` is the burst, in time.
    fn reserve(&self, n: u64, not_before: u64) -> u64 {
        let rate = self.rate() as u128;
        if rate == 0 {
            return not_before;
        }
        let cost = (n as u128 * 1_000_000_000 / rate) as u64;
        let burst = (rate * BURST_WINDOW_NS / 1_000_000_000).max(MIN_BURST_BYTES);
        let tau = (burst * 1_000_000_000 / rate) as u64;
        let mut send = not_before;
        let _ = self.tat.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |tat| {
            let s = not_before.max(tat.saturating_sub(tau));
            send = s;
            Some(tat.max(s).saturating_add(cost))
        });
        send
    }
}

/// An upload bucket and a download bucket.
#[derive(Default)]
pub struct RatePair {
    pub up: RateBucket,
    pub down: RateBucket,
}

impl RatePair {
    pub fn get(&self, dir: Dir) -> &RateBucket {
        match dir {
            Dir::Up => &self.up,
            Dir::Down => &self.down,
        }
    }
}

/// The limits of one engine, and of the client above it.
#[derive(Default)]
pub struct EngineRates {
    pub engine: RatePair,
    /// The process-wide pair, shared by every engine of the binary. Unset for
    /// an engine on its own (a test, the RPC binary), which then has no client
    /// level at all.
    client: OnceLock<Arc<RatePair>>,
}

impl EngineRates {
    /// Put this engine under a client-wide pair. First call wins: the binary
    /// owns one pair for its whole life.
    pub fn set_client(&self, client: Arc<RatePair>) {
        let _ = self.client.set(client);
    }

    pub fn client(&self) -> Option<&RatePair> {
        self.client.get().map(|c| c.as_ref())
    }
}

/// The limits a torrent runs under. Unlimited at every level.
pub static UNLIMITED: LazyLock<EngineRates> = LazyLock::new(EngineRates::default);

/// The buckets one block has to clear, narrowest first.
pub struct Chain<'a> {
    levels: [Option<&'a RateBucket>; 3],
}

impl<'a> Chain<'a> {
    pub fn new(torrent: Option<&'a RatePair>, engine: &'a EngineRates, dir: Dir) -> Self {
        Self {
            levels: [
                torrent.map(|p| p.get(dir)),
                Some(engine.engine.get(dir)),
                engine.client().map(|p| p.get(dir)),
            ],
        }
    }

    fn buckets(&self) -> impl Iterator<Item = &'a RateBucket> + '_ {
        self.levels.iter().flatten().copied()
    }

    pub fn is_limited(&self) -> bool {
        self.buckets().any(|b| b.is_limited())
    }

    /// Changes whenever any level's rate does, or a level appears.
    fn generation(&self) -> u64 {
        self.levels
            .iter()
            .map(|l| l.map_or(0, |b| b.generation().wrapping_add(1)))
            .fold(0u64, |a, g| a.wrapping_mul(31).wrapping_add(g))
    }

    /// Book `n` bytes through every limited level, and answer when they may go.
    ///
    /// Each level books no earlier than the one before it allows: a block held
    /// by its torrent's cap must not take an engine slot NOW and use it later,
    /// or the engine would see bytes arrive in a bunch it believes it spread.
    fn reserve(&self, n: u64, now: u64) -> u64 {
        let mut at = now;
        for b in self.buckets() {
            if b.is_limited() {
                at = b.reserve(n, at);
            }
        }
        at
    }
}

/// What a gate says about the next block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Send it now.
    Now,
    /// Not before this instant. Capped at `MAX_WAIT` from now, so the caller
    /// looks again even when the slot is further away.
    Wait(Instant),
}

/// One direction of one peer session.
///
/// Holds at most one reservation. `credit` is what has been booked and not yet
/// spent, `due` when it may be: the first block that cannot go now books its
/// bytes, and the same block -- or whichever comes next, blocks are all 16 KiB
/// but the last -- spends them when the slot comes. A session therefore books
/// once per block and never twice for the same one.
#[derive(Debug, Default)]
pub struct Gate {
    credit: u64,
    due: u64,
    generation: u64,
}

impl Gate {
    pub fn admit(&mut self, n: u64, chain: &Chain<'_>) -> Admit {
        let generation = chain.generation();
        if !chain.is_limited() {
            *self = Gate { generation, ..Gate::default() };
            return Admit::Now;
        }
        if generation != self.generation {
            // Booked under a rate that no longer holds: forget it, book anew.
            *self = Gate { generation, ..Gate::default() };
        }
        let now = now_ns();
        if self.credit > 0 && self.due > now {
            return Admit::Wait(capped(self.due, now));
        }
        if self.credit < n {
            let need = n - self.credit;
            self.due = chain.reserve(need, now);
            self.credit += need;
            if self.due > now {
                return Admit::Wait(capped(self.due, now));
            }
        }
        self.credit -= n;
        Admit::Now
    }
}

fn capped(due: u64, now: u64) -> Instant {
    instant_of(due.min(now.saturating_add(MAX_WAIT.as_nanos() as u64)))
}

/// Reserve `n` bytes through `chain` and sleep until they may go. For the paths
/// that are a single transfer in their own task -- the BEP 19 webseed fetches --
/// where waiting holds up nothing else.
pub async fn acquire(n: u64, chain: &Chain<'_>) {
    if !chain.is_limited() {
        return;
    }
    let now = now_ns();
    let at = chain.reserve(n, now);
    if at > now {
        tokio::time::sleep_until(instant_of(at)).await;
    }
}

/// KiB/s, as a config file and a human write it, to the bytes/s a bucket holds.
/// Negative and zero both mean unlimited.
pub fn kib_to_bytes(kib: i64) -> u64 {
    if kib <= 0 { 0 } else { (kib as u64).saturating_mul(1024) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `total` bytes through a gate in 16 KiB blocks, sleeping when told,
    /// and answer how long it took.
    async fn pump(gate: &mut Gate, chain: &Chain<'_>, total: u64) -> Duration {
        let start = Instant::now();
        let mut sent = 0;
        while sent < total {
            match gate.admit(16384, chain) {
                Admit::Now => sent += 16384,
                Admit::Wait(t) => tokio::time::sleep_until(t).await,
            }
        }
        start.elapsed()
    }

    /// ⭐ The average rate holds over a window: 1 MiB at 512 KiB/s takes about
    /// two seconds, less the burst a fresh bucket hands out.
    #[tokio::test]
    async fn the_average_rate_holds_over_a_window() {
        let engine = EngineRates::default();
        engine.engine.up.set_rate(512 * 1024);
        let chain = Chain::new(None, &engine, Dir::Up);
        let mut gate = Gate::default();
        let took = pump(&mut gate, &chain, 1024 * 1024).await;
        // Burst = 128 KiB = 0.25 s of credit up front: the last block is due
        // at 1.72 s.
        assert!(took >= Duration::from_millis(1700), "1 MiB at 512 KiB/s took {took:?}");
        assert!(took <= Duration::from_millis(2600), "and not much more: {took:?}");
    }

    #[tokio::test]
    async fn unlimited_lets_everything_through_at_once() {
        let engine = EngineRates::default();
        let chain = Chain::new(None, &engine, Dir::Up);
        let mut gate = Gate::default();
        for _ in 0..10_000 {
            assert_eq!(gate.admit(16384, &chain), Admit::Now);
        }
    }

    /// ⭐ A rate set under a live gate bites at once, and lifting it releases
    /// a session that was waiting on a slot booked under the old rate.
    #[tokio::test]
    async fn a_rate_change_applies_to_a_live_gate() {
        let engine = EngineRates::default();
        let chain = Chain::new(None, &engine, Dir::Down);
        let mut gate = Gate::default();
        assert_eq!(gate.admit(16384, &chain), Admit::Now);

        engine.engine.down.set_rate(16 * 1024);
        // The 32 KiB burst floor plus the block in hand: three go, the fourth
        // waits.
        for _ in 0..3 {
            assert_eq!(gate.admit(16384, &chain), Admit::Now);
        }
        let Admit::Wait(t) = gate.admit(16384, &chain) else { panic!("16 KiB/s must bite") };
        assert!(t > Instant::now());

        engine.engine.down.set_rate(0);
        assert_eq!(gate.admit(16384, &chain), Admit::Now, "lifting the cap releases at once");
    }

    /// A long reservation is reported in steps of at most `MAX_WAIT`, so a
    /// waiting session looks again and can notice a change of rate.
    #[tokio::test]
    async fn a_wait_is_never_longer_than_max_wait() {
        let engine = EngineRates::default();
        engine.engine.up.set_rate(1024); // 16 s per block
        let chain = Chain::new(None, &engine, Dir::Up);
        let mut gate = Gate::default();
        for _ in 0..3 {
            let _ = gate.admit(16384, &chain);
        }
        match gate.admit(16384, &chain) {
            Admit::Wait(t) => assert!(t <= Instant::now() + MAX_WAIT),
            Admit::Now => panic!("1 KiB/s cannot send three blocks at once"),
        }
    }

    /// ⭐ The hierarchy: a block clears the narrowest level. A torrent capped
    /// under a generous engine runs at the torrent's rate, and the reverse.
    #[tokio::test]
    async fn the_narrowest_level_decides() {
        let engine = EngineRates::default();
        let torrent = RatePair::default();
        engine.engine.up.set_rate(4 * 1024 * 1024);
        torrent.up.set_rate(256 * 1024);
        let chain = Chain::new(Some(&torrent), &engine, Dir::Up);
        let took = pump(&mut Gate::default(), &chain, 512 * 1024).await;
        assert!(took >= Duration::from_millis(1600), "the torrent's 256 KiB/s: {took:?}");

        let engine = EngineRates::default();
        let torrent = RatePair::default();
        engine.engine.up.set_rate(256 * 1024);
        torrent.up.set_rate(4 * 1024 * 1024);
        let chain = Chain::new(Some(&torrent), &engine, Dir::Up);
        let took = pump(&mut Gate::default(), &chain, 512 * 1024).await;
        assert!(took >= Duration::from_millis(1600), "the engine's 256 KiB/s: {took:?}");
    }

    /// The client level is shared by every engine under it: two engines each
    /// pumping 256 KiB under a 256 KiB/s client cap take two seconds together,
    /// not one each.
    #[tokio::test]
    async fn the_client_level_is_shared_between_engines() {
        let client = Arc::new(RatePair::default());
        client.up.set_rate(256 * 1024);
        let (a, b) = (EngineRates::default(), EngineRates::default());
        a.set_client(client.clone());
        b.set_client(client.clone());
        let (ca, cb) = (Chain::new(None, &a, Dir::Up), Chain::new(None, &b, Dir::Up));
        let (mut ga, mut gb) = (Gate::default(), Gate::default());
        let start = Instant::now();
        let ((), ()) = tokio::join!(
            async { pump(&mut ga, &ca, 256 * 1024).await; },
            async { pump(&mut gb, &cb, 256 * 1024).await; },
        );
        // 512 KiB at 256 KiB/s, less the burst: the last block is due at 1.69 s.
        assert!(start.elapsed() >= Duration::from_millis(1600), "{:?}", start.elapsed());
    }

    /// Many sessions behind one cap share it: the total, not each of them,
    /// is held to the rate.
    #[tokio::test]
    async fn many_sessions_share_one_engine_cap() {
        let engine = Arc::new(EngineRates::default());
        engine.engine.up.set_rate(256 * 1024);
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let engine = engine.clone();
            tasks.push(tokio::spawn(async move {
                let chain = Chain::new(None, &engine, Dir::Up);
                pump(&mut Gate::default(), &chain, 64 * 1024).await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        // 512 KiB at 256 KiB/s: two seconds, less the 64 KiB burst.
        assert!(start.elapsed() >= Duration::from_millis(1600), "{:?}", start.elapsed());
    }

    #[test]
    fn kib_converts_and_non_positive_is_unlimited() {
        assert_eq!(kib_to_bytes(0), 0);
        assert_eq!(kib_to_bytes(-1), 0);
        assert_eq!(kib_to_bytes(100), 102_400);
    }
}
