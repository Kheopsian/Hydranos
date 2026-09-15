//! What the trackers last said about each torrent.
//!
//! An announce is the only place the swarm's size is ever known: the engine's
//! own view is connected peers, which is zero for a torrent nobody is talking
//! to right now. 3.x kept these counts in the Go front's cache; here they are
//! kept where they are produced.
//!
//! Two things read it. The trackers tab shows it. The download slot manager
//! ranks by it -- and that is why it matters: ranking by connected peers made
//! the priority effectively random, because a parked torrent reports none.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

/// The host part of a tracker announce URL.
///
/// The entry may hold either a full URL or a bare host depending on which code
/// path recorded it, so both are accepted. Port and path are dropped: the tab
/// groups by host, and `tracker.example:2810` and `tracker.example` are the
/// same tracker to an operator.
fn host_of(tracker: &str) -> String {
    let s = tracker.split("://").nth(1).unwrap_or(tracker);
    s.split(|c| c == '/' || c == ':').next().unwrap_or("").to_string()
}

/// The last answer one tracker gave about one torrent.
#[derive(Debug, Clone)]
pub struct Entry {
    /// Seeders, as the tracker counts them.
    pub complete: i64,
    /// Leechers.
    pub incomplete: i64,
    /// The tracker that answered.
    pub tracker: String,
    pub at: Instant,
    pub interval: Duration,
}

/// What one announce self-check observed about our own presence.
///
/// Three states, not a boolean. A tracker usually omits the announcing peer
/// from its own answer, so "absent" alone proves nothing. What does prove
/// something is an ASYMMETRY: seen in one family and not the other means the
/// tracker kept one address and dropped the other -- either it dedups by peer
/// id, or one family never reached it. That is the failure that cost us three
/// days of upload in September 2026, and it is invisible from inside.
#[derive(Clone, Debug)]
pub struct Verify {
    pub at: Instant,
    /// Our listen port seen on an IPv4 address in the returned peer list.
    pub v4: bool,
    /// ... and on an IPv6 one.
    pub v6: bool,
    /// The tracker returned fewer peers than we asked for, so the list was
    /// whole and an absence is real rather than a truncation.
    pub conclusive: bool,
    pub swarm: i64,
}

impl Verify {
    /// One word for the interface.
    pub fn verdict(&self) -> &'static str {
        match (self.v4, self.v6, self.conclusive) {
            (true, true, _) => "ok",
            (true, false, true) => "v6_missing",
            (false, true, true) => "v4_missing",
            (false, false, true) => "absent",
            _ => "unknown",
        }
    }
}

#[derive(Default)]
pub struct Cache {
    entries: RwLock<HashMap<String, Entry>>,
    /// Lifetime announce outcomes for this engine. Monotonic: the bench
    /// sampler turns them into a per-second rate by differencing two samples,
    /// and a counter that reset would draw a negative spike.
    announces_ok: std::sync::atomic::AtomicU64,
    announces_failed: std::sync::atomic::AtomicU64,
    /// Running sums of what the trackers last said about the whole library.
    ///
    /// Kept incrementally because the alternative is walking 300k entries on
    /// every header refresh. `record` has the displaced entry in hand, so the
    /// update is exact rather than a periodic recount.
    swarm_seeds_total: std::sync::atomic::AtomicI64,
    swarm_leechers_total: std::sync::atomic::AtomicI64,
    /// (host, error class) -> how many announces failed that way.
    ///
    /// A single number for "failed" says a tracker is unhappy; it does not say
    /// whether we are rate limited, banned, unreachable, or announcing torrents
    /// it deleted -- which are four different jobs for the operator. The class
    /// is derived from the REDACTED message: a raw reqwest error embeds the
    /// announce URL, and that URL carries the passkey.
    errors: RwLock<HashMap<(String, String), u64>>,
    /// host -> what the last announce self-check saw.
    verify: RwLock<HashMap<String, Verify>>,
}

