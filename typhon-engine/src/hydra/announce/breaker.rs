//! Sparing a tracker that has stopped answering.
//!
//! Announces are per torrent, so a tracker that is down does not cost one
//! failed announce per pass: it costs one per pass *per torrent that lists it*.
//! Each of those waits out its own timeout, and the workers walk a fixed
//! budget, so one dead host can eat the whole budget and starve the trackers
//! that are actually up.
//!
//! Keyed by host and not by tracker URL: the passkey differs between torrents
//! on the same tracker, the outage does not.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Consecutive failures before a host is set aside.
const FAIL_THRESHOLD: u32 = 5;
/// How long it is left alone once it trips.
const COOLDOWN: Duration = Duration::from_secs(10 * 60);

#[derive(Default)]
struct State {
    fails: u32,
    until: Option<Instant>,
}

#[derive(Default)]
pub struct Breaker {
    hosts: Mutex<HashMap<String, State>>,
}

impl Breaker {
    /// Whether an announce to this host may go out now.
    pub fn allows(&self, host: &str, now: Instant) -> bool {
        if host.is_empty() {
            return true;
        }
        let hosts = self.hosts.lock().unwrap();
        match hosts.get(host).and_then(|s| s.until) {
            Some(until) => now >= until,
            None => true,
        }
    }

    /// Fold one announce outcome in.
    ///
    /// A success clears the host outright: this exists to spare a dead
    /// tracker, not to hold an old hiccup against one that recovered.
    pub fn record(&self, host: &str, ok: bool, now: Instant) {
        if host.is_empty() {
            return;
        }
        let mut hosts = self.hosts.lock().unwrap();
        if ok {
            hosts.remove(host);
            return;
        }
        let state = hosts.entry(host.to_string()).or_default();
        state.fails += 1;
        let expired = state.until.map(|u| now > u).unwrap_or(true);
        if state.fails >= FAIL_THRESHOLD && expired {
            state.until = Some(now + COOLDOWN);
            state.fails = 0;
            tracing::warn!(
                host,
                fails = FAIL_THRESHOLD,
                cooldown_s = COOLDOWN.as_secs(),
                "tracker stopped answering, pausing announces to it"
            );
        }
    }

    /// The hosts currently set aside, for the trackers tab.
    pub fn tripped(&self, now: Instant) -> Vec<String> {
        let hosts = self.hosts.lock().unwrap();
        let mut out: Vec<String> = hosts
            .iter()
            .filter(|(_, s)| s.until.map(|u| now < u).unwrap_or(false))
            .map(|(h, _)| h.clone())
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_spared_only_after_repeated_failures() {
        let b = Breaker::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD - 1 {
            b.record("dead.example", false, now);
            assert!(b.allows("dead.example", now), "one bad announce is not an outage");
        }
        b.record("dead.example", false, now);
        assert!(!b.allows("dead.example", now), "it stopped answering, stop asking");
        assert_eq!(b.tripped(now), ["dead.example"]);
    }

    #[test]
    fn the_cooldown_ends_by_itself() {
        let b = Breaker::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD {
            b.record("dead.example", false, now);
        }
        assert!(!b.allows("dead.example", now));
        let later = now + COOLDOWN + Duration::from_secs(1);
        assert!(b.allows("dead.example", later), "a tracker is given another chance");
        assert!(b.tripped(later).is_empty());
    }

    /// A success wipes the slate. Counting failures forever would trip a
    /// healthy tracker on an unlucky afternoon spread over weeks.
    #[test]
    fn one_success_clears_the_record() {
        let b = Breaker::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD - 1 {
            b.record("flaky.example", false, now);
        }
        b.record("flaky.example", true, now);
        for _ in 0..FAIL_THRESHOLD - 1 {
            b.record("flaky.example", false, now);
        }
        assert!(b.allows("flaky.example", now), "the count restarted from zero");
    }

    #[test]
    fn an_empty_host_is_never_blocked() {
        let b = Breaker::default();
        let now = Instant::now();
        for _ in 0..FAIL_THRESHOLD * 3 {
            b.record("", false, now);
        }
        assert!(b.allows("", now), "a URL we could not read a host from is not a host");
    }
}
