//! Turnover at the connection ceiling.
//!
//! At `max_connections` every inbound handshake used to be refused, and
//! nothing ever left to make room. On a hoard many of the live sessions are
//! long-lived peers that only trade Haves and never ask for a block, so a new
//! leecher that wants data lost its place to a peer that wanted nothing.
//!
//! Now, at the ceiling, one idle session is asked to close and the new peer
//! is admitted in its place. Idle means: the peer is not interested in us, we
//! are not interested in it, and it took no byte from us for [`IDLE_FOR`].
//! A torrent never loses a session this way once it is down to
//! [`FLOOR_PER_TORRENT`], so one busy swarm cannot drain every other torrent
//! of its peers and take the whole engine.
//!
//! The choice must not walk every session under a lock on each handshake:
//! a hoard holds 12k of them. Sessions are enlisted in the order they
//! connect, and a handshake at the ceiling looks at no more than [`SCAN`] of
//! them from the front. The first idle one goes; a busy one moves to the back
//! and is looked at again a full turn later. Oldest first, roughly: a session
//! that was busy once loses its place in the line.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::torrent::meta::{PeerStats, TorrentState};

/// How long a session must have taken nothing from us to count as idle.
pub const IDLE_FOR: Duration = Duration::from_secs(120);
/// Sessions a torrent keeps whatever the turnover wants.
pub const FLOOR_PER_TORRENT: usize = 2;
/// Sessions one handshake at the ceiling may look at, at most.
const SCAN: usize = 64;
/// Line length below which ended sessions are left for the front to drop.
const SWEEP_FROM: usize = 128;

/// The line, and when it is next swept of ended sessions.
struct Line {
    queue: VecDeque<Entry>,
    /// Length at which the next sweep runs: twice what the last one left,
    /// and never below [`SWEEP_FROM`].
    sweep_at: usize,
    /// Sweeps run so far.
    sweeps: u64,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            sweep_at: SWEEP_FROM,
            sweeps: 0,
        }
    }
}

/// An idle session that may go, and its BEP 40 rank (higher is worse).
type Candidate = (Arc<PeerStats>, Arc<TorrentState>, u32);

struct Entry {
    stats: Weak<PeerStats>,
    torrent: Weak<TorrentState>,
    /// `total_uploaded` when last looked at, and when that was. A session
    /// whose count has not moved since `seen` uploaded nothing in between.
    uploaded: u64,
    seen: Instant,
}

/// The sessions of one engine, oldest first, as eviction candidates.
#[derive(Default)]
pub struct Turnover {
    line: Mutex<Line>,
    /// Our address as peers see it, once the dial loop has learned it. Only
    /// used to rank eviction candidates (BEP 40).
    ours: Mutex<Option<std::net::SocketAddr>>,
}

impl Turnover {
    pub fn set_our_addr(&self, addr: Option<std::net::SocketAddr>) {
        if let Ok(mut g) = self.ours.lock() {
            *g = addr;
        }
    }

    /// Put a new session at the back of the line.
    ///
    /// Ended sessions are not taken out when they end: a `VecDeque` has no
    /// cheap removal from the middle. They are dropped when they reach the
    /// front, or in a sweep once the line reaches twice what the last sweep
    /// left (and at least [`SWEEP_FROM`]). It stays within about twice the
    /// sessions alive, and every sweep is paid for by at least as many
    /// enlistments as it keeps: a live count hovering at some size does not
    /// sweep the whole line on each connect.
    pub fn enlist(&self, torrent: &Arc<TorrentState>, stats: &Arc<PeerStats>, now: Instant) {
        let entry = Entry {
            stats: Arc::downgrade(stats),
            torrent: Arc::downgrade(torrent),
            uploaded: stats.total_uploaded.load(Ordering::Relaxed),
            seen: now,
        };
        let Ok(mut line) = self.line.lock() else {
            return;
        };
        line.queue.push_back(entry);
        if line.queue.len() >= line.sweep_at {
            line.queue.retain(|e| {
                e.stats
                    .upgrade()
                    .is_some_and(|s| !s.closing.load(Ordering::Relaxed))
            });
            line.sweep_at = line.queue.len().saturating_mul(2).max(SWEEP_FROM);
            line.sweeps += 1;
        }
    }