impl Cache {
    pub fn count_ok(&self) {
        self.announces_ok.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn count_failed(&self) {
        self.announces_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Record a failure under its class, for the trackers tab.
    pub fn count_failed_kind(&self, host: &str, class: &str) {
        self.count_failed();
        *self
            .errors
            .write()
            .unwrap()
            .entry((host.to_string(), class.to_string()))
            .or_insert(0) += 1;
    }

    /// host -> [(class, count)], most frequent first.
    pub fn error_breakdown(&self) -> HashMap<String, Vec<(String, u64)>> {
        let mut out: HashMap<String, Vec<(String, u64)>> = HashMap::new();
        for ((host, class), n) in self.errors.read().unwrap().iter() {
            out.entry(host.clone()).or_default().push((class.clone(), *n));
        }
        for v in out.values_mut() {
            v.sort_by(|a, b| b.1.cmp(&a.1));
        }
        out
    }

    pub fn record_verify(&self, host: &str, v: Verify) {
        self.verify.write().unwrap().insert(host.to_string(), v);
    }

    /// The last self-check for one tracker.
    ///
    /// Separate from `verifications()` because this is read on the announce
    /// path: cloning the whole table per announce is a cost that scales with
    /// the number of trackers times the number of torrents.
    pub fn verify_for(&self, host: &str) -> Option<Verify> {
        self.verify.read().unwrap().get(host).cloned()
    }

    pub fn verifications(&self) -> HashMap<String, Verify> {
        self.verify.read().unwrap().clone()
    }

    /// (successful, failed) announces since this process started.
    pub fn outcomes(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering;
        (
            self.announces_ok.load(Ordering::Relaxed),
            self.announces_failed.load(Ordering::Relaxed),
        )
    }

    pub fn record(&self, info_hash: &str, entry: Entry) {
        use std::sync::atomic::Ordering;
        let (seeds, leechers) = (entry.complete, entry.incomplete);
        let previous = self
            .entries
            .write()
            .unwrap()
            .insert(info_hash.to_string(), entry);
        let (old_seeds, old_leechers) = previous
            .map(|e| (e.complete, e.incomplete))
            .unwrap_or((0, 0));
        self.swarm_seeds_total
            .fetch_add(seeds - old_seeds, Ordering::Relaxed);
        self.swarm_leechers_total
            .fetch_add(leechers - old_leechers, Ordering::Relaxed);
    }

    /// Seeders and leechers the trackers report across every torrent that has
    /// answered at least once.
    ///
    /// The leecher figure is the honest denominator for "peers connected vs
    /// peers available". Until 4.4.5 the header divided `unseeded_peers` by
    /// `swarm_leechers` while the API served the same number under both names,
    /// so the ratio read 100.0% on any node, always.
    pub fn swarm_totals(&self) -> (i64, i64) {
        use std::sync::atomic::Ordering;
        (
            self.swarm_seeds_total.load(Ordering::Relaxed).max(0),
            self.swarm_leechers_total.load(Ordering::Relaxed).max(0),
        )
    }

    pub fn get(&self, info_hash: &str) -> Option<Entry> {
        self.entries.read().unwrap().get(info_hash).cloned()
    }

    /// Seeders last reported for this torrent, or zero when no tracker has
    /// answered yet.
    ///
    /// Zero and "unknown" are deliberately the same answer here: a torrent
    /// nobody has heard about should sort last, and that is what zero does.
    pub fn swarm_seeds(&self, info_hash: &str) -> i64 {
        self.get(info_hash).map(|e| e.complete).unwrap_or(0)
    }

    pub fn forget(&self, info_hash: &str) {
        use std::sync::atomic::Ordering;
        if let Some(e) = self.entries.write().unwrap().remove(info_hash) {
            self.swarm_seeds_total.fetch_sub(e.complete, Ordering::Relaxed);
            self.swarm_leechers_total.fetch_sub(e.incomplete, Ordering::Relaxed);
        }
    }

    /// How many torrents each tracker last answered about, and how long ago
    /// the most recent of those answers was.
    ///
    /// This is what the trackers tab is actually asking: a tracker is known
    /// because torrents announce to it, not because the operator declared a
    /// client override for it. Listing only the declared ones showed a single
    /// row on a node announcing to several trackers.
    pub fn per_tracker(&self) -> HashMap<String, (i64, Duration)> {
        let mut out: HashMap<String, (i64, Duration)> = HashMap::new();
        for entry in self.entries.read().unwrap().values() {
            let host = host_of(&entry.tracker);
            if host.is_empty() {
                continue;
            }
            let age = entry.at.elapsed();
            let slot = out.entry(host).or_insert((0, age));
            slot.0 += 1;
            if age < slot.1 {
                slot.1 = age;
            }
        }
        out
    }

    pub fn len(&self) -> usize {
        self.entries.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(complete: i64) -> Entry {
        Entry {
            complete,
            incomplete: 0,
            tracker: "tr4ker.net".into(),
            at: Instant::now(),
            interval: Duration::from_secs(1800),
        }
    }

    #[test]
    fn a_torrent_no_tracker_answered_for_sorts_last() {
        let c = Cache::default();
        c.record("aa", entry(42));
        assert_eq!(c.swarm_seeds("aa"), 42);
        assert_eq!(c.swarm_seeds("never-announced"), 0);
    }

    #[test]
    fn the_latest_answer_replaces_the_previous_one() {
        let c = Cache::default();
        c.record("aa", entry(1));
        c.record("aa", entry(9));
        assert_eq!(c.swarm_seeds("aa"), 9);
        assert_eq!(c.len(), 1, "one entry per torrent, not one per announce");
        c.forget("aa");
        assert!(c.is_empty());
    }
}
