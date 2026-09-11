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
pub fn spawn_download_slots(
    manager: Arc<TorrentManager>,
    cache: Arc<Cache>,
    max_slots: i64,
    store: Arc<std::sync::Mutex<crate::store::Store>>,
    engine_id: String,
) {
    if max_slots <= 0 {
        tracing::info!("download slots: no ceiling configured, every torrent may download");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        tracing::info!(max = max_slots, "download slot manager started");
        loop {
            tokio::time::sleep(DOWNLOAD_SLOT_INTERVAL).await;
            // Read once per pass, not once per torrent: this is a ceiling of a
            // few dozen slots against a catalogue of hundreds of thousands.
            let paused: std::collections::HashSet<String> = match store.lock() {
                Ok(store) => store
                    .paused_hashes(&engine_id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                Err(_) => Default::default(),
            };
            enforce_download_slots(&manager, &cache, max_slots as usize, &paused);
        }
    });
}

/// One pass: start the best candidates, stop the excess.
///
/// A torrent the operator paused is not a candidate and not an excess: it is
/// invisible here. It frees its slot for something that will actually finish,
/// and comes back into the queue when the operator resumes it -- which is why
/// a resume can show "queued" before it shows "downloading".
///
/// Skipping it is not a nicety. This loop used to call `start_torrent` on any
/// incomplete torrent inside the ceiling without asking whose decision stopped
/// it, so it undid every manual pause within one interval, silently.
fn enforce_download_slots(
    manager: &Arc<TorrentManager>,
    cache: &Cache,
    max_slots: usize,
    user_paused: &std::collections::HashSet<String>,
) {
    let mut incomplete: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, i64, bool)> = manager
        .all()
        .into_iter()
        .filter(|t| {
            let status = t.status.load(Ordering::Relaxed);
            status != TorrentStatus::Seeding as u8 && status != TorrentStatus::Error as u8
        })
        .filter(|t| {
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            !user_paused.contains(&hash)
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

/// Re-run the health invariants on a timer and keep the last report.
///
/// The scan walks both catalogues and stats the ghosts, so it is not free: it
/// runs every five minutes, as 3.x did, and the route serves whatever the last
/// pass found rather than scanning on request. A panel refresh must not be
/// able to walk 244k torrents.
pub fn spawn_health_scan(
    engines: Arc<crate::engines::EngineHost>,
    last: Arc<std::sync::RwLock<Option<crate::health::Report>>>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            tick.tick().await;
            let mut report = crate::health::Report::default();
            let mut per_engine = Vec::new();
            for engine in engines.engines() {
                let torrents = engine.manager.all();
                let cache = engine.announce_cache.clone();
                crate::health::scan_engine(
                    &engine.id,
                    &torrents,
                    |hash| cache.swarm_seeds(hash),
                    // Outage is a host-level fact and the breaker owns it; the
                    // scan does not second-guess it from here.
                    |_host| false,
                    &mut report,
                );
                per_engine.push((engine.id.clone(), torrents));
            }
            crate::health::scan_dual_seed(&per_engine, &mut report);
            let found = report.anomalies.len();
            *last.write().unwrap() = Some(report);
            if found > 0 {
                tracing::info!(anomalies = found, "health scan found something");
            }
        }
    });
}

/// Free space on the race disk by removing what has earned its keep.
///
/// Destructive by design and gated twice: it does nothing unless the operator
/// enabled it, and nothing until usage is over the high watermark. It then
/// removes only down to the low watermark -- the gap between the two is what
/// stops it running again on the next tick.
pub fn spawn_race_drain(
    manager: Arc<TorrentManager>,
    config: crate::config::RaceDrain,
    race_path: std::path::PathBuf,
) {
    if !config.enabled {
        tracing::info!("race drain: disabled");
        return;
    }
    let interval = if config.check_interval_seconds > 0 {
        Duration::from_secs(config.check_interval_seconds as u64)
    } else {
        Duration::from_secs(300)
    };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        tracing::info!(
            check_interval_s = interval.as_secs(),
            high = config.high_watermark_pct,
            low = config.low_watermark_pct,
            "race drain started"
        );
        loop {
            tokio::time::sleep(interval).await;
            drain_once(&manager, &config, &race_path);
        }
    });
}

fn drain_once(
    manager: &Arc<TorrentManager>,
    config: &crate::config::RaceDrain,
    race_path: &std::path::Path,
) {
    let Some((used, total)) = disk_usage(race_path) else {
        return;
    };
    if total == 0 {
        return;
    }
    let pct = used as f64 * 100.0 / total as f64;
    if pct < config.high_watermark_pct as f64 {
        return;
    }
    let target = total as f64 * config.low_watermark_pct as f64 / 100.0;
    let mut to_free = used as f64 - target;
    tracing::warn!(pct = pct.round(), high = config.high_watermark_pct, "race disk over the high watermark, draining");

    // Oldest first: a race that has been sitting the longest has had the most
    // time to earn its ratio, so it is the cheapest to let go.
    let mut torrents = manager.all();
    torrents.sort_by_key(|t| t.added_time);

    for torrent in torrents {
        if to_free <= 0.0 {
            break;
        }
        let size = torrent.meta.total_size as f64;
        // ⚠ keep_data, NOT delete_files. The Go signature at this position is
        // `deleteFiles` and passes true; this one is its opposite. Passing true
        // here would drop the torrent from the engine and leave every byte on
        // disk -- freeing nothing, so the next tick drains again, and the race
        // catalogue disappears without the disk ever emptying.
        if manager.remove_torrent(&torrent.info_hash, false).is_ok() {
            to_free -= size;
            tracing::info!(name = %torrent.meta.name, "drained");
        }
    }
}

