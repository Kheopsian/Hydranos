//! Letting a returning client keep its place.
//!
//! A tab that comes back already holds the library. Re-streaming it costs
//! 187 MB and 300k rows for a client that was away four seconds -- the "it
//! takes about seven seconds to come back to life" report. Instead the client
//! sends the cursor it left on and gets only what changed.
//!
//! Events are timestamped by SERVER observation time, when the change crossed
//! this process, and never by the torrent's own `added_time`: that field is 0
//! for a freshly added torrent, which is exactly the case a delta has to catch.

use std::collections::VecDeque;
use std::sync::Mutex;

/// One observed change.
#[derive(Debug, Clone)]
struct Event {
    hash: String,
    at: f64,
    /// true = the row must be (re)sent, false = the torrent is gone.
    ///
    /// Not "was added": a row whose category or trackers changed is replayed
    /// as an add, because the delta carries full rows for added hashes and
    /// that is exactly the refresh a returning client needs.
    added: bool,
}

/// The window of changes this process can still speak for.
pub struct Ring {
    inner: Mutex<Inner>,
}

struct Inner {
    events: VecDeque<Event>,
    cap: usize,
    /// Earliest time from which the delta is still complete. Older than this
    /// and we no longer know what was missed.
    floor: f64,
}

impl Default for Ring {
    fn default() -> Self {
        // The floor starts at now: this process has no history from before it
        // started, so a cursor older than its boot is refused and the client
        // reloads. That is the honest answer, not an empty delta.
        Self::with_floor(now())
    }
}

impl Ring {
    fn with_floor(floor: f64) -> Self {
        Self { inner: Mutex::new(Inner { events: VecDeque::new(), cap: 16384, floor }) }
    }
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as f64)
        .unwrap_or(0.0)
}

impl Ring {
    pub fn record(&self, hash: &str, added: bool) {
        self.record_at(hash, added, now());
    }

    /// The same, with the observation time given rather than read.
    ///
    /// The clock is an input here, not a dependency: a test that has to sleep
    /// to observe an ordering is a test that fails on a busy machine.
    fn record_at(&self, hash: &str, added: bool, at: f64) {
        if hash.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        inner.events.push_back(Event { hash: hash.to_string(), at, added });
        while inner.events.len() > inner.cap {
            // Everything up to the dropped event is no longer guaranteed: a
            // client whose cursor predates it must be told to reload in full.
            if let Some(dropped) = inner.events.pop_front() {
                inner.floor = dropped.at;
            }
        }
    }

    /// A row changed without being added or removed: a category set, tags
    /// edited, a tracker added.
    pub fn record_changed(&self, hashes: &[String]) {
        for h in hashes {
            self.record(h, true);
        }
    }

    /// What changed after `since`, and whether the answer can be trusted.
    ///
    /// `None` means the cursor is older than the window: the honest answer is
    /// "reload everything", not a delta that silently omits what was dropped.
    pub fn changes_since(&self, since: f64) -> Option<(Vec<String>, Vec<String>)> {
        let inner = self.inner.lock().unwrap();
        if since < inner.floor {
            return None;
        }
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for e in inner.events.iter().filter(|e| e.at > since) {
            if e.added {
                added.push(e.hash.clone());
            } else {
                removed.push(e.hash.clone());
            }
        }
        Some((added, removed))
    }
}

/// Whether a delta may be served at all.
///
/// ⚠ The question is whether a DIALLED agent exists -- a node this process
/// reaches over the network, whose torrents its own hubs cannot speak for. A
/// torrent that arrived on one while the client was away would be missing from
/// the delta and stay invisible until a full reload.
///
/// This node's own engines are agents by name (`local-race`, `local-hoard`,
/// `local-race-2`, since one engine became one agent) and are NOT that case:
/// their rows are right here. Counting them as remote is what silently
/// disabled the delta on every multi-engine node -- the count was never zero
/// again, so every tab return re-streamed the whole library.
pub fn delta_allowed(agents: &[(String, bool)]) -> bool {
    agents.iter().all(|(_, is_local)| *is_local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_inside_the_window_gets_only_what_changed() {
        let r = Ring::with_floor(100.0);
        r.record_at("aaa", true, 110.0);
        r.record_at("bbb", false, 120.0);
        let (added, removed) = r.changes_since(105.0).expect("the window covers this cursor");
        assert_eq!(added, ["aaa"]);
        assert_eq!(removed, ["bbb"]);
    }

    /// A change and a reconnect inside the same second: the timestamps are
    /// whole seconds, as 3.x's are, so `at > since` drops it. Faithful, and
    /// worth knowing -- the client sees it on its next event, not never.
    #[test]
    fn the_window_is_exclusive_on_its_lower_bound() {
        let r = Ring::with_floor(100.0);
        r.record_at("aaa", true, 110.0);
        assert!(r.changes_since(110.0).unwrap().0.is_empty());
        assert_eq!(r.changes_since(109.0).unwrap().0, ["aaa"]);
    }

    #[test]
    fn a_cursor_older_than_the_window_is_refused_rather_than_answered_partially() {
        let r = Ring::with_floor(1_000.0);
        assert!(
            r.changes_since(999.0).is_none(),
            "a delta that omits what was dropped is worse than asking for a reload"
        );
        assert!(r.changes_since(1_000.0).is_some());
    }

    #[test]
    fn an_edited_row_is_replayed_as_an_add() {
        // The delta carries full rows for added hashes, so replaying an edit
        // that way is the refresh the client needs -- it has no other channel
        // for "this row changed".
        let r = Ring::with_floor(100.0);
        r.record_at("aaa", true, 110.0);
        let (added, removed) = r.changes_since(105.0).unwrap();
        assert_eq!(added, ["aaa"]);
        assert!(removed.is_empty());
    }

    /// ⭐ The bug 3.181.0 fixed. Since one engine became one agent, a
    /// multi-engine node always has agents registered, so a guard counting
    /// them never fired -- and every tab return re-streamed 187 MB.
    #[test]
    fn this_nodes_own_engines_do_not_disable_the_delta() {
        let local_only = vec![
            ("local-race".to_string(), true),
            ("local-hoard".to_string(), true),
            ("local-race-2".to_string(), true),
        ];
        assert!(delta_allowed(&local_only), "our own engines are not a reason to give up");

        let with_remote = vec![("local-race".to_string(), true), ("far-node".to_string(), false)];
        assert!(
            !delta_allowed(&with_remote),
            "a dialled agent holds rows this process cannot speak for"
        );
        assert!(delta_allowed(&[]), "no agent at all is not a reason either");
    }
}
