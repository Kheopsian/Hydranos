//! Outbound dial pacing, the live-connection ceiling, and the startup pause.
//!
//! Announces were rate limited first (`announce_rate_limit`, Go side), on the
//! theory that a hoard announcing in waves was what drowned a VPN tunnel. That
//! only capped the trigger, not the amplification: one announce asks for
//! `numwant=200` peers, and every peer that comes back is dialed immediately.
//! At 20 announces/s that is up to 4000 new outbound flows per second through
//! a single tunnel — which is what actually kills it. qBittorrent's equivalent
//! knob is connections per second, not announces per second, and this is ours.
//!
//! Three controls, all off by default so an unconfigured engine behaves
//! exactly as it always has:
//!   * `max_dials_per_sec` — token bucket in front of every outbound dial.
//!   * `max_connections` — ceiling on live peer connections. Note this config
//!     key existed and was echoed by `get_config` for a long time while being
//!     enforced nowhere; setting it did nothing at all.
//!
//!     It bounds what this engine *opens*: at the ceiling we stop dialing, but
//!     an inbound peer is still accepted. Inbound connections count towards
//!     the total (so they do shut dialing down), they are just never refused
//!     -- turning away a peer that wants to leech from us would trade upload,
//!     the thing a seedbox exists for, against a number. The live count is
//!     therefore allowed to sit above the ceiling; what it cannot do is climb
//!     there by our own doing.
//!   * the startup pause — a process-level gate, see `dials_paused`.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use tokio::time::{Duration, Instant};

/// The dial ceilings and gauges of ONE engine.
///
/// Every field here was a static, which described reality while one engine
/// meant one process. Sharing them between race and hoard would give the two a
/// single connection ceiling and a single dial rate -- the last engine to
/// start deciding for both -- and would report one engine's refusals under the
/// other's diagnostics.
pub struct DialLimiter {
    /// Live peer connections, inbound and outbound alike. Maintained by
    /// `PeerGuard` (RAII), so it is decremented even when a session panics.
    live: AtomicUsize,
    /// Ceiling on `live`. 0 = unlimited.
    max_conns: AtomicUsize,
    /// Live cap on outbound dials per second, held as `f64` bits. 0 = unlimited.
    ///
    /// It is read on every `acquire` rather than captured by the pacer, so a
    /// new value takes effect on the next dial instead of at the next restart.
    /// Restarting a 200k-torrent hoard to try a rate is not a knob anyone would
    /// turn twice.
    max_dials_per_sec: AtomicU64,
    /// Startup pause. While set, no outbound dial leaves this engine. It is an
    /// engine-level gate on purpose: it must never be written into per-torrent
    /// paused state, or lifting it would resume the torrents the user had
    /// deliberately paused and destroy that intent silently.
    dials_paused: AtomicBool,
    /// Dials refused because the connection ceiling was reached.
    skipped_conn_cap: AtomicU64,
    /// Dials refused because the startup pause was in force.
    skipped_paused: AtomicU64,
    /// Dials that had to wait on the token bucket.
    delayed: AtomicU64,
}

impl Default for DialLimiter {
    fn default() -> Self {
        Self {
            live: AtomicUsize::new(0),
            max_conns: AtomicUsize::new(0),
            max_dials_per_sec: AtomicU64::new(0),
            dials_paused: AtomicBool::new(false),
            skipped_conn_cap: AtomicU64::new(0),
            skipped_paused: AtomicU64::new(0),
            delayed: AtomicU64::new(0),
        }
    }
}

