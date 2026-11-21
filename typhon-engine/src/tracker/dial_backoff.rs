//! Peers whose dial recently failed, so they are not dialled again at once.
//!
//! Trackers, DHT and LSD hand back the same addresses over and over. A peer
//! that refused us, timed out or failed the handshake would otherwise be
//! dialled again on every one of those repeats, each time spending a slot of
//! the engine's dial rate on a connection that is very likely to fail again.
//!
//! The table belongs to one engine's dial queue: a peer unreachable through
//! one engine's tunnel may well answer another's.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A torrent and one of its peers.
pub type DialKey = ([u8; 20], SocketAddr);

/// How long a failed peer is left alone. Shorter than a tracker's usual
/// re-announce interval, so a peer that was only briefly down is tried again
/// by the next announce, but long enough to absorb the DHT, LSD and PEX
/// repeats that arrive in between.
pub const DIAL_BACKOFF: Duration = Duration::from_secs(5 * 60);

/// Most failures remembered at once. At the default 50 dials/s, five minutes
/// of nothing but failures is 15 000 entries, so this holds a whole backoff
/// window at that rate; past it the oldest failures are forgotten first. One
/// entry is under 100 bytes in each of the map and the queue.
pub const DIAL_BACKOFF_CAP: usize = 32_768;

/// Bounded, expiring set of recently failed dials.
#[derive(Debug)]
pub struct DialBackoff {
    ttl: Duration,
    cap: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// When each key's backoff ends.
    until: HashMap<DialKey, Instant>,
    /// Every failure in the order it was recorded. A key failing again is
    /// pushed again; the older copy is recognised as stale when it reaches
    /// the front because its time no longer matches `until`.
    order: VecDeque<(DialKey, Instant)>,
}

impl DialBackoff {
    /// A table that keeps each failure for `ttl` and at most `cap` of them
    /// (at least one).
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap: cap.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Remember that dialling `key` just failed.
    pub fn record_failure(&self, key: DialKey, now: Instant) {
        let until = now + self.ttl;
        let mut inner = self.lock();
        inner.until.insert(key, until);
        inner.order.push_back((key, until));
        inner.prune(now, self.cap);
    }

    /// Whether `key` failed recently enough that it should not be dialled.
    pub fn is_backing_off(&self, key: &DialKey, now: Instant) -> bool {
        let mut inner = self.lock();
        inner.prune(now, self.cap);
        inner.until.get(key).is_some_and(|until| *until > now)
    }

    /// Failures currently remembered.
    pub fn len(&self) -> usize {
        self.lock().until.len()
    }

    /// Whether no failure is remembered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves the table consistent enough
        // for a cache: at worst one peer is dialled early or skipped once.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Default for DialBackoff {
    fn default() -> Self {
        Self::new(DIAL_BACKOFF, DIAL_BACKOFF_CAP)
    }
}

impl Inner {
    /// Drop expired entries from the front, then the oldest ones while the
    /// queue is over `cap`. Every live key has its latest copy in the queue,
    /// so bounding the queue bounds the map too.
    fn prune(&mut self, now: Instant, cap: usize) {
        while let Some(&(key, until)) = self.order.front() {
            if until > now && self.order.len() <= cap {
                break;
            }
            self.order.pop_front();
            // Only the latest copy owns the map entry: an older one leaving
            // must not end a backoff that a later failure extended.
            if self.until.get(&key) == Some(&until) {
                self.until.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u16) -> DialKey {
        ([1; 20], SocketAddr::from(([192, 0, 2, 1], n)))
    }

    #[test]
    fn a_failed_peer_is_skipped_until_its_backoff_ends() {
        let table = DialBackoff::new(Duration::from_secs(60), 16);
        let t0 = Instant::now();
        assert!(!table.is_backing_off(&key(1), t0), "nothing failed yet");
        table.record_failure(key(1), t0);
        assert!(table.is_backing_off(&key(1), t0 + Duration::from_secs(59)));
        assert!(
            !table.is_backing_off(&key(1), t0 + Duration::from_secs(60)),
            "the backoff must end"
        );
        assert!(table.is_empty(), "an expired entry is dropped, not kept");
    }

    #[test]
    fn the_backoff_is_per_torrent_and_per_address() {
        let table = DialBackoff::new(Duration::from_secs(60), 16);
        let t0 = Instant::now();
        table.record_failure(key(1), t0);
        assert!(!table.is_backing_off(&key(2), t0), "another port");
        let other_torrent = ([2; 20], key(1).1);
        assert!(!table.is_backing_off(&other_torrent, t0), "another torrent");
    }

    /// ⭐ A second failure restarts the backoff. The first failure's queue
    /// entry expiring must not end it early.
    #[test]
    fn a_second_failure_extends_the_backoff() {
        let table = DialBackoff::new(Duration::from_secs(60), 16);
        let t0 = Instant::now();
        table.record_failure(key(1), t0);
        table.record_failure(key(1), t0 + Duration::from_secs(30));
        assert!(
            table.is_backing_off(&key(1), t0 + Duration::from_secs(70)),
            "the stale first entry ended a backoff the second one set"
        );
        assert!(!table.is_backing_off(&key(1), t0 + Duration::from_secs(90)));
    }

    /// ⭐ The table never holds more than its cap, however many peers fail
    /// inside one backoff window; the oldest failures go first.
    #[test]
    fn the_table_stays_bounded_and_forgets_the_oldest_first() {
        let table = DialBackoff::new(Duration::from_secs(3600), 100);
        let t0 = Instant::now();
        for n in 0..10_000u16 {
            table.record_failure(key(n), t0);
        }
        assert_eq!(table.len(), 100);
        assert!(table.lock().order.len() <= 100, "the queue is bounded too");
        assert!(!table.is_backing_off(&key(0), t0), "the oldest was evicted");
        assert!(table.is_backing_off(&key(9_999), t0), "the newest is kept");
    }

    /// The same peer failing over and over must not grow the queue past the
    /// cap with stale copies of itself.
    #[test]
    fn repeated_failures_of_one_peer_stay_bounded() {
        let table = DialBackoff::new(Duration::from_secs(3600), 8);
        let t0 = Instant::now();
        for s in 0..1_000u64 {
            table.record_failure(key(1), t0 + Duration::from_millis(s));
        }
        assert!(table.lock().order.len() <= 8);
        assert!(table.is_backing_off(&key(1), t0 + Duration::from_secs(1)));
    }
}
