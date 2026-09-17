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

/// How far back the error counts look. Everything older is forgotten.
const WINDOW_MINS: u64 = 60;

/// Minutes since this process started.
///
/// Monotonic on purpose: an epoch clock can step backwards (NTP, or the RTC
/// fixups this fleet has already been bitten by) and a bucket index that moves
/// backwards would drop counts that are still inside the window.
fn now_min() -> u64 {
    static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    ORIGIN.get_or_init(Instant::now).elapsed().as_secs() / 60
}

/// One-minute buckets over the last hour, oldest first.
///
/// A deque of (minute, count) rather than a fixed [u64; 60]: a tracker that
/// fails twice a day holds two entries instead of sixty zeroes, and expiry is
/// a pop from the front instead of a scan.
#[derive(Default, Debug)]
struct Window {
    buckets: std::collections::VecDeque<(u64, u64)>,
}

impl Window {
    fn cutoff(now: u64) -> u64 {
        now.saturating_sub(WINDOW_MINS - 1)
    }

    fn add(&mut self, now: u64) {
        self.expire(now);
        match self.buckets.back_mut() {
            Some((m, n)) if *m == now => *n += 1,
            _ => self.buckets.push_back((now, 1)),
        }
    }

    fn expire(&mut self, now: u64) {
        let cutoff = Self::cutoff(now);
        while self.buckets.front().is_some_and(|(m, _)| *m < cutoff) {
            self.buckets.pop_front();
        }
    }

    /// What is still inside the window. Expiry is applied on read too, so a
    /// tracker that stopped failing reads zero without waiting for a write
    /// that may never come.
    fn total(&self, now: u64) -> u64 {
        let cutoff = Self::cutoff(now);
        self.buckets.iter().filter(|(m, _)| *m >= cutoff).map(|(_, n)| n).sum()
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
    /// (host, error class) -> how many announces failed that way IN THE LAST HOUR.
    ///
    /// A single number for "failed" says a tracker is unhappy; it does not say
    /// whether we are rate limited, banned, unreachable, or announcing torrents
    /// it deleted -- which are four different jobs for the operator. The class
    /// is derived from the REDACTED message: a raw reqwest error embeds the
    /// announce URL, and that URL carries the passkey.
    ///
    /// ⚠ This was a LIFETIME count until 2026-09-17, and a lifetime count is
    /// the wrong shape for a health signal: it only ever grows, so a tracker
    /// that broke once at boot stayed red for the life of the process and the
    /// only way to clear it was a restart -- the worst possible trigger for a
    /// panel that exists to say what is wrong RIGHT NOW. Measured that day:
    /// bt1.archive.org sat at 64126 errors while announcing successfully.
    errors: RwLock<HashMap<(String, String), Window>>,
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
        self.count_failed_kind_at(host, class, now_min());
    }

    /// The clock is a parameter so the window can be tested without sleeping
    /// for an hour.
    fn count_failed_kind_at(&self, host: &str, class: &str, now: u64) {
        let mut errors = self.errors.write().unwrap();
        errors
            .entry((host.to_string(), class.to_string()))
            .or_default()
            .add(now);
        // Drop what has aged out entirely, so a host that recovered stops
        // costing a map entry -- and, more importantly, stops being named by
        // `error_breakdown`, which is what paints its row red.
        errors.retain(|_, w| w.total(now) > 0);
    }

    /// host -> [(class, count)] over the last hour, most frequent first.
    ///
    /// A host whose failures have all aged out is ABSENT from the map rather
    /// than present with zero: the callers use the key set to decide that a
    /// tracker is in trouble, so an empty entry would keep it red for ever --
    /// which is the whole bug this window replaced.
    pub fn error_breakdown(&self) -> HashMap<String, Vec<(String, u64)>> {
        self.error_breakdown_at(now_min())
    }