impl DialLimiter {
    pub fn connection_opened(&self) {
        self.live.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.live.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn live_connections(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    pub fn set_max_connections(&self, n: usize) {
        self.max_conns.store(n, Ordering::Relaxed);
    }

    pub fn max_connections(&self) -> usize {
        self.max_conns.load(Ordering::Relaxed)
    }

    /// True when a new connection would exceed the configured ceiling. Always
    /// false when no ceiling is set.
    pub fn conn_cap_reached(&self) -> bool {
        let cap = self.max_conns.load(Ordering::Relaxed);
        cap != 0 && self.live.load(Ordering::Relaxed) >= cap
    }

    /// Sets the outbound dial ceiling in dials per second. Anything that is not
    /// a finite positive number means "no limit", so a caller cannot wedge the
    /// pacer with a NaN and stop every dial in the engine.
    pub fn set_max_dials_per_sec(&self, per_sec: f64) {
        let v = if per_sec.is_finite() && per_sec > 0.0 { per_sec } else { 0.0 };
        self.max_dials_per_sec.store(v.to_bits(), Ordering::Relaxed);
    }

    /// The live dial rate. 0 = unlimited.
    pub fn max_dials_per_sec(&self) -> f64 {
        f64::from_bits(self.max_dials_per_sec.load(Ordering::Relaxed))
    }

    pub fn set_dials_paused(&self, paused: bool) {
        self.dials_paused.store(paused, Ordering::Relaxed);
    }

    pub fn dials_paused(&self) -> bool {
        self.dials_paused.load(Ordering::Relaxed)
    }

    pub fn note_skipped_conn_cap(&self) {
        self.skipped_conn_cap.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_skipped_paused(&self) {
        self.skipped_paused.fetch_add(1, Ordering::Relaxed);
    }

    pub fn skipped_conn_cap(&self) -> u64 {
        self.skipped_conn_cap.load(Ordering::Relaxed)
    }

    pub fn skipped_paused(&self) -> u64 {
        self.skipped_paused.load(Ordering::Relaxed)
    }

    pub fn delayed(&self) -> u64 {
        self.delayed.load(Ordering::Relaxed)
    }
}

/// The limiter a torrent with no engine behind it runs under: unit tests, and
/// a magnet being resolved before it is added. Unlimited, which is what an
/// unconfigured engine has always been.
pub static DEFAULT_LIMITER: std::sync::LazyLock<DialLimiter> =
    std::sync::LazyLock::new(DialLimiter::default);

/// Token bucket pacing outbound dials, mirroring `announceLimiter` on the Go
/// side so the two read the same way. Owned by the single dial-queue consumer
/// task, hence no interior locking: the consumer is the only caller.
pub struct DialPacer {
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl DialPacer {
    /// The pacer is always constructed; whether it actually paces is decided
    /// per call by `max_dials_per_sec`. There is no "unlimited" variant to
    /// build, because unlimited is a value the rate can hold and then stop
    /// holding while the process runs.
    pub fn new() -> Self {
        Self {
            burst: 0.0,
            tokens: 0.0,
            last: Instant::now(),
        }
    }

    /// Block until this dial may go out. Unlike the announce limiter there is
    /// no wait cap: a dial that waits is a peer we connect to later, while a
    /// dial we drop is a peer lost until the next announce. The queue is the
    /// backlog, and dropping is what the ceiling is for.
    pub async fn acquire(&mut self, limiter: &DialLimiter) {
        // Counted once per dial that had to wait, not once per sleep: a single
        // dial can go round this loop several times, and counting each pass
        // would report a backlog far worse than the real one.
        let mut counted = false;
        loop {
            // Re-read every pass: the rate can change mid-wait, and a dial
            // already sleeping on the old rate must be released by the new one.
            let rate = limiter.max_dials_per_sec();
            let now = Instant::now();
            let elapsed = now.duration_since(self.last).as_secs_f64();
            self.last = now;
            if rate <= 0.0 {
                // Unlimited. Drop the bucket rather than keep filling it, so
                // turning a limit back on paces from empty instead of handing
                // out a burst minted while nothing was being enforced.
                self.burst = 0.0;
                self.tokens = 0.0;
                return;
            }
            // A rate under 1/s must still let one dial through eventually.
            self.burst = if rate < 1.0 { 1.0 } else { rate };
            self.tokens = (self.tokens + elapsed * rate).min(self.burst);
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            if !counted {
                limiter.delayed.fetch_add(1, Ordering::Relaxed);
                counted = true;
            }
            let need = (1.0 - self.tokens) / rate;
            tokio::time::sleep(Duration::from_secs_f64(need.max(0.001))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No RATE_LOCK any more: the rate used to be a process-wide static, so two
    // tests touching it raced and had to serialise. A limiter is a value, and
    // each test builds its own.

    #[tokio::test]
    async fn sub_unit_rate_still_gets_one_token() {
        // A rate of 0.5/s must not produce a bucket that can never fill.
        let lim = DialLimiter::default();
        lim.set_max_dials_per_sec(0.5);
        let mut p = DialPacer::new();
        p.acquire(&lim).await;
        assert_eq!(p.burst, 1.0);
    }

    #[tokio::test]
    async fn burst_drains_then_paces() {
        let lim = DialLimiter::default();
        lim.set_max_dials_per_sec(10.0);
        let mut p = DialPacer::new();
        // The bucket starts empty, so credit has to be earned: ten dials at
        // 10/s cannot all clear inside a tenth of a second.
        let start = Instant::now();
        for _ in 0..3 {
            p.acquire(&lim).await;
        }
        assert!(
            start.elapsed() >= Duration::from_millis(200),
            "3 dials at 10/s should have taken ~300ms, took {:?}",
            start.elapsed()
        );
    }

    /// ⭐ A pacer built while the engine was unlimited must start pacing when
    /// the rate is set under it, with no restart and no reconstruction.
    /// Pinning the rate in `DialPacer::new` fails this.
    #[tokio::test]
    async fn rate_change_applies_to_a_live_pacer() {
        let lim = DialLimiter::default();
        lim.set_max_dials_per_sec(0.0);
        let mut p = DialPacer::new();
        for _ in 0..50 {
            p.acquire(&lim).await;
        }

        lim.set_max_dials_per_sec(4.0);
        let start = Instant::now();
        for _ in 0..3 {
            p.acquire(&lim).await;
        }
        let tightened = start.elapsed();
        assert!(
            tightened >= Duration::from_millis(500),
            "a rate set under a live pacer must bite: 3 dials at 4/s took {tightened:?}"
        );

        // ...and lifting it must release just as promptly.
        lim.set_max_dials_per_sec(0.0);
        let start = Instant::now();
        for _ in 0..100 {
            p.acquire(&lim).await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "lifting the ceiling must stop pacing at once"
        );
    }

    #[test]
    fn conn_cap_is_off_by_default() {
        let lim = DialLimiter::default();
        lim.set_max_connections(0);
        for _ in 0..20 {
            lim.connection_opened();
        }
        assert!(!lim.conn_cap_reached(), "no ceiling means never capped");
        lim.set_max_connections(21);
        assert!(!lim.conn_cap_reached());
        lim.connection_opened();
        assert!(lim.conn_cap_reached());
    }

    /// The reason the ceilings stopped being statics: race throttled while
    /// hoard runs wide open, in one process. The old design gave both whatever
    /// the last engine to start had set.
    #[test]
    fn two_engines_hold_different_ceilings_at_once() {
        let race = DialLimiter::default();
        let hoard = DialLimiter::default();
        race.set_max_connections(50);
        hoard.set_max_connections(0);
        for _ in 0..50 {
            race.connection_opened();
            hoard.connection_opened();
        }
        assert!(race.conn_cap_reached(), "race must be at its ceiling");
        assert!(!hoard.conn_cap_reached(), "hoard has no ceiling to reach");
    }
}