/// Bytes used and total on the filesystem holding `path`.
fn disk_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let block = stat.f_frsize as u64;
    let total = stat.f_blocks as u64 * block;
    // Used is what the filesystem counts as taken, not total minus free: the
    // reserved blocks are neither available nor used by us, and counting them
    // as used would trigger a drain on a disk that is not full.
    let used = (stat.f_blocks as u64 - stat.f_bfree as u64) * block;
    Some((used, total))
}

/// Watch our own memory and say so before the kernel does.
///
/// 3.x watched the engine *process* -- was it alive, how much had it taken --
/// because the engine was a separate process it had spawned. Half of that
/// disappears here: there is no other process to find dead. What remains is
/// the ceiling, and it still matters, because the way this ends otherwise is
/// the OOM killer taking the whole thing with no warning and no dump.
pub fn spawn_memory_watch(limit_bytes: u64) {
    if limit_bytes == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut over = false;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let Some(rss) = resident_bytes() else { continue };
            if rss > limit_bytes && !over {
                // Edge-triggered: a process sitting over the line for an hour
                // is one problem, not one hundred and twenty alerts.
                over = true;
                tracing::error!(
                    rss_mib = rss / (1 << 20),
                    limit_mib = limit_bytes / (1 << 20),
                    "resident memory over the configured ceiling"
                );
            } else if rss <= limit_bytes && over {
                over = false;
                tracing::info!(rss_mib = rss / (1 << 20), "resident memory back under the ceiling");
            }
        }
    });
}

/// Resident set size of this process, in bytes.
///
/// From statm, whose second field is the resident page count. Not from
/// `VmRSS` in status: same number, more parsing.
fn resident_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    Some(pages * page_size)
}

#[cfg(test)]
mod memory_tests {
    #[test]
    fn our_own_resident_size_is_readable_and_not_absurd() {
        let rss = super::resident_bytes().expect("/proc/self/statm is readable on Linux");
        // A running test process holds more than a page and less than a
        // terabyte. The point is that the page-size multiplication happened:
        // forgetting it reports pages as bytes and never alerts.
        assert!(rss > 4096, "{rss} bytes looks like a page count, not bytes");
        assert!(rss < (1 << 40));
    }
}

/// Bring every torrent up at boot, in batches.
///
/// Starting one is nearly free -- two atomic stores, no verify -- so the box
/// serves inbound peers straight away: they find us through the tracker
/// announce made before the restart, still valid for about half an hour. The
/// announce ramp is paced separately by the scheduler and trails behind
/// without holding seeding back.
///
/// Batched anyway, because 244k starts in one pass is a single burst of work
/// on the runtime that starves everything else, including the HTTP handler
/// that would tell an operator what is happening.
pub fn spawn_stagger_start(manager: Arc<TorrentManager>) {
    const BATCH: usize = 2000;
    const PAUSE: Duration = Duration::from_millis(100);
    tokio::spawn(async move {
        let torrents = manager.all();
        let total = torrents.len();
        if total == 0 {
            return;
        }
        let mut started = 0usize;
        for (i, torrent) in torrents.iter().enumerate() {
            // A torrent the operator stopped stays stopped. Starting it here
            // would undo an intent every restart.
            if torrent.is_paused.load(Ordering::Relaxed) {
                continue;
            }
            if manager.start_torrent(&torrent.info_hash).is_ok() {
                started += 1;
            }
            if (i + 1) % BATCH == 0 && i + 1 < total {
                tracing::info!(started, total, pct = started * 100 / total, "stagger start");
                tokio::time::sleep(PAUSE).await;
            }
        }
        tracing::info!(started, total, "stagger start done");
    });
}

/// Drop the store rows whose torrent no longer exists.
///
/// A row outlives its torrent whenever a removal is interrupted -- a crash
/// between "the engine forgot it" and "the store forgot it" leaves one behind.
/// One is nothing; years of them are a table that answers questions about
/// torrents nobody holds, and counts that do not match the engine's.
///
/// Reconciled rather than deleted on the spot, because the engine is the
/// authority on what exists and the store is not: comparing the two is the
/// only way to tell an orphan from a torrent that is merely paused.
/// The store rows one reconcile pass may delete, or `Err(n)` when there are so
/// many that the pass refuses to act.
///
/// Two things are NOT orphans even though the engine does not hold them:
///
///   * a record the loader refused -- its .torrent on disk is a different
///     torrent, so the store row is the last copy of its metainfo and the next
///     start can repair from it;
///   * everything, when there is a lot of it. A partial load looks exactly like
///     a mass deletion from here and this worker cannot tell the two apart, so
///     past a ceiling it refuses rather than acting on a reading it cannot
///     verify. The caller's emptiness check only catches an engine that loaded
///     NOTHING; this catches the one that loaded almost everything.
fn rows_to_drop(
    known: Vec<String>,
    live: &std::collections::HashSet<String>,
    refused: &std::collections::HashSet<String>,
) -> Result<Vec<String>, usize> {
    let total = known.len();
    let doomed: Vec<String> = known
        .into_iter()
        .filter(|h| !live.contains(h) && !refused.contains(h))
        .collect();
    let ceiling = (total / 100).max(50);
    if doomed.len() > ceiling {
        return Err(doomed.len());
    }
    Ok(doomed)
}

