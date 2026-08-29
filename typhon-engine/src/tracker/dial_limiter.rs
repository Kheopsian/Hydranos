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

/// Live peer connections, inbound and outbound alike. Maintained by
/// `PeerGuard` (RAII), so it is decremented even when a session panics.
pub static LIVE_CONNS: AtomicUsize = AtomicUsize::new(0);

/// Ceiling on `LIVE_CONNS`. 0 = unlimited.
static MAX_CONNS: AtomicUsize = AtomicUsize::new(0);

/// Live cap on outbound dials per second, held as `f64` bits. 0 = unlimited.
///
/// This lives here rather than in `DialPacer` so the rate can be changed while
/// the engine runs: the pacer re-reads it on every `acquire`, so a new value
/// takes effect on the next dial instead of at the next restart. Restarting a
/// 200k-torrent hoard to try a rate is not a knob anyone would turn twice.
static MAX_DIALS_PER_SEC: AtomicU64 = AtomicU64::new(0);

/// Startup pause. While set, no outbound dial leaves the process. This is a
/// process-level gate on purpose: it must never be written into per-torrent
/// paused state, or lifting it would resume the torrents the user had
/// deliberately paused and destroy that intent silently.
static DIALS_PAUSED: AtomicBool = AtomicBool::new(false);

/// Dials refused because the connection ceiling was reached.
pub static DIAL_SKIPPED_CONN_CAP: AtomicU64 = AtomicU64::new(0);
/// Dials refused because the startup pause was in force.
pub static DIAL_SKIPPED_PAUSED: AtomicU64 = AtomicU64::new(0);
/// Dials that had to wait on the token bucket.
pub static DIAL_DELAYED: AtomicU64 = AtomicU64::new(0);

pub fn set_max_connections(n: usize) {
    MAX_CONNS.store(n, Ordering::Relaxed);
}

pub fn max_connections() -> usize {
    MAX_CONNS.load(Ordering::Relaxed)
}

/// True when a new connection would exceed the configured ceiling. Always
/// false when no ceiling is set.
pub fn conn_cap_reached() -> bool {
    let cap = MAX_CONNS.load(Ordering::Relaxed);
    cap != 0 && LIVE_CONNS.load(Ordering::Relaxed) >= cap
}

/// Sets the outbound dial ceiling in dials per second. Anything that is not a
/// finite positive number means "no limit", so a caller cannot wedge the pacer
/// with a NaN and stop every dial in the process.
pub fn set_max_dials_per_sec(per_sec: f64) {
    let v = if per_sec.is_finite() && per_sec > 0.0 { per_sec } else { 0.0 };
    MAX_DIALS_PER_SEC.store(v.to_bits(), Ordering::Relaxed);
}

/// The live dial rate. 0 = unlimited.
pub fn max_dials_per_sec() -> f64 {
    f64::from_bits(MAX_DIALS_PER_SEC.load(Ordering::Relaxed))
}

pub fn set_dials_paused(paused: bool) {
    DIALS_PAUSED.store(paused, Ordering::Relaxed);
}

pub fn dials_paused() -> bool {
    DIALS_PAUSED.load(Ordering::Relaxed)
}

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
    pub async fn acquire(&mut self) {
        // Counted once per dial that had to wait, not once per sleep: a single
        // dial can go round this loop several times, and counting each pass
        // would report a backlog far worse than the real one.
        let mut counted = false;
        loop {
            // Re-read every pass: the rate can change mid-wait, and a dial
            // already sleeping on the old rate must be released by the new one.
            let rate = max_dials_per_sec();
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
                DIAL_DELAYED.fetch_add(1, Ordering::Relaxed);
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

    /// The rate is a process global, so these tests cannot run concurrently
    /// with each other and stay meaningful.
    static RATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn non_positive_and_nan_rates_mean_unlimited() {
        let _g = RATE_LOCK.lock().unwrap();
        for bad in [0.0, -1.0, f64::NAN, f64::NEG_INFINITY] {
            set_max_dials_per_sec(bad);
            assert_eq!(max_dials_per_sec(), 0.0, "{bad} should read back as unlimited");
        }
        set_max_dials_per_sec(0.0);
    }

    #[tokio::test]
    async fn unlimited_rate_does_not_pace() {
        let _g = RATE_LOCK.lock().unwrap();
        set_max_dials_per_sec(0.0);
        let mut p = DialPacer::new();
        let start = Instant::now();
        for _ in 0..1000 {
            p.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "no ceiling must not pace");
    }

    #[tokio::test]
    async fn sub_unit_rate_still_gets_one_token() {
        // A rate of 0.5/s must not produce a bucket that can never fill.
        let _g = RATE_LOCK.lock().unwrap();
        set_max_dials_per_sec(0.5);
        let mut p = DialPacer::new();
        p.acquire().await;
        assert_eq!(p.burst, 1.0);
        set_max_dials_per_sec(0.0);
    }

    #[tokio::test]
    async fn burst_drains_then_paces() {
        let _g = RATE_LOCK.lock().unwrap();
        set_max_dials_per_sec(10.0);
        let mut p = DialPacer::new();
        // The bucket starts empty, so credit has to be earned: ten dials at
        // 10/s cannot all clear inside a tenth of a second.
        let start = Instant::now();
        for _ in 0..3 {
            p.acquire().await;
        }
        assert!(
            start.elapsed() >= Duration::from_millis(200),
            "3 dials at 10/s should have taken ~300ms, took {:?}",
            start.elapsed()
        );
        set_max_dials_per_sec(0.0);
    }

    /// ⭐ The point of the whole change: a pacer built while the engine was
    /// unlimited must start pacing when the rate is set under it, with no
    /// restart and no reconstruction. Pinning the rate in `DialPacer::new`
    /// (what the code used to do) fails this.
    #[tokio::test]
    async fn rate_change_applies_to_a_live_pacer() {
        let _g = RATE_LOCK.lock().unwrap();
        set_max_dials_per_sec(0.0);
        let mut p = DialPacer::new();
        for _ in 0..50 {
            p.acquire().await;
        }

        set_max_dials_per_sec(4.0);
        let start = Instant::now();
        for _ in 0..3 {
            p.acquire().await;
        }
        let tightened = start.elapsed();
        assert!(
            tightened >= Duration::from_millis(500),
            "a rate set under a live pacer must bite: 3 dials at 4/s took {tightened:?}"
        );

        // ...and lifting it must release just as promptly.
        set_max_dials_per_sec(0.0);
        let start = Instant::now();
        for _ in 0..100 {
            p.acquire().await;
        }
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "lifting the ceiling must stop pacing at once"
        );
    }

    #[test]
    fn conn_cap_is_off_by_default() {
        set_max_connections(0);
        LIVE_CONNS.store(999_999, Ordering::Relaxed);
        assert!(!conn_cap_reached(), "no ceiling means never capped");
        set_max_connections(10);
        LIVE_CONNS.store(9, Ordering::Relaxed);
        assert!(!conn_cap_reached());
        LIVE_CONNS.store(10, Ordering::Relaxed);
        assert!(conn_cap_reached());
        // Leave the globals clean for other tests in this binary.
        set_max_connections(0);
        LIVE_CONNS.store(0, Ordering::Relaxed);
    }
}
