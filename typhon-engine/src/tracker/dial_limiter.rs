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
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl DialPacer {
    /// `per_sec <= 0` means "no limit" and yields None, which the consumer
    /// treats as a no-op.
    pub fn new(per_sec: f64) -> Option<Self> {
        if per_sec <= 0.0 {
            return None;
        }
        // One second of credit, never below a single token: a rate under 1/s
        // must still let one dial through.
        let burst = if per_sec < 1.0 { 1.0 } else { per_sec };
        Some(Self {
            rate: per_sec,
            burst,
            tokens: burst,
            last: Instant::now(),
        })
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
            let now = Instant::now();
            self.tokens += now.duration_since(self.last).as_secs_f64() * self.rate;
            self.last = now;
            if self.tokens > self.burst {
                self.tokens = self.burst;
            }
            if self.tokens >= 1.0 {
                self.tokens -= 1.0;
                return;
            }
            if !counted {
                DIAL_DELAYED.fetch_add(1, Ordering::Relaxed);
                counted = true;
            }
            let need = (1.0 - self.tokens) / self.rate;
            tokio::time::sleep(Duration::from_secs_f64(need.max(0.001))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rate_means_no_pacer() {
        assert!(DialPacer::new(0.0).is_none());
        assert!(DialPacer::new(-1.0).is_none());
    }

    #[test]
    fn sub_unit_rate_still_gets_one_token() {
        // A rate of 0.5/s must not produce a bucket that can never fill.
        let p = DialPacer::new(0.5).expect("pacer");
        assert_eq!(p.burst, 1.0);
        assert_eq!(p.tokens, 1.0);
    }

    #[tokio::test]
    async fn burst_drains_then_paces() {
        // 10/s: the first 10 dials are free (one second of credit), the 11th
        // has to wait for the bucket to refill.
        let mut p = DialPacer::new(10.0).expect("pacer");
        let start = Instant::now();
        for _ in 0..10 {
            p.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "burst should not pace");
        p.acquire().await;
        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "11th dial should have waited for a token"
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
