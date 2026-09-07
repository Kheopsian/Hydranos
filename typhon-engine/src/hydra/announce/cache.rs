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

#[derive(Default)]
pub struct Cache {
    entries: RwLock<HashMap<String, Entry>>,
}

impl Cache {
    pub fn record(&self, info_hash: &str, entry: Entry) {
        self.entries.write().unwrap().insert(info_hash.to_string(), entry);
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
        self.entries.write().unwrap().remove(info_hash);
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