    /// Ask one idle session to close. False when none of the sessions looked
    /// at may go, and the new peer has to be refused.
    #[must_use]
    pub fn evict_one(&self, now: Instant) -> bool {
        let ours = self.ours.lock().ok().and_then(|g| *g);
        self.evict_ranked(now, ours)
    }

    fn evict_ranked(&self, now: Instant, ours: Option<std::net::SocketAddr>) -> bool {
        let Ok(mut line) = self.line.lock() else {
            return false;
        };
        // Among the idle sessions looked at, the one BEP 40 ranks lowest goes
        // (the highest number: see `peer::priority`), so the peers a swarm
        // would pick for us are the ones that stay. Priority is measured from
        // our own address; when nothing has learned it yet, the first idle
        // one goes, as before.
        let mut scanned: Vec<(Entry, Option<Candidate>)> = Vec::new();
        for _ in 0..SCAN.min(line.queue.len()) {
            let Some(mut e) = line.queue.pop_front() else {
                break;
            };
            let (Some(stats), Some(torrent)) = (e.stats.upgrade(), e.torrent.upgrade()) else {
                continue;
            };
            if stats.closing.load(Ordering::Relaxed) {
                continue;
            }
            let uploaded = stats.total_uploaded.load(Ordering::Relaxed);
            if uploaded != e.uploaded {
                e.uploaded = uploaded;
                e.seen = now;
            }
            let idle = !stats.interested.load(Ordering::Relaxed)
                && !stats.am_interested.load(Ordering::Relaxed)
                && now.saturating_duration_since(e.seen) >= IDLE_FOR;
            // Evicted sessions already left this count (`PeerStats::evict`).
            let staying = torrent.peers_connected.load(Ordering::Relaxed);
            if !(idle && staying > FLOOR_PER_TORRENT) {
                scanned.push((e, None));
                continue;
            }
            let Some(ours) = ours else {
                if stats.evict(&torrent) {
                    line.queue.extend(scanned.into_iter().map(|(e, _)| e));
                    return true;
                }
                scanned.push((e, None));
                continue;
            };
            let rank = crate::peer::priority::peer_priority(ours, stats.addr);
            scanned.push((e, Some((stats, torrent, rank))));
        }
        // Worst first.
        let mut order: Vec<usize> = (0..scanned.len())
            .filter(|&i| scanned[i].1.is_some())
            .collect();
        order.sort_by_key(|&i| std::cmp::Reverse(scanned[i].1.as_ref().map(|c| c.2)));
        let mut evicted = None;
        for i in order {
            if let Some((stats, torrent, _)) = &scanned[i].1 {
                if stats.evict(torrent) {
                    evicted = Some(i);
                    break;
                }
            }
        }
        for (i, (e, _)) in scanned.into_iter().enumerate() {
            if Some(i) != evicted {
                line.queue.push_back(e);
            }
        }
        evicted.is_some()
    }

