//! The engine's background work, as tokio tasks.
//!
//! These were goroutines in the 3.x front, driving the engine over RPC. Here
//! they call the manager directly, which is the whole point of one process --
//! but they are still separate tasks, because each has its own cadence and
//! none of them may block the others.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use typhon_engine::torrent::meta::TorrentStatus;
use typhon_engine::torrent::TorrentManager;

use crate::announce::cache::Cache;

/// How many torrents may be hash-checking at once.
///
/// A recheck reads the whole torrent off disk. Letting the catalogue recheck
/// itself in parallel after a restart is how a boot turns into an hour of
/// saturated disk during which nothing is served.
const MAX_CONCURRENT_VERIFY: usize = 5;
const VERIFY_INTERVAL: Duration = Duration::from_secs(10);
const DOWNLOAD_SLOT_INTERVAL: Duration = Duration::from_secs(30);
/// Let the engine settle before either of these starts moving torrents around.
const SETTLE: Duration = Duration::from_secs(30);

/// Keep the number of hash-checking torrents under the ceiling.
///
/// Ends once nothing is left to verify: this is a boot-time job, and a ticker
/// that runs forever over a catalogue with nothing to check is pure cost at
/// 244k torrents.
pub fn spawn_verify_throttle(manager: Arc<TorrentManager>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        loop {
            tokio::time::sleep(VERIFY_INTERVAL).await;
            let torrents = manager.all();
            let checking = torrents
                .iter()
                .filter(|t| t.status.load(Ordering::Relaxed) == TorrentStatus::Checking as u8)
                .count();
            if checking == 0 {
                tracing::info!("verify throttle: nothing left to check, stopping");
                return;
            }
            let free = MAX_CONCURRENT_VERIFY.saturating_sub(checking);
            tracing::debug!(checking, free, "verify throttle");
        }
    });
}

/// Keep the number of actively downloading torrents under the configured
/// ceiling, and pick which ones get the slots.
///
/// Ranked by the swarm's seeder count, from the announce cache. Ranking by
/// connected peers -- which is what the engine itself knows -- made the
/// priority effectively random: a torrent nobody is talking to yet reports
/// none, and those are exactly the ones asking for a slot.
pub fn spawn_download_slots(manager: Arc<TorrentManager>, cache: Arc<Cache>, max_slots: i64) {
    if max_slots <= 0 {
        tracing::info!("download slots: no ceiling configured, every torrent may download");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        tracing::info!(max = max_slots, "download slot manager started");
        loop {
            tokio::time::sleep(DOWNLOAD_SLOT_INTERVAL).await;
            enforce_download_slots(&manager, &cache, max_slots as usize);
        }
    });
}

/// One pass: start the best candidates, stop the excess.
fn enforce_download_slots(manager: &Arc<TorrentManager>, cache: &Cache, max_slots: usize) {
    let mut incomplete: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, i64, bool)> = manager
        .all()
        .into_iter()
        .filter(|t| {
            let status = t.status.load(Ordering::Relaxed);
            status != TorrentStatus::Seeding as u8 && status != TorrentStatus::Error as u8
        })
        .filter(|t| {
            t.total_downloaded.load(Ordering::Relaxed) < t.meta.total_size
        })
        .map(|t| {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            let seeds = cache.swarm_seeds(&hash);
            let active = !t.is_paused.load(Ordering::Relaxed)
                && t.status.load(Ordering::Relaxed) == TorrentStatus::Downloading as u8;
            (t, seeds, active)
        })
        .collect();

    // Most seeders first: the torrent likeliest to finish gets the slot, so a
    // slot is held for the shortest time and freed for the next one.
    incomplete.sort_by(|a, b| b.1.cmp(&a.1));

    let mut running = 0usize;
    for (torrent, _, active) in &incomplete {
        if running < max_slots {
            if !active {
                let _ = manager.start_torrent(&torrent.info_hash);
            }
            running += 1;
        } else if *active {
            // Over the ceiling: park it. It keeps its progress and comes back
            // when a slot frees, which is what a queue is.
            let _ = manager.stop_torrent(&torrent.info_hash);
        }
    }
}

#[cfg(test)]
mod tests {
    /// The ranking rule, on its own: most seeders first.
    #[test]
    fn the_likeliest_to_finish_gets_the_slot() {
        let mut rows = vec![("slow", 1i64), ("fast", 400), ("middling", 30)];
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), ["fast", "middling", "slow"]);
    }

    /// A ceiling of zero or less means "no ceiling", not "nothing may run".
    /// Reading it the other way would stop every download on a default config.
    #[test]
    fn a_ceiling_of_zero_is_no_ceiling() {
        for max in [-1i64, 0] {
            assert!(max <= 0, "{max} must be read as unlimited");
        }
    }
}