#[cfg(test)]
mod reconcile_tests {
    use super::rows_to_drop;
    use std::collections::HashSet;

    fn set(v: &[&str]) -> HashSet<String> { v.iter().map(|s| s.to_string()).collect() }
    fn many(n: usize) -> Vec<String> { (0..n).map(|i| format!("h{i:06}")).collect() }

    /// A row whose torrent is simply gone is still collected.
    #[test]
    fn a_real_orphan_is_dropped() {
        let known = vec!["a".to_string(), "b".to_string()];
        let out = rows_to_drop(known, &set(&["a"]), &HashSet::new()).unwrap();
        assert_eq!(out, vec!["b".to_string()]);
    }

    /// The row of a refused record is the last copy of its metainfo. Deleting
    /// it is what turned a recoverable collision into a torrent lost for good.
    #[test]
    fn a_refused_record_keeps_its_row() {
        let known = vec!["a".to_string(), "b".to_string()];
        let out = rows_to_drop(known, &set(&["a"]), &set(&["b"])).unwrap();
        assert!(out.is_empty(), "the refused row must survive");
    }

    /// A library that failed to load must not be mistaken for one that was
    /// emptied on purpose.
    #[test]
    fn a_mass_deletion_is_refused() {
        match rows_to_drop(many(10_000), &set(&["h000000"]), &HashSet::new()) {
            Err(n) => assert_eq!(n, 9_999),
            Ok(_) => panic!("dropping 9999 of 10000 rows must be refused"),
        }
    }

    /// The ceiling is a share, not a constant: 1% of a big session still goes
    /// through, so ordinary churn is not blocked.
    #[test]
    fn ordinary_churn_still_goes_through() {
        let known = many(10_000);
        let live: HashSet<String> = known.iter().skip(60).cloned().collect();
        let out = rows_to_drop(known, &live, &HashSet::new()).unwrap();
        assert_eq!(out.len(), 60);
    }

    /// A small session has a floor, or removing two rows from a library of ten
    /// would trip the percentage.
    #[test]
    fn a_small_session_has_a_floor() {
        let out = rows_to_drop(many(10), &HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(out.len(), 10);
    }
}

pub fn spawn_store_reconcile(
    engines: Arc<crate::engines::EngineHost>,
    store: Arc<std::sync::Mutex<crate::store::Store>>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            tick.tick().await;
            // Per engine, because the store keys its rows by session: a
            // hoard row is not an orphan just because the race does not hold
            // that torrent.
            for engine in engines.engines() {
                let live: std::collections::HashSet<String> = engine
                    .manager
                    .all()
                    .iter()
                    .map(|t| t.info_hash.iter().map(|b| format!("{b:02x}")).collect())
                    .collect();
                // An engine that failed to load its resume data reports
                // nothing, and taking that at face value would empty its half
                // of the store. A reconcile with no live torrent is refused.
                if live.is_empty() {
                    tracing::warn!(engine = %engine.id, "store reconcile: no torrent, skipping");
                    continue;
                }
                let store = store.lock().unwrap();
                let known = match store.all_hashes(&engine.id) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(engine = %engine.id, error = %e, "store reconcile: cannot list");
                        continue;
                    }
                };
                // A record the loader REFUSED is not an orphan: its .torrent
                // on disk holds a different torrent, so the store row is the
                // last copy of its metainfo and the next start can repair from
                // it. Deleting it here is what turned a recoverable collision
                // into a torrent lost for good.
                let refused = engine.manager.refused_records();
                let total = known.len();
                let doomed = match rows_to_drop(known, &live, &refused) {
                    Ok(rows) => rows,
                    Err(would_drop) => {
                        tracing::warn!(
                            engine = %engine.id,
                            would_drop,
                            of = total,
                            "store reconcile: refusing to drop that many rows at once -- \
                             load them or repair them first"
                        );
                        continue;
                    }
                };

                let mut dropped = 0usize;
                for hash in doomed {
                    if store.delete_torrent(&hash).unwrap_or(false) {
                        dropped += 1;
                    }
                }
                if dropped > 0 {
                    tracing::info!(engine = %engine.id, dropped, "store reconcile: rows without a torrent removed");
                }
                if !refused.is_empty() {
                    tracing::info!(
                        engine = %engine.id,
                        kept = refused.len(),
                        "store reconcile: rows kept for records the loader refused"
                    );
                }
            }
        }
    });
}