    /// Sessions in the line, ended ones included.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.line.lock().map(|l| l.queue.len()).unwrap_or(0)
    }

    /// Sweeps of ended sessions run so far.
    #[cfg(test)]
    pub(crate) fn sweeps(&self) -> u64 {
        self.line.lock().map(|l| l.sweeps).unwrap_or(0)
    }

    /// Move every session's last look `by` into the past, as if that long
    /// had gone by with no upload.
    #[cfg(test)]
    pub(crate) fn age(&self, by: Duration) {
        if let Ok(mut line) = self.line.lock() {
            for e in line.queue.iter_mut() {
                e.seen = e.seen.checked_sub(by).unwrap_or(e.seen);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::meta::{TorrentMeta, TorrentState};
    use std::path::PathBuf;

    fn torrent() -> Arc<TorrentState> {
        let meta = TorrentMeta {
            info_hash: [1u8; 20],
            name: "t".into(),
            num_pieces: 1,
            piece_length: 16384,
            total_size: 16384,
            files: Vec::new(),
            trackers: Vec::new(),
            url_list: Vec::new(),
            private: false,
            multi_file: false,
            info_dict_len: 0,
            v2: false,
            renames: Default::default(),
        };
        Arc::new(TorrentState::new(meta, PathBuf::from("/tmp"), true))
    }

    /// A session of `t`, counted on it the way its guard counts it.
    fn session(t: &Arc<TorrentState>, n: u8) -> Arc<PeerStats> {
        let addr: std::net::SocketAddr = format!("93.184.216.{n}:6881").parse().unwrap();
        t.peers_connected.fetch_add(1, Ordering::Relaxed);
        Arc::new(PeerStats::new(addr, [n; 20], "test".into(), false, false))
    }

    /// `n` sessions of a fresh torrent, enlisted at `at`, oldest first.
    fn swarm(line: &Turnover, n: u8, at: Instant) -> (Arc<TorrentState>, Vec<Arc<PeerStats>>) {
        let t = torrent();
        let s: Vec<_> = (1..=n).map(|i| session(&t, i)).collect();
        for p in &s {
            line.enlist(&t, p, at);
        }
        (t, s)
    }

    fn closing(s: &[Arc<PeerStats>]) -> Vec<bool> {
        s.iter()
            .map(|p| p.closing.load(Ordering::Relaxed))
            .collect()
    }

    #[test]
    fn the_oldest_idle_session_is_the_one_asked_to_close() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (t, s) = swarm(&line, 4, t0);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&s), [true, false, false, false]);
        assert_eq!(t.peers_connected.load(Ordering::Relaxed), 3, "counted out");
    }

    #[test]
    fn an_interested_peer_is_kept() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, 4, t0);
        s[0].interested.store(true, Ordering::Relaxed);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&s), [false, true, false, false]);
    }

    #[test]
    fn a_peer_we_want_pieces_from_is_kept() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, 4, t0);
        s[0].am_interested.store(true, Ordering::Relaxed);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&s), [false, true, false, false]);
    }

    /// Uploading even once restarts the idle clock from when it was seen.
    #[test]
    fn a_session_that_uploaded_since_last_look_is_kept() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, 4, t0);
        s[0].total_uploaded.store(16384, Ordering::Relaxed);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&s), [false, true, false, false]);
        // Nothing more for a full idle period after it was seen: now it may go.
        s[2].interested.store(true, Ordering::Relaxed);
        s[3].interested.store(true, Ordering::Relaxed);
        assert!(!line.evict_one(t0 + IDLE_FOR * 2 - Duration::from_secs(1)));
        assert!(line.evict_one(t0 + IDLE_FOR * 2));
        assert_eq!(closing(&s), [true, true, false, false]);
    }

    #[test]
    fn a_session_younger_than_the_idle_period_is_kept() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, 4, t0);

        assert!(!line.evict_one(t0 + IDLE_FOR - Duration::from_secs(1)));
        assert_eq!(closing(&s), [false; 4]);
    }

    /// A torrent at its floor keeps its sessions, counting those already
    /// asked to close as gone.
    #[test]
    fn a_torrent_is_never_taken_below_its_floor() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, FLOOR_PER_TORRENT as u8 + 1, t0);
        let late = t0 + IDLE_FOR;

        assert!(line.evict_one(late));
        assert!(!line.evict_one(late), "the floor was crossed");
        assert_eq!(
            closing(&s).iter().filter(|c| **c).count(),
            1,
            "only one went"
        );
    }

    /// The floor protects the small swarm; the big one still gives way.
    #[test]
    fn a_torrent_at_its_floor_is_passed_over_for_another() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_small, small) = swarm(&line, FLOOR_PER_TORRENT as u8, t0);
        let (_big, big) = swarm(&line, 4, t0);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&small), [false; FLOOR_PER_TORRENT]);
        assert_eq!(closing(&big), [true, false, false, false]);
    }

    #[test]
    fn a_session_is_never_evicted_twice() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (t, s) = swarm(&line, 4, t0);
        // Already on its way out, by an eviction or its own end.
        s[0].closing.store(true, Ordering::Relaxed);

        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&s), [true, true, false, false]);
        assert_eq!(
            t.peers_connected.load(Ordering::Relaxed),
            3,
            "counted out once"
        );
    }

    #[test]
    fn ended_sessions_are_swept_out_of_the_line() {
        let line = Turnover::default();
        let t = torrent();
        let now = Instant::now();
        for i in 0..1000u32 {
            let s = session(&t, (i % 250) as u8);
            line.enlist(&t, &s, now);
            // `s` drops here: the session ended.
        }
        assert!(
            line.len() < SWEEP_FROM,
            "the line kept {} ended sessions",
            line.len()
        );
    }

    /// A live count that hovers -- one session ends, another connects -- must
    /// not sweep the whole line on every connect. It did when the sweep ran at
    /// each power of two: 1023 alive put the line at 1024 after each connect,
    /// the sweep took it back to 1023, and the next connect swept again.
    #[test]
    fn a_hovering_live_count_does_not_sweep_on_every_connect() {
        let line = Turnover::default();
        let t = torrent();
        let now = Instant::now();
        let mut alive: std::collections::VecDeque<_> = (0..1023u32)
            .map(|i| {
                let s = session(&t, (i % 250) as u8);
                line.enlist(&t, &s, now);
                s
            })
            .collect();
        let before = line.sweeps();

        for i in 0..1000u32 {
            alive.pop_front(); // one session ends
            let s = session(&t, (i % 250) as u8);
            line.enlist(&t, &s, now);
            alive.push_back(s);
        }

        let swept = line.sweeps() - before;
        assert!(swept <= 1, "{swept} sweeps for 1000 connects");
        assert!(line.len() <= 2 * 1023, "the line grew to {}", line.len());
    }

    /// The look is bounded: past `SCAN` busy sessions nobody else is tried.
    #[test]
    fn one_handshake_looks_at_no_more_than_scan_sessions() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_busy, busy) = swarm(&line, SCAN as u8, t0);
        for p in &busy {
            p.interested.store(true, Ordering::Relaxed);
        }
        let (_idle, idle) = swarm(&line, 4, t0);

        assert!(!line.evict_one(t0 + IDLE_FOR), "it looked past SCAN");
        assert_eq!(closing(&idle), [false; 4]);
        // The busy ones went to the back: the next handshake reaches the idle.
        assert!(line.evict_one(t0 + IDLE_FOR));
        assert_eq!(closing(&idle), [true, false, false, false]);
    }

    /// With our address known, the idle session BEP 40 ranks lowest goes, not
    /// the oldest.
    #[test]
    fn the_lowest_priority_idle_session_goes_first() {
        let line = Turnover::default();
        let t0 = Instant::now();
        let (_t, s) = swarm(&line, 4, t0);
        let ours: std::net::SocketAddr = "198.51.100.7:6881".parse().unwrap();
        let rank = |p: &Arc<PeerStats>| crate::peer::priority::peer_priority(ours, p.addr);
        let worst = (0..4).max_by_key(|&i| rank(&s[i])).unwrap();

        assert!(line.evict_ranked(t0 + IDLE_FOR, Some(ours)));
        let mut want = [false; 4];
        want[worst] = true;
        assert_eq!(closing(&s), want);

        // A busy one is never the pick, however low it ranks.
        let next = (0..4)
            .filter(|&i| i != worst)
            .max_by_key(|&i| rank(&s[i]))
            .unwrap();
        s[next].interested.store(true, Ordering::Relaxed);
        assert!(line.evict_ranked(t0 + IDLE_FOR, Some(ours)));
        assert!(!s[next].closing.load(Ordering::Relaxed));
    }
}
