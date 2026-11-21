//! When a reachable seed asks for peers anyway.
//!
//! A complete torrent the self-check found reachable asks for no peers: the
//! leechers open the connection. In a large swarm they often do not -- the
//! tracker's list a leecher got may not hold us, a leecher at its own
//! connection limit dials nobody new, and on a racing swarm the seeds that
//! connect first get the requests. The seed then sits with nobody connected
//! while the tracker says people are downloading.
//!
//! So when the tracker's last count shows leechers and we have few or none of
//! them, the next announce asks for a short list and the dial queue reaches
//! out. A hoard can hold a quarter of a million such torrents, and asking on
//! every one of them is the connection storm passive seeding was made to
//! avoid, so the asks share one budget per engine.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How many peers a starved seed asks for. A handful of leechers is what it
/// needs to start uploading; the tracker's 200 would be a leecher's list.
pub const REACH_NUMWANT: u32 = 30;

/// Below this many connected peers a seed counts as starved.
const FEW_PEERS: usize = 3;

/// Peers all of an engine's seeds may ask for in one [`BUDGET_WINDOW`].
///
/// Twenty asks a minute: 1 200 torrents an hour get a list, and the dial
/// queue (4 096 deep) never sees more than a fraction of its depth from this.
const BUDGET_PEERS: u32 = 20 * REACH_NUMWANT;
const BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// The tracker counts leechers and we are connected to few or none of them.
///
/// "Few" is also "fewer than there are": one leecher and one peer connected is
/// most likely that leecher already.
pub fn starved(leechers: u32, connected: usize) -> bool {
    leechers > 0 && connected < FEW_PEERS && connected < leechers as usize
}

/// How many peers a reachable seed asks for: [`REACH_NUMWANT`] when it is
/// [`starved`] and the engine's budget still has room, nothing otherwise.
pub fn numwant(leechers: u32, connected: usize, budget: &Budget, now: Instant) -> Option<u32> {
    (starved(leechers, connected) && budget.take(REACH_NUMWANT, now)).then_some(REACH_NUMWANT)
}

/// The engine-wide allowance for [`numwant`]: [`BUDGET_PEERS`] per
/// [`BUDGET_WINDOW`], in fixed windows.
#[derive(Default)]
pub struct Budget {
    /// When the current window opened, and how much of it is spent.
    window: Mutex<Option<(Instant, u32)>>,
}

impl Budget {
    /// Spends `n` if the current window still has it.
    fn take(&self, n: u32, now: Instant) -> bool {
        let mut w = self.window.lock().unwrap_or_else(|e| e.into_inner());
        let (opened, spent) = match *w {
            Some((opened, spent)) if now.saturating_duration_since(opened) < BUDGET_WINDOW => {
                (opened, spent)
            }
            _ => (now, 0),
        };
        let after = spent.saturating_add(n);
        let fits = after <= BUDGET_PEERS;
        *w = Some((opened, if fits { after } else { spent }));
        fits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case the issue is about: the tracker lists leechers, nobody is
    /// connected, so the seed asks.
    #[test]
    fn a_seed_with_leechers_and_no_peers_asks_for_some() {
        let b = Budget::default();
        assert_eq!(numwant(12, 0, &b, Instant::now()), Some(REACH_NUMWANT));
    }

    /// Nobody downloading: nothing to dial, so a reachable seed stays passive.
    #[test]
    fn a_seed_without_leechers_asks_for_nothing() {
        assert!(!starved(0, 0));
        assert_eq!(numwant(0, 0, &Budget::default(), Instant::now()), None);
    }

    /// Already serving the swarm: the leechers found us, nothing to fix.
    #[test]
    fn a_seed_already_connected_asks_for_nothing() {
        assert!(!starved(50, FEW_PEERS));
        assert!(
            !starved(1, 1),
            "the one leecher is most likely the one peer"
        );
        assert!(starved(50, FEW_PEERS - 1));
    }

    /// ⭐ The hoard guard: once an engine's seeds have asked for the window's
    /// worth, the rest stay passive until the next window, however many are
    /// starved.
    #[test]
    fn the_engine_budget_caps_asks_per_window_and_refills() {
        let b = Budget::default();
        let t0 = Instant::now();
        let asks = (BUDGET_PEERS / REACH_NUMWANT) as usize;
        for _ in 0..asks {
            assert_eq!(numwant(12, 0, &b, t0), Some(REACH_NUMWANT));
        }
        assert_eq!(numwant(12, 0, &b, t0), None, "the window is spent");
        assert_eq!(
            numwant(12, 0, &b, t0 + BUDGET_WINDOW - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            numwant(12, 0, &b, t0 + BUDGET_WINDOW),
            Some(REACH_NUMWANT),
            "a new window, a new allowance"
        );
    }

    /// A seed that does not ask spends nothing: the budget is for asks.
    #[test]
    fn a_seed_that_does_not_ask_spends_no_budget() {
        let b = Budget::default();
        let t0 = Instant::now();
        for _ in 0..1000 {
            assert_eq!(numwant(0, 0, &b, t0), None);
        }
        assert_eq!(numwant(12, 0, &b, t0), Some(REACH_NUMWANT));
    }
}