    fn error_breakdown_at(&self, now: u64) -> HashMap<String, Vec<(String, u64)>> {
        let mut out: HashMap<String, Vec<(String, u64)>> = HashMap::new();
        for ((host, class), w) in self.errors.read().unwrap().iter() {
            let n = w.total(now);
            if n == 0 {
                continue;
            }
            out.entry(host.clone()).or_default().push((class.clone(), n));
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

    /// ⭐ The point of the whole change: an hour after it stopped failing, a
    /// tracker is not named at all -- so the row that reads this key set goes
    /// back to green WITHOUT a restart. Until 2026-09-17 this count was kept
    /// for the life of the process and only a restart could clear it.
    #[test]
    fn a_tracker_that_stopped_failing_leaves_the_breakdown() {
        let c = Cache::default();
        for _ in 0..40 {
            c.count_failed_kind_at("tracker.example", "timeout", 100);
        }
        assert_eq!(
            c.error_breakdown_at(100).get("tracker.example").unwrap()[0],
            ("timeout".to_string(), 40)
        );
        // 59 minutes on: the bucket is the oldest one still inside the window.
        assert_eq!(
            c.error_breakdown_at(159).get("tracker.example").unwrap()[0].1,
            40,
            "the window is inclusive of its oldest minute"
        );
        // One more minute and it has aged out entirely.
        assert!(
            c.error_breakdown_at(160).get("tracker.example").is_none(),
            "an hour later the host is absent, not present with zero"
        );
    }

    /// Failures spread across minutes add up while they share the window, and
    /// only the part that aged out is lost.
    #[test]
    fn the_window_forgets_only_what_fell_out_of_it() {
        let c = Cache::default();
        c.count_failed_kind_at("t", "connect", 10);
        c.count_failed_kind_at("t", "connect", 50);
        c.count_failed_kind_at("t", "connect", 69);
        assert_eq!(c.error_breakdown_at(69).get("t").unwrap()[0].1, 3);
        // At minute 70 the cutoff is 11, so the first one is gone and the
        // other two remain.
        assert_eq!(c.error_breakdown_at(70).get("t").unwrap()[0].1, 2);
    }

    /// The classes stay apart: "rate limited" and "banned" are different jobs
    /// for the operator, which is why they were split in the first place.
    #[test]
    fn classes_are_counted_separately_and_sorted_by_weight() {
        let c = Cache::default();
        c.count_failed_kind_at("t", "timeout", 5);
        for _ in 0..3 {
            c.count_failed_kind_at("t", "connect", 5);
        }
        let b = c.error_breakdown_at(5);
        let v = b.get("t").unwrap();
        assert_eq!(v[0], ("connect".to_string(), 3), "most frequent first");
        assert_eq!(v[1], ("timeout".to_string(), 1));
    }

    /// The lifetime `announces_failed` counter must NOT become a window: the
    /// bench sampler differences it to draw a rate, and a counter that resets
    /// would draw a negative spike.
    #[test]
    fn the_rate_counter_stays_monotonic_while_the_breakdown_expires() {
        let c = Cache::default();
        c.count_failed_kind_at("t", "timeout", 0);
        c.count_failed_kind_at("t", "timeout", 0);
        assert!(c.error_breakdown_at(500).is_empty(), "the breakdown forgets");
        assert_eq!(c.outcomes().1, 0, "counted by count_failed, not by the window");
        c.count_failed_kind("t", "timeout");
        assert_eq!(c.outcomes().1, 1, "the lifetime counter still only grows");
    }

    /// A host that never failed was never in the map, and a window that is
    /// pruned must not resurrect it.
    #[test]
    fn an_expired_host_stops_costing_an_entry() {
        let c = Cache::default();
        c.count_failed_kind_at("old", "timeout", 0);
        c.count_failed_kind_at("new", "timeout", 500);
        let b = c.error_breakdown_at(500);
        assert!(b.get("old").is_none());
        assert!(b.get("new").is_some());
        assert_eq!(c.errors.read().unwrap().len(), 1, "the dead entry is dropped, not kept at zero");
    }
}
