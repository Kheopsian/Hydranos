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
    spawn_verify_throttle_until(manager, typhon_engine::stop::global().clone());
}

/// `spawn_verify_throttle` with the stop signal given, so a test can end it
/// without raising the process-wide one.
fn spawn_verify_throttle_until(manager: Arc<TorrentManager>, stop: typhon_engine::stop::Stop) {
    tokio::spawn(async move {
        if !stop.sleep(Duration::from_secs(5)).await {
            return;
        }
        loop {
            if !stop.sleep(VERIFY_INTERVAL).await {
                return;
            }
            // Counted in place. `all()` copied the whole catalogue every ten
            // seconds for exactly as long as the boot lasts -- the window in
            // which the header was frozen.
            let checking = manager
                .count_where(|t| t.status.load(Ordering::Relaxed) == TorrentStatus::Checking as u8);
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
/// ceiling, and pick which ones get the slots -- and, with `queueing` on, the
/// seeds and the total under `active_seeds` / `active_limit`, qBittorrent's
/// queue.
///
/// Ranked by the swarm's seeder count, from the announce cache. Ranking by
/// connected peers -- which is what the engine itself knows -- made the
/// priority effectively random: a torrent nobody is talking to yet reports
/// none, and those are exactly the ones asking for a slot.
///
/// The limits are read from the live config on every pass, so a save in the
/// settings applies on the next one. A pass with nothing to enforce walks
/// nothing.
pub fn spawn_download_slots(
    manager: Arc<TorrentManager>,
    cache: Arc<Cache>,
    cfg_handle: Arc<std::sync::RwLock<Arc<crate::config::Config>>>,
    store: Arc<crate::store::StoreLock>,
    engine_id: String,
) {
    let limits_now = {
        let cfg_handle = cfg_handle.clone();
        let engine_id = engine_id.clone();
        move || -> QueueLimits {
            let cfg = match cfg_handle.read() {
                Ok(g) => g.clone(),
                Err(e) => e.into_inner().clone(),
            };
            cfg.local_engines()
                .into_iter()
                .find(|l| l.id == engine_id)
                .map(|l| QueueLimits::from_session(&l.session))
                .unwrap_or_default()
        }
    };
    let first = limits_now();
    if first.is_empty() {
        tracing::info!(engine = %engine_id, "queue: no ceiling configured, every torrent may run");
    }
    tokio::spawn(async move {
        if !crate::shutdown::sleep(SETTLE).await {
            return;
        }
        if !first.is_empty() {
            tracing::info!(engine = %engine_id, downloads = ?first.downloads, seeds = ?first.seeds,
                total = ?first.total, "queue manager started");
        }
        // Seeds THIS loop stopped. Only those are ever started again by it: a
        // seed stopped by anything else (an operator, `start_paused`) is not
        // the queue's to resume.
        let mut parked: std::collections::HashSet<typhon_engine::torrent::meta::InfoHash> = Default::default();
        // Since when each running torrent has been under its slow threshold.
        let mut slow_since: SlowClock = Default::default();
        loop {
            if !crate::shutdown::sleep(DOWNLOAD_SLOT_INTERVAL).await {
                return;
            }
            let limits = limits_now();
            if limits.is_empty() && parked.is_empty() {
                continue;
            }
            // Read once per pass, not once per torrent: this is a ceiling of a
            // few dozen slots against a catalogue of hundreds of thousands.
            let paused: std::collections::HashSet<String> = match store.read() {
                Ok(store) => store
                    .paused_hashes(&engine_id)
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                Err(_) => Default::default(),
            };
            let forced: std::collections::HashSet<String> = match store.read() {
                Ok(store) => store.pinned(&engine_id).unwrap_or_default().into_iter().collect(),
                Err(_) => Default::default(),
            };
            enforce_queue(&manager, &cache, &limits, &paused, &forced, &mut parked, &mut slow_since, std::time::Instant::now());
        }
    });
}

/// The three ceilings of one engine, `None` = not enforced, and whether slow
/// torrents are left out of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueLimits {
    pub downloads: Option<usize>,
    pub seeds: Option<usize>,
    pub total: Option<usize>,
    pub slow: Option<SlowRule>,
}

/// qBittorrent's "do not count slow torrents in these limits". A running
/// download under `dl_bps` (a seed under `ul_bps`) for `after` keeps running
/// but holds no slot. A threshold of 0 makes nothing slow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlowRule {
    pub dl_bps: u64,
    pub ul_bps: u64,
    pub after: Duration,
}

/// Since when each running torrent has been under its threshold. Rebuilt on
/// every pass from the torrents still under it, so it holds the slow running
/// set and nothing else.
pub type SlowClock = std::collections::HashMap<typhon_engine::torrent::meta::InfoHash, std::time::Instant>;

impl QueueLimits {
    /// `active_downloads` as it always was: > 0 is a ceiling, anything else
    /// none. `active_seeds` / `active_limit` only under `queueing = true`
    /// (see `config::Session::queueing` for why the switch exists), and only
    /// when > 0: qBittorrent's -1 is "unlimited", and a 0 that stops every
    /// seed of the library is never what a file that says 0 meant.
    pub fn from_session(s: &crate::config::Session) -> QueueLimits {
        let cap = |v: i64| (v > 0).then_some(v as usize);
        let queueing = s.queueing.unwrap_or(false);
        let kib = |v: Option<i64>| v.unwrap_or(2).max(0) as u64 * 1024;
        QueueLimits {
            downloads: cap(s.active_downloads),
            seeds: if queueing { s.active_seeds.and_then(cap) } else { None },
            total: if queueing { s.active_limit.and_then(cap) } else { None },
            slow: s.dont_count_slow_torrents.unwrap_or(false).then(|| SlowRule {
                dl_bps: kib(s.slow_torrent_dl_rate_threshold),
                ul_bps: kib(s.slow_torrent_ul_rate_threshold),
                after: Duration::from_secs(s.slow_torrent_inactive_timer.unwrap_or(60).max(0) as u64),
            }),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.downloads.is_none() && self.seeds.is_none() && self.total.is_none()
    }
}

/// Has this torrent all of its data? Read without the picker lock whenever
/// the status already says so.
pub(crate) fn holds_everything(t: &typhon_engine::torrent::meta::TorrentState) -> bool {
    let status = t.status.load(Ordering::Relaxed);
    if status == TorrentStatus::Seeding as u8 {
        return true;
    }
    if status == TorrentStatus::Downloading as u8 {
        return false;
    }
    // No picker = added in seed mode, its data trusted whole.
    match t.picker.get() {
        None => true,
        Some(p) => p.lock().map(|p| p.is_complete()).unwrap_or(false),
    }
}

/// Is this running torrent slow enough to stop counting? Notes it in `next`
/// while it is under the threshold, keeping the time it went under.
fn slow_long_enough(
    after: Duration,
    threshold: u64,
    rate: u64,
    ih: &typhon_engine::torrent::meta::InfoHash,
    prev: &SlowClock,
    next: &mut SlowClock,
    now: std::time::Instant,
) -> bool {
    if rate >= threshold {
        return false;
    }
    let since = prev.get(ih).copied().unwrap_or(now);
    next.insert(*ih, since);
    now.saturating_duration_since(since) >= after
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
///
/// Downloads take their slots first, then seeds fill what `active_limit`
/// leaves, as in qBittorrent. Seeds keep their place by age (oldest first,
/// qBittorrent's queue order) rather than by a swarm figure: a ranking that
/// moves would stop and start seeds every pass, and every stop is a
/// `stopped` announce.
///
/// With `limits.slow`, a running torrent under its threshold for long enough
/// is skipped by the count: it keeps running, and the next candidate gets the
/// slot. The clock is `slow_since`, read at `now`.
fn enforce_queue(
    manager: &Arc<TorrentManager>,
    cache: &Cache,
    limits: &QueueLimits,
    user_paused: &std::collections::HashSet<String>,
    forced: &std::collections::HashSet<String>,
    parked: &mut std::collections::HashSet<typhon_engine::torrent::meta::InfoHash>,
    slow_since: &mut SlowClock,
    now: std::time::Instant,
) {
    let manage_downloads = limits.downloads.is_some() || limits.total.is_some();
    let manage_seeds = limits.seeds.is_some() || limits.total.is_some();
    // A parked seed the operator (or a share limit) has since stopped is
    // theirs now: lifting the ceiling must not start it again.
    parked.retain(|ih| !user_paused.contains(&typhon_engine::torrent::hex_encode(ih)));
    if !manage_seeds && !parked.is_empty() {
        // The seed ceiling was lifted: what this loop parked runs again, and
        // nothing else is touched.
        for ih in parked.drain() {
            let _ = manager.start_torrent(&ih);
        }
    }
    if !manage_downloads && !manage_seeds {
        slow_since.clear();
        return;
    }
    let mut still_slow = SlowClock::new();

    let mut incomplete: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, i64, bool)> = Vec::new();
    let mut seeds: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, bool)> = Vec::new();
    for t in manager.all() {
        let status = t.status.load(Ordering::Relaxed);
        if status == TorrentStatus::Error as u8 {
            continue;
        }
        let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
        if user_paused.contains(&hash) {
            // Stopped by the user, running in the engine: the stop did not
            // hold. Measured 2026-10-14, 305 torrents added stopped downloaded
            // ~416 GB from a private tracker -- the cause is not found yet, and
            // whatever it is, the intent is what the user asked for. Enforce
            // it, and say so: this line is the bug's only trace.
            if !t.is_paused.load(Ordering::Relaxed) {
                tracing::warn!(hash = %hash, status, "stopped by the user but running in the engine; stopping it");
                let _ = manager.stop_torrent(&t.info_hash);
            }
            continue;
        }
        // Forced (Force download, qBittorrent's setForceStart): it runs past
        // the ceiling and takes no slot, as in qBittorrent. 4.4 stored the pin
        // and this loop never read it, so a forced torrent waited its turn
        // like any other -- or was parked again within a minute.
        if forced.contains(&hash) {
            if t.is_paused.load(Ordering::Relaxed) {
                let _ = manager.start_torrent(&t.info_hash);
            }
            parked.remove(&t.info_hash);
            continue;
        }
        let running = !t.is_paused.load(Ordering::Relaxed);
        let checking = status == TorrentStatus::Checking as u8;
        // A check holds a download slot, as it always has here.
        if status != TorrentStatus::Seeding as u8
            && t.total_downloaded.load(Ordering::Relaxed) < t.meta.total_size
            && (checking || !holds_everything(&t))
        {
            if manage_downloads {
                let seeds_n = cache.swarm_seeds(&hash);
                let active = running && status == TorrentStatus::Downloading as u8;
                incomplete.push((t, seeds_n, active));
            }
        } else if manage_seeds && !checking {
            // A seed is a candidate when it runs, or when THIS loop parked it.
            if running || parked.contains(&t.info_hash) {
                seeds.push((t, running));
            }
        }
    }

    let mut running_downloads = 0usize;
    if manage_downloads {
        // Most seeders first: the torrent likeliest to finish gets the slot, so a
        // slot is held for the shortest time and freed for the next one.
        incomplete.sort_by(|a, b| b.1.cmp(&a.1));
        let ceiling = limits.downloads.unwrap_or(usize::MAX).min(limits.total.unwrap_or(usize::MAX));
        for (torrent, _, active) in &incomplete {
            if let Some(rule) = limits.slow.filter(|_| *active) {
                let rate = torrent.download_rate.get();
                if slow_long_enough(rule.after, rule.dl_bps, rate, &torrent.info_hash, slow_since, &mut still_slow, now) {
                    continue;
                }
            }
            if running_downloads < ceiling {
                if !active {
                    let _ = manager.start_torrent(&torrent.info_hash);
                }
                running_downloads += 1;
            } else if *active {
                // Over the ceiling: park it. It keeps its progress and comes back
                // when a slot frees, which is what a queue is.
                let _ = manager.stop_torrent(&torrent.info_hash);
            }
        }
    }

    if manage_seeds {
        seeds.sort_by_key(|(t, _)| t.added_time);
        let ceiling = limits
            .seeds
            .unwrap_or(usize::MAX)
            .min(limits.total.unwrap_or(usize::MAX).saturating_sub(running_downloads));
        let (mut started, mut stopped, mut counted) = (0usize, 0usize, 0usize);
        for (torrent, running) in seeds.iter() {
            if let Some(rule) = limits.slow.filter(|_| *running) {
                let rate = torrent.upload_rate.get();
                if slow_long_enough(rule.after, rule.ul_bps, rate, &torrent.info_hash, slow_since, &mut still_slow, now) {
                    continue;
                }
            }
            if counted < ceiling {
                counted += 1;
                if !running {
                    let _ = manager.start_torrent(&torrent.info_hash);
                    parked.remove(&torrent.info_hash);
                    started += 1;
                }
            } else if *running {
                let _ = manager.stop_torrent(&torrent.info_hash);
                parked.insert(torrent.info_hash);
                stopped += 1;
            }
        }
        // A parked seed that left the catalogue is not ours to remember.
        parked.retain(|ih| manager.get(ih).is_some());
        if started + stopped > 0 {
            tracing::info!(started, stopped, queued = parked.len(), ceiling, "queue: seeds");
        }
    }
    *slow_since = still_slow;
}

// --- share limits ------------------------------------------------------------

const SHARE_LIMIT_INTERVAL: Duration = Duration::from_secs(120);
/// At most this many stops or removals per pass. A ratio limit turned on over
/// a large library is reached by most of it at once: the work is spread over
/// passes instead of holding the store and the disk for an hour in one.
const SHARE_LIMIT_MAX_ACTIONS: usize = 500;

/// What the share-limit worker remembers between passes.
#[derive(Default)]
pub struct ShareLimitBook {
    /// Per torrent watched for inactivity: the upload total last seen and
    /// when it last moved. Only torrents with an inactivity limit have an
    /// entry (~50 bytes each), and the clock starts when the worker first
    /// sees them -- so a restart grants a full inactivity period again
    /// rather than acting on a figure it never measured.
    idle: std::collections::HashMap<typhon_engine::torrent::meta::InfoHash, (u64, i64, u32)>,
    /// Pass counter: an `idle` entry not stamped by the current pass is
    /// dropped at its end, without a second set the size of the library.
    pass: u32,
    /// Torrents already reported as held by their tracker's minimum, so the
    /// log says it once and not every two minutes.
    held: std::collections::HashSet<typhon_engine::torrent::meta::InfoHash>,
}

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShareLimitOutcome {
    pub stopped: usize,
    pub removed: usize,
    /// Reached a limit but still owed seeding time to its tracker.
    pub held: usize,
}

/// Apply the share limits of one engine (qBittorrent's `max_ratio`,
/// `max_seeding_time`, `max_inactive_seeding_time`, and the torrents' own).
///
/// ⚠ Cost, at a million torrents. The pass walks the WHOLE engine only when
/// the engine itself has a limit on, which is the only case where every
/// torrent is concerned; otherwise it reads the store's sparse per-torrent
/// table (the torrents that have a limit of their own, a handful) and looks
/// those up by hash. With no limit anywhere -- the default -- it reads that
/// table and stops. A full walk is a few atomics per torrent, every two
/// minutes, off the request path; nothing is copied into the store.
pub fn spawn_share_limits(
    state: crate::api::AppState,
    manager: Arc<TorrentManager>,
    cfg_handle: Arc<std::sync::RwLock<Arc<crate::config::Config>>>,
    engine_id: String,
) {
    tokio::spawn(async move {
        if !crate::shutdown::sleep(SETTLE + Duration::from_secs(60)).await {
            return;
        }
        let book = Arc::new(std::sync::Mutex::new(ShareLimitBook::default()));
        loop {
            let cfg = match cfg_handle.read() {
                Ok(g) => g.clone(),
                Err(e) => e.into_inner().clone(),
            };
            let (st, mgr, id, book2) = (state.clone(), manager.clone(), engine_id.clone(), book.clone());
            // Blocking: a removal unlinks files.
            let _ = tokio::task::spawn_blocking(move || {
                let mut b = book2.lock().unwrap_or_else(|p| p.into_inner());
                share_limits_pass(&st, &mgr, &cfg, &id, typhon_engine::torrent::meta::now_secs(), &mut b)
            })
            .await;
            if !crate::shutdown::sleep(SHARE_LIMIT_INTERVAL).await {
                return;
            }
        }
    });
}

/// One pass over one engine. See `spawn_share_limits`.
///
/// Never touches a torrent that is downloading, checking, in error, or that
/// the operator stopped (the store's intent): only a seed. And never before
/// its tracker's minimum (`seed_obligation_met`, the race drain's own test):
/// a seed that still owes its tracker seeding time is left seeding whatever
/// the limit says, for every action, `stop` included -- a stopped torrent is
/// what Sonarr's "Remove Completed" deletes next, so a stop before the
/// minimum is the same hit-and-run as a removal. A tracker with NO
/// declaration is held the same way, as the drain holds it: an unknown rule
/// is not "no rule". The operator opts a tracker in by declaring its hours,
/// 0 included.
pub fn share_limits_pass(
    state: &crate::api::AppState,
    manager: &Arc<TorrentManager>,
    cfg: &crate::config::Config,
    engine_id: &str,
    now: i64,
    book: &mut ShareLimitBook,
) -> ShareLimitOutcome {
    use crate::sharelimits::{effective, reached, Action};
    let engine = crate::sharelimits::engine_limits_in(cfg, engine_id);
    let overrides = crate::sharelimits::overrides(state);
    let mut out = ShareLimitOutcome::default();
    let candidates: Vec<Arc<typhon_engine::torrent::meta::TorrentState>> = if engine.any() {
        manager.all()
    } else {
        overrides
            .iter()
            .filter(|(_, own)| effective(own, &engine).any())
            .filter_map(|(h, _)| typhon_engine::torrent::hex_decode(h).ok())
            .filter_map(|ih| manager.get(&ih))
            .collect()
    };
    if candidates.is_empty() {
        book.idle.clear();
        book.held.clear();
        return out;
    }
    let paused: std::collections::HashSet<String> = state
        .store
        .read()
        .ok()
        .and_then(|s| s.paused_hashes(engine_id).ok())
        .unwrap_or_default()
        .into_iter()
        .collect();
    let follow = crate::sharelimits::TorrentShareLimits::default();
    book.pass = book.pass.wrapping_add(1);
    let stamp = book.pass;
    let mut actions = 0usize;
    for t in candidates {
        if t.is_removed.load(Ordering::Relaxed) {
            continue;
        }
        let status = t.status.load(Ordering::Relaxed);
        // A seed, running or parked by the queue. Downloading, checking,
        // moving into place or in error is not a seed yet.
        if status != TorrentStatus::Seeding as u8
            && !(status == TorrentStatus::Stopped as u8 && holds_everything(&t))
        {
            continue;
        }
        let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
        if paused.contains(&hash) {
            continue;
        }
        let eff = effective(overrides.get(&hash).unwrap_or(&follow), &engine);
        if !eff.any() {
            continue;
        }
        let idle = if eff.inactive_minutes >= 0 {
            let up = t.total_uploaded.load(Ordering::Relaxed);
            let e = book.idle.entry(t.info_hash).or_insert((up, now, stamp));
            if e.0 != up {
                *e = (up, now, stamp);
            }
            e.2 = stamp;
            Some(now - e.1)
        } else {
            None
        };
        let Some(why) = reached(&eff, || crate::row::torrent_ratio(&t), t.seed_time_now(now), idle) else {
            continue;
        };
        let (met, host, hours) = seed_obligation_met(&t, cfg, now);
        if !met {
            out.held += 1;
            if book.held.insert(t.info_hash) {
                if hours < 0 {
                    tracing::info!(engine = %engine_id, name = %t.meta.name, tracker = %host, reason = %why,
                        "share limit reached, held: the tracker declares no minimum seeding time \
                         (announce_min_seed_hours), so it is not known to be free to go -- declare one, 0 included");
                } else {
                    tracing::info!(engine = %engine_id, name = %t.meta.name, tracker = %host, owed_hours = hours,
                        reason = %why, "share limit reached, held: the tracker's minimum seeding time is not served yet");
                }
            }
            continue;
        }
        if actions >= SHARE_LIMIT_MAX_ACTIONS {
            continue;
        }
        match engine.action {
            Action::Stop => {
                if let Ok(store) = state.store.lock() {
                    crate::store::log_store("set_paused", store.set_paused(&hash, engine_id, true));
                }
                crate::api::apply_pause_to_engine(state, engine_id, &hash, true);
                out.stopped += 1;
                tracing::info!(engine = %engine_id, name = %t.meta.name, reason = %why, "share limit reached: stopped");
            }
            Action::Remove | Action::RemoveWithFiles => {
                let with_files = engine.action == Action::RemoveWithFiles;
                // The API's own path: the trackers hear `stopped`, the store
                // row goes, and the files only with the last copy.
                match crate::api::remove_one_torrent(state, &hash, &[engine_id.to_string()], engine_id, with_files) {
                    Ok(_) => {
                        out.removed += 1;
                        tracing::info!(engine = %engine_id, name = %t.meta.name, reason = %why, with_files,
                            "share limit reached: removed");
                    }
                    Err(e) => tracing::warn!(engine = %engine_id, name = %t.meta.name,
                        "share limit reached but the removal failed: {e}"),
                }
            }
        }
        actions += 1;
        book.idle.remove(&t.info_hash);
        book.held.remove(&t.info_hash);
    }
    // Forget what no longer has an inactivity limit, or no longer exists.
    book.idle.retain(|_, e| e.2 == stamp);
    if out.stopped + out.removed > 0 {
        tracing::info!(engine = %engine_id, stopped = out.stopped, removed = out.removed, held = out.held,
            "share limits applied");
    }
    out
}

#[cfg(test)]
mod seed_obligation_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// A manager on a throwaway tree, so each test owns its state database.
    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-workers-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let data = root.join("data");
        let resume = root.join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        (mgr, root)
    }

    /// A minimal single-file torrent. The bencode lengths are COMPUTED: a
    /// hand-counted one yields a file the parser refuses for a reason that has
    /// nothing to do with the test.
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(
            format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes(),
        );
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        info.extend_from_slice(&[0xAB; 20]);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// One torrent, announcing to `tracker`, having seeded `seeded_secs`.
    fn torrent(
        mgr: &Arc<TorrentManager>,
        name: &str,
        tracker: Option<&str>,
        seeded_secs: i64,
    ) -> Arc<typhon_engine::torrent::meta::TorrentState> {
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
            .expect("the fixture torrent parses");
        let t = mgr.get(&ih).expect("just added");
        {
            let mut live = t.live_trackers.write();
            live.clear();
            if let Some(url) = tracker {
                live.push(vec![url.to_string()]);
            }
        }
        t.seed_secs.store(seeded_secs, Ordering::Relaxed);
        t.seed_since.store(0, Ordering::Relaxed);
        t
    }

    fn config(declared: &[(&str, &str)]) -> crate::config::Config {
        let mut cfg: crate::config::Config =
            toml::from_str("").expect("every Config field has a serde default");
        for (host, hours) in declared {
            cfg.announce_min_seed_hours
                .insert(host.to_string(), hours.to_string());
        }
        cfg
    }

    const NOW: i64 = 1_700_000_000;

    /// ⭐⭐ THE race policy: a tracker the operator has NOT declared a seed
    /// obligation for is PROTECTED, not free to drop. Reading an absent
    /// declaration as "nothing is owed" is what would delete a torrent from a
    /// tracker whose rules nobody wrote down.
    #[test]
    fn an_undeclared_tracker_is_protected_not_free_to_drop() {
        let (mgr, root) = manager("undeclared");
        let t = torrent(&mgr, "a", Some("https://tracker.example/announce"), 10_000_000);
        let (met, host, hours) = seed_obligation_met(&t, &config(&[]), NOW);
        assert!(!met, "an undeclared tracker never counts as satisfied");
        assert_eq!(host, "tracker.example");
        assert_eq!(hours, -1, "-1 is what marks it undeclared rather than zero");
        let _ = std::fs::remove_dir_all(root);
    }

    /// No tracker at all: nobody is owed anything, so the torrent is free.
    #[test]
    fn a_torrent_with_no_tracker_owes_nothing() {
        let (mgr, root) = manager("notracker");
        let t = torrent(&mgr, "b", None, 0);
        let (met, host, hours) = seed_obligation_met(&t, &config(&[]), NOW);
        assert!(met, "there is no tracker to owe anything to");
        assert!(host.is_empty());
        assert_eq!(hours, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A declaration of zero hours is a real declaration: the operator said
    /// this tracker asks for nothing.
    #[test]
    fn a_declared_zero_is_an_obligation_that_is_already_met() {
        let (mgr, root) = manager("zero");
        let t = torrent(&mgr, "c", Some("https://tracker.example/announce"), 0);
        let (met, host, hours) = seed_obligation_met(&t, &config(&[("tracker.example", "0")]), NOW);
        assert!(met);
        assert_eq!(host, "tracker.example");
        assert_eq!(hours, 0);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seeding_short_of_the_declared_hours_is_not_met() {
        let (mgr, root) = manager("short");
        // Two hours owed, one hour served.
        let t = torrent(&mgr, "d", Some("https://tracker.example/announce"), 3600);
        let (met, _, hours) = seed_obligation_met(&t, &config(&[("tracker.example", "2")]), NOW);
        assert!(!met);
        assert_eq!(hours, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn seeding_past_the_declared_hours_is_met() {
        let (mgr, root) = manager("long");
        let t = torrent(&mgr, "e", Some("https://tracker.example/announce"), 2 * 3600);
        let (met, _, hours) = seed_obligation_met(&t, &config(&[("tracker.example", "2")]), NOW);
        assert!(met, "exactly the declared time counts as served");
        assert_eq!(hours, 2);
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⚠️ A declaration that is not a number is not a declaration. Parsing it
    /// as zero would turn a typo into "this tracker asks for nothing" and free
    /// every torrent on it.
    #[test]
    fn an_unparseable_declaration_protects_rather_than_frees() {
        let (mgr, root) = manager("garbage");
        let t = torrent(&mgr, "f", Some("https://tracker.example/announce"), 10_000_000);
        for bad in ["abc", "", "2h", "-1"] {
            let (met, _, hours) =
                seed_obligation_met(&t, &config(&[("tracker.example", bad)]), NOW);
            assert!(!met, "{bad:?} must not free the torrent");
            assert_eq!(hours, -1, "{bad:?} reads as undeclared");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The declaration is keyed on the tracker HOST, so a declaration for
    /// another tracker must not apply here.
    #[test]
    fn a_declaration_for_another_tracker_does_not_apply() {
        let (mgr, root) = manager("otherhost");
        let t = torrent(&mgr, "g", Some("https://tracker.example/announce"), 10_000_000);
        let (met, _, hours) =
            seed_obligation_met(&t, &config(&[("other-tracker.example", "0")]), NOW);
        assert!(!met, "this host is still undeclared");
        assert_eq!(hours, -1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Whitespace around a declared value is the operator's typing, not a
    /// different value.
    #[test]
    fn a_declaration_is_trimmed_before_it_is_read() {
        let (mgr, root) = manager("trim");
        let t = torrent(&mgr, "h", Some("https://tracker.example/announce"), 7200);
        let (met, _, hours) = seed_obligation_met(&t, &config(&[("tracker.example", " 2 ")]), NOW);
        assert!(met);
        assert_eq!(hours, 2);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod disk_usage_tests {
    /// Used is what the filesystem counts as taken, NOT total minus free:
    /// reserved blocks are neither available nor used by us, and counting them
    /// as used would trigger a drain on a disk that is not full.
    #[test]
    fn reserved_blocks_count_as_neither_used_nor_free() {
        let (used, total) = super::disk_usage(std::path::Path::new("/tmp"))
            .expect("/tmp is on a filesystem");
        assert!(total > 0);
        assert!(used <= total);
    }

    #[test]
    fn a_path_that_does_not_exist_has_no_usage() {
        assert!(super::disk_usage(std::path::Path::new("/tmp/typhon-no-such-dir-9f2b")).is_none());
    }
}

/// Re-run the health invariants on a timer and keep the last report.
///
/// The scan walks both catalogues and stats the ghosts, so it is not free: it
/// runs every five minutes, as 3.x did, and the route serves whatever the last
/// pass found rather than scanning on request. A panel refresh must not be
/// able to walk 244k torrents.
///
/// ⚠ Written in 3.x's image and never spawned until 4.4: the anomaly route
/// answered zeros for every invariant this checks. It now runs, on a
/// blocking thread (the pass stats directories and locks pickers), and
/// publishes to `health::latest`.
pub fn spawn_health_scan(engines: Arc<crate::engines::EngineHost>) {
    tokio::spawn(async move {
        // Not at boot: the first minutes of a process are the announce burst
        // and the store's busiest, and nothing here is urgent.
        if !crate::shutdown::sleep(Duration::from_secs(120)).await {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            if !crate::shutdown::tick(&mut tick).await {
                return;
            }
            let engines = engines.clone();
            let _ = tokio::task::spawn_blocking(move || health_pass(&engines)).await;
        }
    });
}

/// Keep the volumes in fault current for `/health`: one blocking pass every
/// 30 s, so the route itself walks nothing.
pub fn spawn_volume_fault_scan(engines: Arc<crate::engines::EngineHost>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            if !crate::shutdown::tick(&mut tick).await {
                return;
            }
            let engines = engines.clone();
            let _ = tokio::task::spawn_blocking(move || crate::obs::refresh_volume_faults(&engines)).await;
        }
    });
}

/// One anomaly pass over every engine, published when done.
pub fn health_pass(engines: &crate::engines::EngineHost) {
    let started = std::time::Instant::now();
    let mut report = crate::health::Report::default();
    report.skip_outage = true;
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
        let (n_issues, issues) = engine.manager.startup_issues();
        crate::health::scan_startup_issues(&engine.id, &issues, n_issues, &mut report);
        per_engine.push((engine.id.clone(), torrents, engine.manager.clone()));
    }
    crate::health::scan_dual_seed_live(&per_engine, &mut report);
    // Not scanned: a path shared by two torrents is found where it is
    // created, at the add or the recheck, and kept there. Grouping a million
    // torrents' files by path every five minutes would cost more than the
    // whole rest of this pass.
    for e in crate::pathclaim::recent() {
        report.add(crate::health::Anomaly {
            kind: crate::health::PATH_COLLISION.to_string(),
            engine: e.engine,
            info_hash: e.info_hash,
            name: e.name,
            detail: format!("{}: {}", e.action, e.detail),
            wasted_bytes: 0,
        });
    }
    let found: i64 = report.counts.values().sum();
    if found > 0 {
        tracing::info!(anomalies = found, "health scan found something");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::health::publish(now, started.elapsed().as_millis() as u64, report);
}

/// Copy each torrent's seed counter from the engine into the store.
///
/// The engine owns the number and persists it itself (`seed_secs` in its
/// state.db). Everything that DECIDES on it -- workflows, graduation purges,
/// obligations -- reads the engine's live value, and so does the UI row. The
/// store's `torrents.seeding_time` is only the row's fallback for a torrent no
/// engine holds any more, where the last copied figure is all there is.
///
/// ⚠ Daily, in small transactions, well spaced. This ran hourly in chunks of
/// 2 000, starting two minutes after boot: at a million torrents a pass was
/// ~500 transactions of 17 MB of rollback journal each, the store's lock held
/// 1.45 s of every 1.5 s for twelve minutes of every hour, and every store
/// operation in that window -- an add, a tag, a category, a list page --
/// waited ~0.6 s behind it. A hundred tags took a minute. For a fallback
/// figure, a day of lag costs nothing.
pub fn spawn_seed_time_sync(
    manager: Arc<TorrentManager>,
    store: Arc<crate::store::StoreLock>,
    engine_id: String,
) {
    tokio::spawn(async move {
        let mut last: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        // Not at boot: the first pass rewrites every row (nothing is known of
        // the last one), and a restart is when the store is busiest.
        if !crate::shutdown::sleep(SEED_SYNC_FIRST).await {
            return;
        }
        loop {
            let now = typhon_engine::torrent::meta::now_secs();
            let mut rows: Vec<(String, i64)> = Vec::new();
            for t in manager.all() {
                let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
                let secs = t.seed_time_now(now);
                if last.get(&hash).copied() != Some(secs) {
                    rows.push((hash, secs));
                }
            }
            // ⚠ IN CHUNKS, releasing the store between each one. The first
            // version took the mutex once for the whole catalogue: on 295k
            // torrents that is a single transaction held for minutes, and
            // every route that touches the store hangs behind it. /health kept
            // answering in under a millisecond while /api/status timed out --
            // measured on production, and invisible on a 31-torrent bench.
            const CHUNK: usize = 250;
            let mut wrote_total = 0usize;
            let mut failed = false;
            for chunk in rows.chunks(CHUNK) {
                let wrote = {
                    let st = match store.lock() {
                        Ok(s) => s,
                        Err(e) => e.into_inner(),
                    };
                    st.update_seeding_times(chunk)
                };
                match wrote {
                    Ok(n) => {
                        wrote_total += n;
                        for (hash, secs) in chunk {
                            last.insert(hash.clone(), *secs);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(engine = %engine_id, "seed time sync: {e}");
                        failed = true;
                        break;
                    }
                }
                // Hands the lock to whoever is waiting before taking it again,
                // long enough that the others get most of the time.
                if !crate::shutdown::sleep(SEED_SYNC_YIELD).await {
                    return;
                }
            }
            if wrote_total > 0 && !failed {
                tracing::info!(engine = %engine_id, rows = wrote_total, "seed time synced");
            }
            if !crate::shutdown::sleep(SEED_SYNC_EVERY).await {
                return;
            }
        }
    });
}

const SEED_SYNC_FIRST: Duration = Duration::from_secs(3600);
const SEED_SYNC_EVERY: Duration = Duration::from_secs(24 * 3600);
const SEED_SYNC_YIELD: Duration = Duration::from_millis(250);

/// Delete from a graduation TARGET what has finished paying its seeding time.
///
/// A category that something graduates into is a transit area, not a library:
/// a torrent lands there owing seeding time, serves it, and goes. Without this
/// the transit area only ever grows, and the race disk is simply emptied into
/// the pool.
///
/// ⚠⚠ SCOPED TO GRADUATION TARGETS, and that scope is the whole safety of it.
/// The same rule applied to every category of the hoard would walk a 293 000
/// torrent library deleting everything whose tracker declares an obligation it
/// has already met. The library is not transit; nothing graduates into it.
///
/// No pressure condition: the obligation is a property of the torrent, so the
/// moment it is paid the torrent has no reason to hold the space.
pub fn spawn_transit_sweep(
    state: crate::api::AppState,
    manager: Arc<TorrentManager>,
    cfg_handle: Arc<std::sync::RwLock<Arc<crate::config::Config>>>,
    engine_id: String,
) {
    tokio::spawn(async move {
        if !crate::shutdown::sleep(Duration::from_secs(90)).await {
            return;
        }
        loop {
            let cfg = match cfg_handle.read() {
                Ok(g) => g.clone(),
                Err(e) => e.into_inner().clone(),
            };
            let targets = crate::api::graduation_target_categories(&state);
            if !targets.is_empty() {
                let now = typhon_engine::torrent::meta::now_secs();
                let mut removed = 0usize;
                for t in manager.all() {
                    let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
                    if !targets.contains(&crate::api::category_of_hash(&state, &hash, &engine_id)) {
                        continue;
                    }
                    let (met, host, hours) = seed_obligation_met(&t, &cfg, now);
                    // hours < 0 is an UNDECLARED tracker, which `met` reports as
                    // false. Nothing to do but wait: an undeclared obligation is
                    // never paid, so the operator has to declare one.
                    if !met || hours < 0 {
                        continue;
                    }
                    // Same shared path as the drain, for the same reason: the
                    // store row has to go with the torrent.
                    match crate::api::remove_one_torrent(
                        &state,
                        &hash,
                        &[engine_id.clone()],
                        &engine_id,
                        true,
                    ) {
                        Ok(_) => {
                            removed += 1;
                            tracing::info!(name = %t.meta.name, tracker = %host,
                                "transit: seeding time served, removed");
                        }
                        Err(e) => tracing::warn!(name = %t.meta.name,
                            "transit sweep could not remove it: {e}"),
                    }
                }
                if removed > 0 {
                    tracing::info!(engine = %engine_id, removed, "transit sweep");
                }
            }
            if !crate::shutdown::sleep(Duration::from_secs(300)).await {
                return;
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
/// What one pass of the drain did, so a caller can answer with it.
///
/// The button that runs the drain by hand needs to say whether anything
/// happened; a worker that only logs leaves the UI to invent a sentence.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct DrainOutcome {
    pub deleted: usize,
    pub graduated: usize,
    pub stuck: usize,
    pub freed_bytes: u64,
}

/// Free space on the race volumes by removing what has earned its keep.
///
/// Destructive by design and gated twice: it does nothing unless the operator
/// enabled it, and nothing until a volume is over ITS high watermark. It then
/// removes only down to the low watermark -- the gap between the two is what
/// stops it running again on the next tick.
///
/// ⚠ One pass per VOLUME, never one pass for the engine. Before this, a single
/// `race_path` was measured and every torrent of the engine was a candidate,
/// so a full SSD made the drain delete races living on a different, healthy
/// SSD -- and the full one never emptied, so it did it again on the next tick.
pub fn spawn_race_drain(
    state: crate::api::AppState,
    manager: Arc<TorrentManager>,
    cfg_handle: Arc<std::sync::RwLock<Arc<crate::config::Config>>>,
    engine_id: String,
) {
    let live = || -> Arc<crate::config::Config> {
        match cfg_handle.read() {
            Ok(g) => g.clone(),
            Err(e) => e.into_inner().clone(),
        }
    };
    let config = live().race_drain.clone();
    let interval = if config.check_interval_seconds > 0 {
        Duration::from_secs(config.check_interval_seconds as u64)
    } else {
        Duration::from_secs(300)
    };
    tokio::spawn(async move {
        if !crate::shutdown::sleep(Duration::from_secs(10)).await {
            return;
        }
        tracing::info!(
            engine = %engine_id,
            check_interval_s = interval.as_secs(),
            "race drain started, one pass per volume"
        );
        loop {
            if !crate::shutdown::sleep(interval).await {
                return;
            }
            // Re-read every tick: an obligation typed into the UI has to apply
            // to the NEXT pass, not to the next restart. Volumes are rediscovered
            // for the same reason -- a disk added today is a disk watched today.
            let cfg = match cfg_handle.read() {
                Ok(g) => g.clone(),
                Err(e) => e.into_inner().clone(),
            };
            // A stat per torrent, then removals: blocking work, off the
            // async workers.
            let (state, manager, engine_id) = (state.clone(), manager.clone(), engine_id.clone());
            let _ = tokio::task::spawn_blocking(move || {
                for volume in crate::volumes::discover(&state, &manager, &cfg.race_drain) {
                    if !volume.policy.enabled {
                        continue;
                    }
                    // ALLOCATION, not occupancy: the add is refused on what is
                    // promised, so the drain must start on what is promised too.
                    if volume.alloc_pct() < volume.policy.high as f64 {
                        continue;
                    }
                    drain_once(&state, &manager, &volume, &cfg, &engine_id);
                }
            })
            .await;
        }
    });
}

/// Hours this torrent's tracker requires it to be seeded, and whether that
/// obligation is met.
///
/// ⚠ A tracker with NOTHING declared is treated as protected, not as free.
/// The drain deletes; an unknown rule and no rule are not the same thing, and
/// the cost of confusing them is a hit-and-run on an account that took months
/// to build. The operator opts a tracker INTO deletion by declaring 0.
fn seed_obligation_met(
    t: &Arc<typhon_engine::torrent::meta::TorrentState>,
    cfg: &crate::config::Config,
    now: i64,
) -> (bool, String, i64) {
    let host = t
        .live_trackers
        .read()
        .iter()
        .flatten()
        .next()
        .map(|u| typhon_engine::rpc::dispatch::tracker_host_of(u))
        .unwrap_or_default();
    // No tracker at all: nobody is owed anything.
    if host.is_empty() {
        return (true, host, 0);
    }
    let declared = cfg
        .announce_min_seed_hours
        .get(&host)
        .map(|v| v.trim().to_string());
    let Some(raw) = declared else {
        return (false, host, -1);
    };
    let hours: i64 = raw.parse().unwrap_or(-1);
    if hours < 0 {
        return (false, host, -1);
    }
    let seeded = t.seed_time_now(now);
    (seeded >= hours * 3600, host, hours)
}

/// One pass on ONE volume. Callers gate on the watermark; this frees.
pub fn drain_once(
    state: &crate::api::AppState,
    manager: &Arc<TorrentManager>,
    volume: &crate::volumes::Volume,
    cfg: &crate::config::Config,
    engine_id: &str,
) -> DrainOutcome {
    // Both ends in the same unit. Triggering on allocation and stopping on
    // occupancy would free NOTHING whenever the disk is committed but not yet
    // written -- the drain would fire and return empty-handed.
    let target = volume.total as f64 * volume.policy.low as f64 / 100.0;
    let wanted = volume.allocated() as f64 - target;
    let mut to_free = wanted;
    if to_free <= 0.0 {
        return DrainOutcome::default();
    }
    // `basis` says what the percentages are of. On a shared seedbox slot a
    // bare "96" cannot be told apart from the disk the neighbours filled.
    tracing::warn!(
        volume = %volume.id,
        pct = volume.used_pct().round(),
        alloc_pct = volume.alloc_pct().round(),
        basis = %volume.basis(),
        committed_gb = (volume.committed as f64 / 1e9).round(),
        high = volume.policy.high,
        "race volume over its high watermark, draining"
    );

    // Oldest first: a race that has been sitting the longest has had the most
    // time to earn its ratio, so it is the cheapest to let go.
    // The whole point: a torrent on another disk is not a candidate, however
    // full this one is.
    let mut torrents: Vec<_> = manager
        .all()
        .into_iter()
        .filter(|t| {
            let path = t.save_path.read().clone();
            crate::volumes::device_of_nearest(&path) == Some(volume.dev)
        })
        .collect();
    torrents.sort_by_key(|t| t.added_time);

    let now = typhon_engine::torrent::meta::now_secs();
    let mut deleted = 0usize;
    let mut graduated = 0usize;
    let mut stuck = 0usize;
    // Categories whose `graduate_to` lands back on this same engine, with a
    // count each. A silent `stuck += 1` here is how 142 torrents piled up over
    // ten hours on 2026-09-16 with nothing in the log to explain it.
    let mut stuck_graduate_here: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    // Torrents whose graduation would land on files already at the
    // destination, by category: the job would refuse them, and queueing it
    // anyway would fail the same way every tick.
    let mut stuck_dest_taken: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    // Torrents the target engine already holds, by category: counted, not
    // logged one by one, for the same reason.
    let mut stuck_held_by_target: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for torrent in torrents {
        if to_free <= 0.0 {
            break;
        }
        // The obligation comes FIRST, before size or age. Sorting by added_time
        // and deleting the oldest is a proxy for "has earned its keep" that
        // breaks exactly when a tracker has a minimum: under high churn the
        // oldest race on the disk can still be six hours old.
        // ⭐ The fate of a torrent is a property of the TORRENT, not of the
        // pressure and not of an operator switch. It owes seeding time or it
        // does not; everything else follows. Deciding by pressure would give
        // the same torrent a different fate depending on when the drain
        // happened to look at it.
        //
        //   obligation met     -> delete, the ratio is earned
        //   obligation not met -> graduate, it has to keep seeding elsewhere
        //
        // An UNDECLARED tracker counts as not met: an unknown rule and no rule
        // are not the same thing, and the cost of confusing them is a
        // hit-and-run. But unlike before it no longer blocks the drain -- the
        // torrent leaves the SSD by moving instead of by being deleted.
        let hash = typhon_engine::torrent::hex_encode(&torrent.info_hash);
        let size = torrent.meta.total_size as f64;
        let (met, host, _hours) = seed_obligation_met(&torrent, cfg, now);
        if !met {
            let Some((to_engine, to_category, save_path)) =
                crate::api::category_graduation(state, &hash, engine_id)
            else {
                // Nowhere to put it and no right to delete it. Said out loud,
                // with the category, because a silent counter here is exactly
                // how this drain spent a day doing nothing.
                stuck += 1;
                tracing::warn!(
                    name = %torrent.meta.name,
                    tracker = %host,
                    category = %crate::api::category_of_hash(state, &hash, engine_id),
                    "still owes seeding time and its category has no graduate_to:                      it can be neither deleted nor moved"
                );
                continue;
            };
            if to_engine == engine_id {
                // Graduating onto the engine it is already on moves nothing.
                // Counted by category rather than logged per torrent: this runs
                // every 60s over the whole volume, so a line each would be
                // hundreds a minute -- and the category is the thing to fix.
                *stuck_graduate_here
                    .entry(crate::api::category_of_hash(state, &hash, engine_id))
                    .or_insert(0usize) += 1;
                stuck += 1;
                continue;
            }
            // Checked here as well as in the job, and only the cheap half (the
            // destination, no scan of the other torrents): a refusal is a
            // property of the disk, and it would come back every tick.
            if crate::jobsrun::plan_move(&torrent, std::path::Path::new(&save_path)).refusal().is_some() {
                *stuck_dest_taken
                    .entry(crate::api::category_of_hash(state, &hash, engine_id))
                    .or_insert(0usize) += 1;
                stuck += 1;
                continue;
            }
            // The target engine already has this torrent: the job refuses it
            // before moving a file, so queueing it only fails every tick.
            // Counted as stuck so "cannot free" is said; nothing is removed
            // from either engine here.
            if state.engines.get(&to_engine).is_some_and(|d| d.manager.get(&torrent.info_hash).is_some()) {
                *stuck_held_by_target
                    .entry(crate::api::category_of_hash(state, &hash, engine_id))
                    .or_insert(0usize) += 1;
                stuck += 1;
                continue;
            }
            // Charged to the budget at QUEUE time, not at completion: the copy
            // takes minutes and the drain ticks every 60s. Not counting it
            // would re-queue the same torrents on every tick and flood the job
            // table; `queue_graduation` already refuses a duplicate, so the
            // budget is the only thing that would be wrong.
            if crate::jobsrun::queue_graduation(
                state,
                &hash,
                &torrent.meta.name,
                &engine_id,
                &to_engine,
                &to_category,
                &save_path,
                torrent.meta.total_size as i64,
            )
            .is_some()
            {
                to_free -= size;
                graduated += 1;
                tracing::info!(name = %torrent.meta.name, tracker = %host,
                    to = %to_category, "graduating: still owes seeding time");
            }
            continue;
        }
        // Through `remove_one_torrent`, the same path the DELETE route and the
        // qBit shim take, because it drops the STORE ROW as well.
        //
        // ⚠ Calling `manager.remove_torrent` directly does not. Measured on the
        // bench: 10 torrents gone from the engine and still in the store, which
        // is the shape of a ghost -- a row nothing can reach and the reconcile
        // refuses to clean because removing them all trips its 1% guard.
        //
        // ⚠ `delete_files: true` here. The engine-level call takes the OPPOSITE
        // flag (`keep_data`), and this is the position where the two are easy
        // to confuse: passing false would free nothing and the next tick would
        // drain again forever.
        match crate::api::remove_one_torrent(state, &hash, &[engine_id.to_string()], engine_id, true) {
            Ok(_) => {
                to_free -= size;
                deleted += 1;
                tracing::info!(name = %torrent.meta.name, tracker = %host, "drained");
            }
            Err(e) => tracing::warn!(name = %torrent.meta.name, "drain could not remove it: {e}"),
        }
    }

    // A drain that cannot free what it needs has to SAY so. Otherwise the disk
    // fills while the worker reports nothing, which is the failure this very
    // drain already had once when it was watching the wrong path.
    if !stuck_graduate_here.is_empty() {
        let detail = stuck_graduate_here
            .iter()
            .map(|(cat, n)| format!("{cat} x{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            volume = %volume.id,
            categories = %detail,
            "graduate_to points back at this engine, so these torrents can never leave the volume:              set graduate_to to a category that files under a DIFFERENT engine"
        );
    }
    if !stuck_held_by_target.is_empty() {
        let detail = stuck_held_by_target
            .iter()
            .map(|(cat, n)| format!("{cat} x{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            volume = %volume.id,
            categories = %detail,
            "not graduated: the target engine already holds these torrents; remove the copy on one side"
        );
    }
    if !stuck_dest_taken.is_empty() {
        let detail = stuck_dest_taken
            .iter()
            .map(|(cat, n)| format!("{cat} x{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::warn!(
            volume = %volume.id,
            categories = %detail,
            "not graduated: files of the same name already sit at the destination, and moving onto them would overwrite them"
        );
    }
    if to_free > 0.0 {
        tracing::warn!(
            volume = %volume.id,
            basis = %volume.basis(),
            still_needed_gb = (to_free / 1e9).round(),
            deleted,
            graduated,
            stuck,
            "race drain could not free enough"
        );
    }
    // Measured again, not assumed: deletions free space now, a graduation only
    // queues a copy, so the two do not move this number the same way.
    //
    // ⭐ And the declared sizes are NOT what came back. Measured on the bench:
    // a pass reported 8 MiB freed and the disk gave back 4, because one of the
    // two torrents was a ghost -- a store row whose data was already gone, so
    // deleting it released nothing. `meta.total_size` is what a torrent CLAIMS
    // to occupy; only statvfs knows what the filesystem handed back. The budget
    // below still runs on the declared sizes (something has to decide what to
    // remove before removing it), but what is reported and archived is the
    // delta that actually happened.
    // `after_drain` keeps the after-percentage on the same basis the trigger
    // used (the quota, when there is one) while freed stays the disk delta.
    let (freed, after) = volume.after_drain(
        crate::volumes::usage(std::path::Path::new(&volume.id)).map(|(used, total, _)| (used, total)),
    );
    if deleted > 0 || graduated > 0 || stuck > 0 {
        let store = match state.store.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        if let Err(e) = store.record_drain(
            typhon_engine::torrent::meta::now_secs(),
            &volume.id,
            volume.used_pct(),
            after,
            deleted as i64,
            graduated as i64,
            stuck as i64,
            freed as i64,
        ) {
            tracing::warn!("drain history not written: {e}");
        }
    }
    // Right after the deletions, when the SSD has the most freed blocks it has
    // not been told about. A graduation frees nothing yet; its copy's removal
    // is trimmed by the next drain.
    if deleted > 0 && freed > 0 {
        crate::volumes::trim_after_drain(state, &volume.id);
    }
    DrainOutcome {
        deleted,
        graduated,
        stuck,
        freed_bytes: freed,
    }
}

/// Bytes used and total on the filesystem holding `path`.
fn disk_usage(path: &std::path::Path) -> Option<(u64, u64)> {
    crate::platform::usage(path).map(|(used, total, _)| (used, total))
}

/// Watch our own memory and say so before the kernel does.
///
/// 3.x watched the engine *process* -- was it alive, how much had it taken --
/// because the engine was a separate process it had spawned. Half of that
/// disappears here: there is no other process to find dead. What remains is
/// the ceiling, and it still matters, because the way this ends otherwise is
/// the OOM killer taking the whole thing with no warning and no dump.
pub fn spawn_memory_watch(limit_bytes: u64) {
    watch_memory(limit_bytes, resident_bytes)
}

/// Start-up wrapper for `[daemon] memory_alert_mb`: says in the log whether the
/// memory watch is on, then starts it. Returns whether a watch was started.
pub fn start_memory_watch(limit_bytes: u64) -> bool {
    if limit_bytes == 0 {
        tracing::info!("memory alert: off ([daemon] memory_alert_mb = 0)");
        return false;
    }
    tracing::info!("memory alert: on, ceiling {} MiB", limit_bytes >> 20);
    spawn_memory_watch(limit_bytes);
    true
}

/// `spawn_memory_watch` with the resident-size reader given, so a test can
/// set the reading instead of growing the whole test process to cross it.
fn watch_memory(limit_bytes: u64, mut read: impl FnMut() -> Option<u64> + Send + 'static) {
    if limit_bytes == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut over = false;
        loop {
            if !crate::shutdown::sleep(Duration::from_secs(30)).await {
                return;
            }
            let Some(rss) = read() else { continue };
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
    crate::platform::resident_bytes()
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
                if !crate::shutdown::sleep(PAUSE).await {
                    return;
                }
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

/// How long a row this process just wrote is spared by the store reconcile:
/// two passes, far longer than any add takes to reach the engine.
const RECONCILE_GRACE: Duration = Duration::from_secs(10 * 60);

pub fn spawn_store_reconcile(
    engines: Arc<crate::engines::EngineHost>,
    store: Arc<crate::store::StoreLock>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            if !crate::shutdown::tick(&mut tick).await {
                return;
            }
            // A listing of up to a million rows and a delete per orphan:
            // blocking work, off the async workers.
            let (engines, store) = (engines.clone(), store.clone());
            let _ = tokio::task::spawn_blocking(move || reconcile_pass(&engines, &store)).await;
        }
    });
}

fn reconcile_pass(engines: &crate::engines::EngineHost, store: &crate::store::StoreLock) {
    // Per engine, because the store keys its rows by session: a
    // hoard row is not an orphan just because the race does not hold
    // that torrent.
    for engine in engines.engines() {
        // Listed on the read connection -- a million rows, and it used
        // to hold every write for a second -- and dropped through the
        // shared one, below: the read connection cannot write, and a
        // delete sent there fails without a word.
        let listed = store.read().unwrap().all_hashes(&engine.id);
        let known = match listed {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(engine = %engine.id, error = %e, "store reconcile: cannot list");
                continue;
            }
        };
        // The engine is looked at AFTER the rows are listed. The other
        // way round, a torrent added between the two -- the listing
        // takes seconds at a million rows -- was in the rows and not in
        // the engine's picture, and lost its row while it kept running
        // (13/10/2026: no metainfo, every piece refused as "cannot
        // write to disk"). A row written but not yet loaded is covered
        // by the grace of `delete_settled_copy` below.
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
        if !doomed.is_empty() {
            let store = store.lock().unwrap();
            for hash in doomed {
                // This engine's row only: `all_hashes` listed this
                // engine's half, and 4.3's delete by info hash took
                // another engine's copy of the same torrent with it.
                if store.delete_settled_copy(&hash, &engine.id, RECONCILE_GRACE).unwrap_or(false) {
                    dropped += 1;
                }
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

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};
    use crate::volumes::{Policy, Volume};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn volume(total: u64, used: u64, enabled: bool, high: i64, low: i64) -> Volume {
        Volume::measure(
            "/mnt/race".into(),
            1,
            (used, total, total.saturating_sub(used)),
            None,
            0,
            0,
            0,
            Policy { enabled, high, low, inherited: true },
        )
    }

    fn race_manager(s: &TestState) -> Arc<TorrentManager> {
        s.engines
            .engines()
            .iter()
            .find(|e| e.id == "race")
            .expect("the race engine")
            .manager
            .clone()
    }

    /// ⭐⭐ Destructive by design, gated TWICE: nothing happens unless the
    /// operator enabled it, and nothing until the volume is over ITS high
    /// watermark. A drain that runs on a disk that is not full is a drain that
    /// deletes for no reason.
    #[tokio::test]
    async fn a_volume_under_its_watermark_is_left_alone() {
        let s = st("drain-under");
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        // 50% used, high at 90: nothing to do.
        let v = volume(1_000_000, 500_000, true, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0);
        assert_eq!(out.graduated, 0);
        assert_eq!(out.freed_bytes, 0);
    }

    /// Exactly AT the low watermark there is nothing left to free: the gap
    /// between high and low is what stops it running again next tick.
    #[tokio::test]
    async fn a_volume_already_at_its_low_watermark_frees_nothing() {
        let s = st("drain-atlow");
        let mgr = race_manager(&s);
        let cfg = s.cfg();
        let v = volume(1_000, 800, true, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0);
        assert_eq!(out.freed_bytes, 0);
    }

    /// A volume over its watermark but holding NOTHING cannot free anything --
    /// and must say so rather than looping or panicking on an empty catalogue.
    #[tokio::test]
    async fn an_over_full_volume_with_no_torrents_frees_nothing_without_failing() {
        let s = st("drain-empty");
        let mgr = race_manager(&s);
        let cfg = s.cfg();
        let v = volume(1_000, 990, true, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0, "there is nothing to delete");
        assert_eq!(out.graduated, 0);
        assert_eq!(out.freed_bytes, 0);
    }

    /// A volume of size zero must not produce a NaN target and start deleting.
    #[tokio::test]
    async fn a_volume_with_no_size_is_not_drained() {
        let s = st("drain-zero");
        let mgr = race_manager(&s);
        let cfg = s.cfg();
        let v = volume(0, 0, true, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0);
        assert_eq!(out.freed_bytes, 0);
    }

    /// ⚠️ The per-volume policy is what the drain reads, not the global one.
    /// A volume whose own policy is disabled is not drained even when the
    /// global default would have it drained.
    #[tokio::test]
    async fn a_disabled_policy_on_the_volume_stops_the_drain() {
        let s = st("drain-disabled");
        let mgr = race_manager(&s);
        let cfg = s.cfg();
        let v = volume(1_000, 990, false, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0, "a disabled volume is never drained");
        assert_eq!(out.freed_bytes, 0);
    }

    /// A drain pass on an engine this node does not host is a no-op.
    #[tokio::test]
    async fn draining_an_engine_that_is_not_here_does_nothing() {
        let s = st("drain-noengine");
        let mgr = race_manager(&s);
        let cfg = s.cfg();
        let v = volume(1_000, 990, true, 90, 80);
        let out = drain_once(&s.state, &mgr, &v, &cfg, "no-such-engine");
        assert_eq!(out.deleted, 0);
    }

    /// The outcome starts at zero on every field: a default that reported
    /// anything non-zero would inflate the drain history for a pass that did
    /// nothing.
    #[test]
    fn an_empty_outcome_is_zero_everywhere() {
        let o = DrainOutcome::default();
        assert_eq!((o.deleted, o.graduated, o.stuck, o.freed_bytes), (0, 0, 0, 0));
    }

    /// ⭐ A ceiling of zero or less means "no ceiling", not "nothing may run".
    /// Reading it the other way stops every download on a default config.
    #[tokio::test]
    async fn a_download_slot_ceiling_of_zero_lets_everything_run() {
        let s = st("slots-zero");
        let mgr = race_manager(&s);
        let cache = Cache::default();
        let paused = std::collections::HashSet::new();
        // Must not panic, and must not stop anything on an empty library.
        let zero = QueueLimits::from_session(&toml::from_str("active_downloads = 0").unwrap());
        assert!(zero.is_empty());
        enforce_queue(&mgr, &cache, &zero, &paused, &Default::default(), &mut Default::default(), &mut SlowClock::new(), std::time::Instant::now());
        assert_eq!(mgr.count(), 0);
    }

    #[tokio::test]
    async fn enforcing_slots_on_an_empty_engine_is_a_no_op() {
        let s = st("slots-empty");
        let mgr = race_manager(&s);
        let cache = Cache::default();
        let paused = std::collections::HashSet::new();
        let five = QueueLimits { downloads: Some(5), ..Default::default() };
        enforce_queue(&mgr, &cache, &five, &paused, &Default::default(), &mut Default::default(), &mut SlowClock::new(), std::time::Instant::now());
        assert!(mgr.all().is_empty());
    }
}

#[cfg(test)]
mod drain_policy_gate_tests {
    use super::*;
    use crate::volumes::{Policy, Volume};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// A volume reported as `used_pct` full, with the policy the test is about.
    fn volume(used_pct: u64, enabled: bool, high: i64, low: i64) -> Volume {
        let total = 100_000_000_000u64;
        let used = total / 100 * used_pct;
        Volume::measure(
            "/mnt/race".into(),
            1,
            (used, total, total - used),
            None,
            0,
            0,
            1,
            Policy { enabled, high, low, inherited: true },
        )
    }

    /// State with `n` seeding torrents on the race engine, each announcing to
    /// `tracker.example` and having seeded `seeded_secs`.
    fn state_with_torrents(
        tag: &str,
        toml_src: &str,
        n: usize,
        seeded_secs: i64,
    ) -> crate::api::testing::TestState {
        let s = crate::api::testing::state_from(tag, toml_src);
        let engines = s.engines.engines();
        let engine = engines.iter().find(|e| e.id == "race").expect("a race engine");
        for i in 0..n {
            let name = format!("t{i}");
            let (ih, _) = engine
                .manager
                .add_torrent_bytes(&torrent_bytes(&name), "/tmp", true, true)
                .unwrap_or_else(|e| panic!("add {name}: {e}"));
            let t = engine.manager.get(&ih).expect("just added");
            {
                let mut live = t.live_trackers.write();
                live.clear();
                live.push(vec!["https://tracker.example/announce".to_string()]);
            }
            t.seed_secs.store(seeded_secs, std::sync::atomic::Ordering::Relaxed);
            t.seed_since.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        s
    }

    fn race_manager(s: &crate::api::testing::TestState) -> Arc<TorrentManager> {
        s.engines
            .engines()
            .iter()
            .find(|e| e.id == "race")
            .expect("a race engine")
            .manager
            .clone()
    }

    /// ⭐⭐ Gate one: the drain DELETES DATA, so it does nothing at all unless
    /// the operator turned it on. A default install must never lose a torrent
    /// to a background task nobody asked for.
    #[test]
    fn a_disabled_drain_deletes_nothing_however_full_the_disk_is() {
        let toml = format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n"
        );
        let s = state_with_torrents("drain-off", &toml, 3, 100_000);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(99, false, 90, 80), &cfg, "race");
        assert_eq!(out.deleted, 0, "a disabled drain must not delete");
        assert_eq!(out.freed_bytes, 0);
        assert_eq!(mgr.all().len(), 3, "the library is intact");
    }

    /// ⭐ Gate two: nothing happens until the volume is over ITS OWN high
    /// watermark. The gap between high and low is what stops it running again
    /// on the next tick.
    #[test]
    fn a_volume_below_its_high_watermark_is_left_alone() {
        let toml = format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n"
        );
        let s = state_with_torrents("drain-under", &toml, 3, 100_000);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(50, true, 90, 80), &cfg, "race");
        assert_eq!(out.deleted, 0, "half full is not over the watermark");
        assert_eq!(mgr.all().len(), 3);
    }

    /// ⚠️⚠️ THE race policy, at the place it decides: a tracker for which the
    /// operator declared NO seed obligation is PROTECTED. The disk being full
    /// is not a reason to drop a torrent whose rules nobody wrote down.
    #[test]
    fn an_undeclared_tracker_is_never_drained_even_on_a_full_disk() {
        // No [announce_min_seed_hours] at all: nothing is declared.
        let toml = format!("[daemon]\napi_key = \"{KEY}\"\n");
        let s = state_with_torrents("drain-undeclared", &toml, 3, 10_000_000);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(99, true, 90, 80), &cfg, "race");
        assert_eq!(
            out.deleted, 0,
            "an undeclared tracker is protected, whatever the disk says"
        );
        assert_eq!(mgr.all().len(), 3, "every torrent survived");
    }

    /// A torrent that has NOT yet served its declared time is not free to go
    /// either -- that is the obligation the whole policy exists to honour.
    #[test]
    fn a_torrent_short_of_its_declared_seed_time_is_not_drained() {
        let toml = format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[announce_min_seed_hours]\n\"tracker.example\" = \"72\"\n"
        );
        // One hour served against seventy-two owed.
        let s = state_with_torrents("drain-short", &toml, 3, 3600);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(99, true, 90, 80), &cfg, "race");
        assert_eq!(out.deleted, 0, "the obligation is not met yet");
        assert_eq!(mgr.all().len(), 3);
    }

    /// An empty engine is a no-op, not a panic: the drain runs on a timer and
    /// will meet this state on every fresh install.
    #[test]
    fn draining_an_engine_that_holds_nothing_is_a_no_op() {
        let toml = format!("[daemon]\napi_key = \"{KEY}\"\n");
        let s = crate::api::testing::state_from("drain-empty", &toml);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(99, true, 90, 80), &cfg, "race");
        assert_eq!(out.deleted, 0);
        assert_eq!(out.graduated, 0);
        assert_eq!(out.freed_bytes, 0);
    }

    /// A volume of size zero must not be read as 100% full and trigger a
    /// deletion sweep -- the same NaN trap the `used_pct` guard exists for.
    #[test]
    fn a_volume_with_no_size_does_not_trigger_a_sweep() {
        let toml = format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n"
        );
        let s = state_with_torrents("drain-zero", &toml, 2, 100_000);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let mut v = volume(0, true, 90, 80);
        v.total = 0;
        v.used = 0;
        v.free = 0;
        let out = drain_once(&s.state, &mgr, &v, &cfg, "race");
        assert_eq!(out.deleted, 0, "a zero-size volume is 0%, not 100%");
        assert_eq!(mgr.all().len(), 2);
    }

    /// The outcome is a report, and its fields must agree with each other: no
    /// freed bytes without a deletion.
    #[test]
    fn the_outcome_never_reports_freed_bytes_without_a_deletion() {
        let toml = format!("[daemon]\napi_key = \"{KEY}\"\n");
        let s = state_with_torrents("drain-report", &toml, 2, 10_000_000);
        let mgr = race_manager(&s);
        let cfg = s.cfg();

        let out = drain_once(&s.state, &mgr, &volume(99, true, 90, 80), &cfg, "race");
        if out.deleted == 0 {
            assert_eq!(out.freed_bytes, 0, "nothing deleted, nothing freed");
        }
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;
    use std::collections::HashSet;

    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-queue-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        (mgr, root)
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// Three running seeds, oldest first.
    fn seeds(mgr: &Arc<TorrentManager>) -> Vec<Arc<typhon_engine::torrent::meta::TorrentState>> {
        ["a", "b", "c"]
            .iter()
            .map(|n| {
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", false, true).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                t.is_paused.store(false, Ordering::Relaxed);
                t
            })
            .collect()
    }

    fn limits(toml_src: &str) -> QueueLimits {
        QueueLimits::from_session(&toml::from_str(toml_src).unwrap())
    }

    /// ⭐⭐ `active_seeds` is read only under `queueing = true`: a file that
    /// carries 3.x's built-in `active_seeds = 50` (or any small value) does
    /// not stop the library at the upgrade.
    #[test]
    fn active_seeds_without_queueing_limits_nothing() {
        assert!(limits("active_seeds = 1\nactive_limit = 1").is_empty());
        assert!(limits("").is_empty());
        assert!(limits("queueing = true\nactive_seeds = -1\nactive_limit = -1").is_empty());
        assert_eq!(limits("active_downloads = 10").downloads, Some(10), "active_downloads works as before");
    }

    #[test]
    fn a_torrent_the_user_stopped_is_stopped_again_if_the_engine_runs_it() {
        // The store says stopped, the engine runs it: whatever lifted the
        // pause, the queue pass puts the user's stop back.
        let (mgr, root) = manager("intent");
        let all = seeds(&mgr);
        let t = &all[0];
        mgr.start_torrent(&t.info_hash).unwrap();
        assert!(!t.is_paused.load(Ordering::Relaxed), "fixture: running");
        let stopped_by_user: HashSet<String> = [typhon_engine::torrent::hex_encode(&t.info_hash)].into();
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &limits("queueing = true\nactive_seeds = 5"), &stopped_by_user, &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert!(t.is_paused.load(Ordering::Relaxed), "the user's stop holds");
        assert!(parked.is_empty(), "a user stop is not the queue's to undo");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_seed_queue_keeps_the_oldest_and_parks_the_rest_then_lets_them_go() {
        let (mgr, root) = manager("seeds");
        let all = seeds(&mgr);
        let cache = Cache::default();
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &cache, &limits("queueing = true\nactive_seeds = 1"), &HashSet::new(), &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        let running: Vec<bool> = all.iter().map(|t| !t.is_paused.load(Ordering::Relaxed)).collect();
        assert_eq!(running.iter().filter(|r| **r).count(), 1, "one seed slot");
        assert_eq!(parked.len(), 2);
        // Stable: a second pass moves nothing.
        enforce_queue(&mgr, &cache, &limits("queueing = true\nactive_seeds = 1"), &HashSet::new(), &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert_eq!(all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count(), 1);
        // A parked seed the operator then stops stays stopped when the
        // ceiling is lifted; the others come back.
        let operator: HashSet<String> = all
            .iter()
            .filter(|t| t.is_paused.load(Ordering::Relaxed))
            .take(1)
            .map(|t| typhon_engine::torrent::hex_encode(&t.info_hash))
            .collect();
        enforce_queue(&mgr, &cache, &QueueLimits::default(), &operator, &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert!(parked.is_empty());
        assert_eq!(all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    /// `active_limit` counts downloads and seeds together.
    #[test]
    fn active_limit_caps_seeds_too() {
        let (mgr, root) = manager("total");
        let all = seeds(&mgr);
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &limits("queueing = true\nactive_limit = 2"), &HashSet::new(), &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert_eq!(all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Three incomplete torrents, stopped, waiting for a download slot.
    fn queued_downloads(mgr: &Arc<TorrentManager>) -> Vec<Arc<typhon_engine::torrent::meta::TorrentState>> {
        ["d", "e", "f"]
            .iter()
            .map(|n| {
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", false, true).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
                t.is_paused.store(true, Ordering::Relaxed);
                t
            })
            .collect()
    }

    /// Run passes at `t0 + secs` and count the running downloads after each.
    /// A torrent the pass started is set downloading, as the engine would.
    fn passes(mgr: &Arc<TorrentManager>, all: &[Arc<typhon_engine::torrent::meta::TorrentState>], l: &QueueLimits, at: &[u64]) -> Vec<usize> {
        let t0 = std::time::Instant::now();
        let (mut parked, mut clock) = (HashSet::new(), SlowClock::new());
        at.iter()
            .map(|secs| {
                enforce_queue(mgr, &Cache::default(), l, &HashSet::new(), &Default::default(), &mut parked, &mut clock, t0 + Duration::from_secs(*secs));
                for t in all {
                    if !t.is_paused.load(Ordering::Relaxed) {
                        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
                    }
                }
                all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count()
            })
            .collect()
    }

    /// ⭐ The case it exists for: a download stuck with no seed stops holding
    /// the slot once it has been slow for the timer, keeps running, and the
    /// next one in the queue starts. Seen slow at 30 s, exempt from 90 s.
    #[test]
    fn a_slow_download_frees_its_slot_and_keeps_running() {
        let (mgr, root) = manager("slow-dl");
        let all = queued_downloads(&mgr);
        let l = limits("active_downloads = 1\ndont_count_slow_torrents = true\nslow_torrent_inactive_timer = 60");
        assert_eq!(l.slow.map(|r| (r.dl_bps, r.ul_bps)), Some((2048, 2048)), "qBittorrent's 2 KiB/s by default");
        assert_eq!(passes(&mgr, &all, &l, &[0, 30, 60, 100, 130, 200]), vec![1, 1, 1, 2, 2, 3]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Off by default, and a threshold of 0 makes nothing slow: the ceiling
    /// holds as it always has.
    #[test]
    fn without_the_switch_or_with_a_zero_threshold_the_ceiling_holds() {
        let (mgr, root) = manager("slow-off");
        let all = queued_downloads(&mgr);
        assert!(limits("active_downloads = 1").slow.is_none());
        assert_eq!(passes(&mgr, &all, &limits("active_downloads = 1"), &[0, 100, 200, 300]), vec![1, 1, 1, 1]);
        let zero = limits("active_downloads = 1\ndont_count_slow_torrents = true\nslow_torrent_dl_rate_threshold = 0");
        assert_eq!(passes(&mgr, &all, &zero, &[400, 500, 600]), vec![1, 1, 1]);
        let _ = std::fs::remove_dir_all(root);
    }

    /// Seeds too, as in qBittorrent: an idle seed holds no `active_seeds`
    /// slot, so a parked one is started in its place.
    #[test]
    fn an_idle_seed_frees_its_seed_slot() {
        let (mgr, root) = manager("slow-seed");
        let all = seeds(&mgr);
        let l = limits("queueing = true\nactive_seeds = 1\ndont_count_slow_torrents = true\nslow_torrent_inactive_timer = 60");
        let t0 = std::time::Instant::now();
        let (mut parked, mut clock) = (HashSet::new(), SlowClock::new());
        let mut run = |secs: u64| {
            enforce_queue(&mgr, &Cache::default(), &l, &HashSet::new(), &Default::default(), &mut parked, &mut clock, t0 + Duration::from_secs(secs));
            all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count()
        };
        // First pass: three running seeds, none slow long enough yet.
        assert_eq!(run(0), 1);
        assert_eq!(run(30), 1);
        assert_eq!(run(70), 2, "the oldest is idle 70 s: it keeps running, a parked seed starts");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A forced torrent runs past the ceiling and takes no slot: two
    /// downloads under `active_downloads = 1`, the forced one stopped -- it
    /// is started, and the other keeps the one slot.
    #[test]
    fn a_forced_download_runs_past_the_ceiling() {
        let (mgr, root) = manager("forced");
        let mut ts = Vec::new();
        for n in ["a", "b"] {
            let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", true, false).unwrap();
            ts.push(mgr.get(&ih).unwrap());
        }
        let forced: HashSet<String> = [typhon_engine::torrent::hex_encode(&ts[0].info_hash)].into();
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &limits("active_downloads = 1"), &HashSet::new(), &forced, &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert!(!ts[0].is_paused.load(Ordering::Relaxed), "the forced one waits its turn");
        assert!(!ts[1].is_paused.load(Ordering::Relaxed), "the forced one took the only slot");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The download ceiling alone never touches a seed.
    #[test]
    fn a_download_ceiling_leaves_the_seeds_alone() {
        let (mgr, root) = manager("dl-only");
        let all = seeds(&mgr);
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &limits("active_downloads = 1"), &HashSet::new(), &Default::default(), &mut parked, &mut SlowClock::new(), std::time::Instant::now());
        assert!(all.iter().all(|t| !t.is_paused.load(Ordering::Relaxed)));
        assert!(parked.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}

// ---------------------------------------------------------------------------
// What the workers decide, and what they leave alone
// ---------------------------------------------------------------------------

#[cfg(test)]
mod decision_tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};
    use std::collections::HashSet;
    use typhon_engine::torrent::meta::{InfoHash, TorrentState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const SIZE: u64 = 16_384;

    /// A single-file torrent, unique per name (the piece hash carries it).
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{SIZE}e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        for (i, b) in name.bytes().enumerate().take(20) {
            piece[i] = b;
        }
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn st(tag: &str, extra: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"))
    }

    fn engine_manager(s: &TestState, id: &str) -> Arc<TorrentManager> {
        s.engines.engines().iter().find(|e| e.id == id).expect("engine").manager.clone()
    }

    /// A seed on `engine`, with its file on disk and a row in the store:
    /// announcing to `tracker`, filed under `category`, `seeded_secs` served.
    fn put(
        s: &TestState,
        engine: &str,
        name: &str,
        category: &str,
        tracker: &str,
        seeded_secs: i64,
    ) -> (String, Arc<TorrentState>, std::path::PathBuf) {
        let dir = s.dir.join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(name);
        std::fs::write(&file, vec![0u8; SIZE as usize]).unwrap();
        let (hash, _) = crate::api::add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            category,
            dir.to_str().unwrap(),
            "",
            false,
            true,
            engine,
        )
        .expect("added");
        let t = engine_manager(s, engine).get(&typhon_engine::torrent::hex_decode(&hash).unwrap()).expect("held");
        {
            let mut live = t.live_trackers.write();
            live.clear();
            live.push(vec![tracker.to_string()]);
        }
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        t.is_paused.store(false, Ordering::Relaxed);
        t.seed_secs.store(seeded_secs, Ordering::Relaxed);
        t.seed_since.store(0, Ordering::Relaxed);
        (hash, t, file)
    }

    const TRACKER: &str = "https://tracker.example/announce";

    fn volume_on(s: &TestState, id: &str, total: u64, used: u64, low: i64) -> crate::volumes::Volume {
        let dev = crate::volumes::device_of_nearest(&s.dir).expect("the test dir is on a device");
        crate::volumes::Volume::measure(
            id.into(),
            dev,
            (used, total, total - used),
            None,
            0,
            0,
            3,
            crate::volumes::Policy { enabled: true, high: 90, low, inherited: true },
        )
    }

    /// A volume id that is no path: the measure after the drain finds nothing,
    /// so no TRIM is ever considered and "freed" is exactly 0.
    const NO_SUCH_MOUNT: &str = "/nonexistent-mount-for-the-drain-tests";

    fn in_store(s: &TestState, hash: &str) -> bool {
        s.store.read().unwrap().all_hashes("race").unwrap().iter().any(|h| h == hash)
            || s.store.read().unwrap().all_hashes("hoard").unwrap().iter().any(|h| h == hash)
    }

    fn history(s: &TestState) -> Vec<serde_json::Value> {
        s.store.read().unwrap().drain_history(10).unwrap()
    }

    fn write_categories(s: &TestState, doc: serde_json::Value) {
        s.store.lock().unwrap().put_meta("categories", &doc.to_string()).unwrap();
    }

    // ---- the drain: what it frees, in what order, and what it will not touch ----

    /// ⭐ Over its watermark, a volume is brought back to the LOW one and no
    /// further: one torrent here frees more than the gap, so exactly one goes
    /// -- data, store row and engine copy together.
    #[test]
    fn the_drain_removes_only_what_the_gap_to_the_low_watermark_needs() {
        let s = st("dec-drain-gap", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let mgr = engine_manager(&s, "race");
        let all: Vec<_> = ["da", "db", "dc"].iter().map(|n| put(&s, "race", n, "", TRACKER, 100)).collect();
        // 90 000 of 100 000 used, low = 80: 10 000 to free, a torrent is 16 384.
        let v = volume_on(&s, NO_SUCH_MOUNT, 100_000, 90_000, 80);
        let out = drain_once(&s.state, &mgr, &v, &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (1, 0, 0));
        assert_eq!(out.freed_bytes, 0, "the disk delta is measured, not assumed: this id is no mount");
        assert_eq!(mgr.all().len(), 2);
        let gone: Vec<_> = all.iter().filter(|(h, _, _)| mgr.get(&typhon_engine::torrent::hex_decode(h).unwrap()).is_none()).collect();
        assert_eq!(gone.len(), 1);
        let (hash, _, file) = gone[0];
        assert!(!in_store(&s, hash), "the store row went with the torrent");
        assert!(!file.exists(), "and its payload, since this was its last copy");
        for (h, _, f) in all.iter().filter(|(h, _, _)| h != hash) {
            assert!(f.exists() && in_store(&s, h), "the others are untouched");
        }
        let hist = history(&s);
        assert_eq!(hist.len(), 1);
        assert_eq!((hist[0]["deleted"].clone(), hist[0]["graduated"].clone(), hist[0]["stuck"].clone()), (serde_json::json!(1), serde_json::json!(0), serde_json::json!(0)));
        assert_eq!(hist[0]["volume"], NO_SUCH_MOUNT);
    }

    /// A bigger gap takes more torrents: two (32 768) do not cover 40 000,
    /// three do.
    #[test]
    fn a_bigger_gap_takes_more_torrents() {
        let s = st("dec-drain-big", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let mgr = engine_manager(&s, "race");
        for n in ["ea", "eb", "ec"] {
            put(&s, "race", n, "", TRACKER, 100);
        }
        // 40 000 to free: two torrents (32 768) are not enough, three are.
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 80_000, 40), &s.cfg(), "race");
        assert_eq!(out.deleted, 3);
        assert!(mgr.all().is_empty());
    }

    /// The torrents of another disk are not candidates, however full this
    /// one is.
    #[test]
    fn a_torrent_on_another_device_is_not_a_candidate() {
        let s = st("dec-drain-dev", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let mgr = engine_manager(&s, "race");
        let (hash, _, file) = put(&s, "race", "fa", "", TRACKER, 100);
        let mut v = volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80);
        v.dev = v.dev.wrapping_add(1);
        let out = drain_once(&s.state, &mgr, &v, &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (0, 0, 0));
        assert!(file.exists() && in_store(&s, &hash));
        assert!(history(&s).is_empty(), "a pass that did nothing writes no history");
    }

    /// A torrent that still owes seeding time AND has nowhere to go is
    /// neither deleted nor moved: it is counted as stuck, and the history
    /// says so.
    #[test]
    fn a_torrent_owing_time_with_no_graduation_target_is_stuck_not_deleted() {
        let s = st("dec-drain-stuck", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"72\"\n");
        let mgr = engine_manager(&s, "race");
        let (hash, _, file) = put(&s, "race", "ga", "", TRACKER, 3600);
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80), &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (0, 0, 1));
        assert!(file.exists() && in_store(&s, &hash), "never deleted for want of somewhere to put it");
        assert_eq!(history(&s)[0]["stuck"], 1);
    }

    fn graduation_state(tag: &str, hours: &str, to_mode: &str) -> TestState {
        let s = st(tag, &format!("\n[announce_min_seed_hours]\n\"tracker.example\" = \"{hours}\"\n"));
        let pool = s.dir.join("pool");
        std::fs::create_dir_all(&pool).unwrap();
        write_categories(
            &s,
            serde_json::json!({
                "ssd": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "race", "graduate_to": "pool"},
                "pool": {"save_path": pool.to_str().unwrap(), "mode": to_mode},
            }),
        );
        s
    }

    /// ⭐ The fate is the torrent's own: owing time, it LEAVES the disk by
    /// moving to its graduation target. A job is queued and charged to the
    /// budget, the data stays until the copy is done, and the next pass does
    /// not queue it twice.
    #[test]
    fn a_torrent_owing_time_graduates_instead_of_being_deleted() {
        let s = graduation_state("dec-drain-grad", "72", "hoard");
        let mgr = engine_manager(&s, "race");
        let (hash, _, file) = put(&s, "race", "ha", "ssd", TRACKER, 3600);
        let v = volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80);
        let out = drain_once(&s.state, &mgr, &v, &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (0, 1, 0));
        assert!(file.exists() && in_store(&s, &hash), "nothing is deleted by a graduation");
        assert!(s.store.read().unwrap().job_pending_for("graduate", &hash), "the move is queued");
        let jobs = s.store.read().unwrap().list_jobs(10).unwrap();
        let job = jobs.iter().find(|j| j.info_hash == hash).expect("the job");
        let params: serde_json::Value = serde_json::from_str(&job.params).unwrap();
        assert_eq!((job.kind.as_str(), job.total_bytes), ("graduate", SIZE as i64));
        assert_eq!(params["to_engine"], "hoard");
        assert_eq!(params["to_category"], "pool");
        assert_eq!(params["from_engine"], "race");

        let again = drain_once(&s.state, &mgr, &v, &s.cfg(), "race");
        assert_eq!((again.graduated, again.deleted), (0, 0), "a duplicate is refused, not queued again");
        assert_eq!(history(&s)[0]["graduated"], 1);
    }

    /// A graduation target that files under the SAME engine moves nothing,
    /// so it is counted as stuck instead of being queued forever.
    #[test]
    fn a_graduation_back_onto_the_same_engine_is_stuck() {
        let s = graduation_state("dec-drain-same", "72", "race");
        let mgr = engine_manager(&s, "race");
        let (hash, _, _) = put(&s, "race", "ia", "ssd", TRACKER, 3600);
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80), &s.cfg(), "race");
        assert_eq!((out.graduated, out.stuck, out.deleted), (0, 1, 0));
        assert!(!s.store.read().unwrap().job_pending_for("graduate", &hash));
    }

    /// Files of the same name already at the destination: the job would
    /// refuse, so the drain does not queue it every tick.
    #[test]
    fn a_graduation_onto_existing_files_is_stuck_not_queued() {
        let s = graduation_state("dec-drain-taken", "72", "hoard");
        let mgr = engine_manager(&s, "race");
        let (hash, _, file) = put(&s, "race", "ja", "ssd", TRACKER, 3600);
        std::fs::write(s.dir.join("pool").join("ja"), b"someone else's bytes").unwrap();
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80), &s.cfg(), "race");
        assert_eq!((out.graduated, out.stuck, out.deleted), (0, 1, 0));
        assert!(!s.store.read().unwrap().job_pending_for("graduate", &hash));
        assert!(file.exists());
        assert_eq!(std::fs::read(s.dir.join("pool").join("ja")).unwrap(), b"someone else's bytes");
    }

    /// The target engine already holds the very torrent (it sits in both): the
    /// job would refuse it, so it is not queued -- it used to be copied over
    /// and back every tick -- and it is counted as stuck, so "cannot free"
    /// is said.
    #[test]
    fn a_graduation_the_target_engine_already_holds_is_stuck_not_queued() {
        let s = graduation_state("dec-drain-held", "72", "hoard");
        let mgr = engine_manager(&s, "race");
        let (hash, _, file) = put(&s, "race", "la", "ssd", TRACKER, 3600);
        let elsewhere = s.dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let hoard = engine_manager(&s, "hoard");
        hoard.add_torrent_bytes(&torrent_bytes("la"), elsewhere.to_str().unwrap(), true, true).unwrap();
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        assert!(hoard.get(&ih).is_some(), "the target holds it");

        let v = volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80);
        for _ in 0..2 {
            let out = drain_once(&s.state, &mgr, &v, &s.cfg(), "race");
            assert_eq!((out.graduated, out.stuck, out.deleted), (0, 1, 0));
            assert!(!s.store.read().unwrap().job_pending_for("graduate", &hash));
        }
        assert!(file.exists() && in_store(&s, &hash));
        assert!(mgr.get(&ih).is_some(), "still in race");
        assert!(hoard.get(&ih).is_some(), "and untouched in hoard");
    }

    /// Mixed fates in one pass: the one that has paid is deleted, the one that
    /// has not graduates.
    #[test]
    fn one_pass_deletes_what_has_paid_and_graduates_what_has_not() {
        let s = graduation_state("dec-drain-mixed", "1", "hoard");
        let mgr = engine_manager(&s, "race");
        let (paid, _, paid_file) = put(&s, "race", "ka", "ssd", TRACKER, 7200);
        let (owing, _, owing_file) = put(&s, "race", "kb", "ssd", TRACKER, 60);
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 0), &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (1, 1, 0));
        assert!(!in_store(&s, &paid) && !paid_file.exists());
        assert!(in_store(&s, &owing) && owing_file.exists());
        assert!(s.store.read().unwrap().job_pending_for("graduate", &owing));
    }

    // ---- the workers on a timer ---------------------------------------------------

    /// Let the spawned task run up to its next await, advance the paused clock,
    /// and let it run again.
    async fn after(secs: u64) {
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(secs)).await;
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
    }

    /// Wait (in real time) for work that runs on the blocking pool.
    async fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
            tokio::task::yield_now().await;
        }
        panic!("never happened: {what}");
    }

    fn alive_tasks() -> usize {
        tokio::runtime::Handle::current().metrics().num_alive_tasks()
    }

    fn queued_downloads(mgr: &Arc<TorrentManager>, names: &[&str]) -> Vec<Arc<TorrentState>> {
        names
            .iter()
            .map(|n| {
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", true, false).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
                t.is_paused.store(true, Ordering::Relaxed);
                t
            })
            .collect()
    }

    fn running(ts: &[Arc<TorrentState>]) -> usize {
        ts.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count()
    }

    /// ⭐ The slot manager waits for the engine to settle, then enforces the
    /// ceiling every 30 s from the LIVE config: raising it in the settings
    /// applies on the next pass.
    #[tokio::test(start_paused = true)]
    async fn the_slot_manager_enforces_the_live_ceiling_every_pass() {
        let s = st("dec-slots", "\n[race]\nlisten_port = 16171\nactive_downloads = 1\n");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["sa", "sb", "sc"]);
        spawn_download_slots(mgr.clone(), Arc::new(Cache::default()), s.config.clone(), s.store.clone(), "race".into());
        after(29).await;
        assert_eq!(running(&ts), 0, "nothing moves while the engine settles");
        after(2).await; // 31 s: settled, the loop's first sleep started
        after(30).await; // the first pass
        assert_eq!(running(&ts), 1, "one slot");

        let raised: crate::config::Config =
            toml::from_str(&format!("[daemon]\napi_key = \"{KEY}\"\n[race]\nlisten_port = 16171\nactive_downloads = 3\n")).unwrap();
        *s.config.write().unwrap() = Arc::new(raised);
        after(30).await;
        assert_eq!(running(&ts), 3, "the next pass reads the new ceiling");
    }

    /// An operator's stop is not the queue's to undo, pass after pass.
    #[tokio::test(start_paused = true)]
    async fn the_slot_manager_never_starts_what_the_operator_stopped() {
        let s = st("dec-slots-paused", "\n[race]\nlisten_port = 16171\nactive_downloads = 2\n");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["pa", "pb", "pc"]);
        let stopped = typhon_engine::torrent::hex_encode(&ts[0].info_hash);
        s.store.lock().unwrap().insert_torrent(&stopped, "race", b"x", "/tmp", "", 0.0, true, "").unwrap();
        s.store.lock().unwrap().set_paused(&stopped, "race", true).unwrap();
        spawn_download_slots(mgr.clone(), Arc::new(Cache::default()), s.config.clone(), s.store.clone(), "race".into());
        after(31).await;
        after(30).await;
        after(30).await;
        assert!(ts[0].is_paused.load(Ordering::Relaxed), "still stopped");
        assert_eq!(running(&ts[1..]), 2, "the two others have the two slots");
    }

    /// With no ceiling configured the loop wakes and does nothing.
    #[tokio::test(start_paused = true)]
    async fn with_no_ceiling_the_slot_manager_touches_nothing() {
        let s = st("dec-slots-none", "");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["na", "nb"]);
        spawn_download_slots(mgr.clone(), Arc::new(Cache::default()), s.config.clone(), s.store.clone(), "race".into());
        after(31).await;
        after(30).await;
        after(30).await;
        assert_eq!(running(&ts), 0);
    }

    /// The stagger start resumes what the operator did not stop, and leaves
    /// the stopped ones stopped.
    #[tokio::test]
    async fn the_stagger_start_keeps_a_stopped_torrent_stopped() {
        let (mgr, root) = {
            let s = st("dec-stagger", "");
            // the state dir is removed on drop; build a manager that outlives it
            let root = std::env::temp_dir().join(format!("hydra-stagger-{}", std::process::id()));
            let (data, resume) = (root.join("data"), root.join("resume"));
            std::fs::create_dir_all(&data).unwrap();
            std::fs::create_dir_all(&resume).unwrap();
            drop(s);
            (
                Arc::new(TorrentManager::new(
                    data.to_string_lossy().into_owned(),
                    resume.to_string_lossy().into_owned(),
                    Arc::new(typhon_engine::disk::DiskManager::new(16)),
                )),
                root,
            )
        };
        let (ih_stopped, _) = mgr.add_torrent_bytes(&torrent_bytes("stopped"), "/tmp", true, false).unwrap();
        let (ih_a, _) = mgr.add_torrent_bytes(&torrent_bytes("wa"), "/tmp", false, false).unwrap();
        let (ih_b, _) = mgr.add_torrent_bytes(&torrent_bytes("wb"), "/tmp", false, false).unwrap();
        for ih in [&ih_a, &ih_b] {
            mgr.get(ih).unwrap().is_paused.store(false, Ordering::Relaxed);
        }
        spawn_stagger_start(mgr.clone());
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(mgr.get(&ih_stopped).unwrap().is_paused.load(Ordering::Relaxed), "the operator's stop survives a restart");
        for ih in [&ih_a, &ih_b] {
            assert!(!mgr.get(ih).unwrap().is_paused.load(Ordering::Relaxed));
        }
        // An empty engine is a clean no-op, not a panic.
        let empty = Arc::new(TorrentManager::new(
            root.join("data").to_string_lossy().into_owned(),
            root.join("resume2").to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        spawn_stagger_start(empty);
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// The verify throttle is a boot job: it ends as soon as nothing is
    /// checking, and keeps ticking while something is.
    #[tokio::test(start_paused = true)]
    async fn the_verify_throttle_ends_when_nothing_is_left_to_check() {
        let s = st("dec-verify", "");
        let mgr = engine_manager(&s, "race");
        let base = alive_tasks();
        spawn_verify_throttle(mgr.clone());
        after(0).await;
        assert_eq!(alive_tasks(), base + 1);
        after(5).await;
        after(10).await;
        assert_eq!(alive_tasks(), base, "nothing was checking: it stopped");

        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes("checking"), "/tmp", true, false).unwrap();
        mgr.get(&ih).unwrap().status.store(TorrentStatus::Checking as u8, Ordering::Relaxed);
        spawn_verify_throttle(mgr.clone());
        after(0).await;
        after(5).await;
        for _ in 0..3 {
            after(10).await;
        }
        assert_eq!(alive_tasks(), base + 1, "a torrent is still checking: it keeps watching");
        mgr.get(&ih).unwrap().status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        after(10).await;
        assert_eq!(alive_tasks(), base);
    }

    /// ⭐ A background loop ends when the stop begins, wherever it is in its
    /// wait: it does not sit out the minutes until its next pass while the
    /// departures are sent and the resume state is written. The signal is the
    /// test's own, not the process-wide one other tests share.
    #[tokio::test(start_paused = true)]
    async fn a_background_loop_ends_when_the_stop_begins() {
        let s = st("dec-verify-stop", "");
        let mgr = engine_manager(&s, "race");
        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes("checking-stop"), "/tmp", true, false).unwrap();
        mgr.get(&ih).unwrap().status.store(TorrentStatus::Checking as u8, Ordering::Relaxed);
        let base = alive_tasks();
        let stop = typhon_engine::stop::Stop::new();

        // In its first wait.
        spawn_verify_throttle_until(mgr.clone(), stop.clone());
        after(0).await;
        assert_eq!(alive_tasks(), base + 1);
        stop.begin();
        after(0).await;
        assert_eq!(alive_tasks(), base, "the loop outlived the stop in its first wait");

        // And in the wait between two passes, with a torrent still checking.
        let stop = typhon_engine::stop::Stop::new();
        spawn_verify_throttle_until(mgr.clone(), stop.clone());
        after(0).await;
        after(5).await;
        after(10).await;
        assert_eq!(alive_tasks(), base + 1, "a torrent is still checking: it keeps watching");
        stop.begin();
        after(0).await;
        assert_eq!(alive_tasks(), base, "the loop outlived the stop between two passes");
    }

    /// No ceiling, no watcher.
    #[tokio::test]
    async fn a_memory_ceiling_of_zero_starts_no_watcher() {
        let base = alive_tasks();
        spawn_memory_watch(0);
        tokio::task::yield_now().await;
        assert_eq!(alive_tasks(), base);
        spawn_memory_watch(1 << 30);
        tokio::task::yield_now().await;
        assert_eq!(alive_tasks(), base + 1);
    }

    /// The start-up line says whether the memory watch is on, and a ceiling
    /// starts it; no ceiling starts nothing.
    #[tokio::test]
    async fn the_memory_watch_says_at_startup_whether_it_is_on() {
        let log = capture();
        let base = alive_tasks();
        assert!(!start_memory_watch(0));
        tokio::task::yield_now().await;
        assert_eq!(alive_tasks(), base, "no ceiling, no watcher");
        let off = lines(&log, "memory alert");
        assert_eq!(off.len(), 1, "{off:?}");
        assert!(off[0].starts_with("INFO") && off[0].contains("off"), "{off:?}");

        assert!(start_memory_watch(1536 << 20));
        tokio::task::yield_now().await;
        assert_eq!(alive_tasks(), base + 1, "a ceiling starts the watcher");
        let on = lines(&log, "memory alert");
        assert_eq!(on.len(), 2, "{on:?}");
        assert!(on[1].starts_with("INFO") && on[1].contains("1536 MiB"), "{on:?}");
    }

    /// ⭐ The daily sync copies the engine's seeding time into the store's
    /// fallback column, in chunks, once per day.
    #[tokio::test(start_paused = true)]
    async fn the_seed_time_sync_copies_the_engines_figure_into_the_store() {
        let s = st("dec-seedsync", "");
        let mgr = engine_manager(&s, "race");
        let (a, ta, _) = put(&s, "race", "ya", "", TRACKER, 1234);
        let (b, _, _) = put(&s, "race", "yb", "", TRACKER, 99);
        let seeded = |h: &str| s.store.read().unwrap().workflow_facts_of("race", h).unwrap().unwrap().seeding_time;
        assert_eq!((seeded(&a), seeded(&b)), (0, 0));
        spawn_seed_time_sync(mgr.clone(), s.store.clone(), "race".into());
        after(3599).await;
        assert_eq!(seeded(&a), 0, "not at boot: the first pass waits an hour");
        after(2).await;
        for _ in 0..5 {
            after(1).await;
        }
        assert_eq!((seeded(&a), seeded(&b)), (1234, 99));

        // A day later only what moved is written again.
        ta.seed_secs.store(5000, Ordering::Relaxed);
        after(24 * 3600).await;
        for _ in 0..5 {
            after(1).await;
        }
        assert_eq!((seeded(&a), seeded(&b)), (5000, 99));
    }

    /// ⭐⭐ The transit sweep is scoped to graduation TARGETS: a torrent there
    /// that has paid its time goes, with its files; one still owing, one on an
    /// undeclared tracker, and one in an ordinary library category stay.
    #[tokio::test(start_paused = true)]
    async fn the_transit_sweep_deletes_only_paid_torrents_in_a_transit_category() {
        let s = st("dec-transit", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"1\"\n");
        let mgr = engine_manager(&s, "hoard");
        write_categories(
            &s,
            serde_json::json!({
                "ssd": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "race", "graduate_to": "transit"},
                "transit": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "hoard", "transit": true},
                "library": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "hoard"},
            }),
        );
        let (paid, _, paid_file) = put(&s, "hoard", "ta", "transit", TRACKER, 7200);
        let (owing, _, owing_file) = put(&s, "hoard", "tb", "transit", TRACKER, 60);
        let (undeclared, _, undeclared_file) = put(&s, "hoard", "tc", "transit", "https://other.example/announce", 999_999);
        let (library, _, library_file) = put(&s, "hoard", "td", "library", TRACKER, 999_999);
        spawn_transit_sweep(s.state.clone(), mgr.clone(), s.config.clone(), "hoard".into());
        after(89).await;
        assert!(in_store(&s, &paid), "not before its first delay");
        after(2).await;
        assert!(!in_store(&s, &paid) && !paid_file.exists(), "paid and in transit: gone, files too");
        assert!(mgr.get(&typhon_engine::torrent::hex_decode(&paid).unwrap()).is_none());
        for (h, f) in [(&owing, &owing_file), (&undeclared, &undeclared_file), (&library, &library_file)] {
            assert!(in_store(&s, h) && f.exists(), "{h} must stay");
        }
    }

    /// With no transit category nothing is even looked at.
    #[tokio::test(start_paused = true)]
    async fn the_transit_sweep_does_nothing_without_a_transit_category() {
        let s = st("dec-transit-none", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let mgr = engine_manager(&s, "hoard");
        let (h, _, f) = put(&s, "hoard", "ua", "", TRACKER, 999_999);
        spawn_transit_sweep(s.state.clone(), mgr, s.config.clone(), "hoard".into());
        after(91).await;
        after(300).await;
        assert!(in_store(&s, &h) && f.exists());
    }

    /// ⭐ The race drain, whole: the timer finds the volume under the engine's
    /// torrents, sees it over ITS watermark, and drains it -- only when that
    /// volume's policy is on.
    #[tokio::test(start_paused = true)]
    async fn the_race_drain_worker_drains_an_enabled_volume_and_spares_a_disabled_one() {
        let s = st(
            "dec-drain-worker",
            "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n[race_drain]\ncheck_interval_seconds = 60\n",
        );
        let mgr = engine_manager(&s, "race");
        let (h, _, f) = put(&s, "race", "va", "", TRACKER, 100);
        let mount = crate::volumes::mount_point_of(&s.dir).to_string_lossy().to_string();
        let off = crate::volumes::Policy { enabled: false, high: 0, low: 0, inherited: false };
        crate::volumes::save_policy(&s.state, &mount, &off).unwrap();
        spawn_race_drain(s.state.clone(), mgr.clone(), s.config.clone(), "race".into());
        after(11).await;
        after(60).await;
        std::thread::sleep(Duration::from_millis(200));
        after(0).await;
        assert!(in_store(&s, &h) && f.exists(), "a disabled volume is never drained");

        let on = crate::volumes::Policy { enabled: true, high: 0, low: 0, inherited: false };
        crate::volumes::save_policy(&s.state, &mount, &on).unwrap();
        after(60).await;
        eventually("the drain removed the torrent", || {
            std::thread::sleep(Duration::from_millis(10));
            mgr.all().is_empty()
        })
        .await;
        assert!(!in_store(&s, &h));
        assert!(!f.exists());
        assert!(history(&s).iter().any(|r| r["deleted"] == 1 && r["volume"] == mount.as_str()), "{:?}", history(&s));
    }

    /// ⭐ The share-limit worker, on its timer: after its delay it stops a
    /// seed past the engine's ratio and leaves the others running.
    #[tokio::test(start_paused = true)]
    async fn the_share_limit_worker_applies_the_engines_ratio_on_its_timer() {
        let s = st(
            "dec-sharelimit",
            "\n[race]\nlisten_port = 16171\nmax_ratio = 2.0\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n",
        );
        let mgr = engine_manager(&s, "race");
        let (over, t_over, _) = put(&s, "race", "wa", "", TRACKER, 0);
        let (_, t_under, _) = put(&s, "race", "wb", "", TRACKER, 0);
        t_over.total_uploaded.store(3 * SIZE, Ordering::Relaxed);
        t_under.total_uploaded.store(SIZE, Ordering::Relaxed);
        spawn_share_limits(s.state.clone(), mgr.clone(), s.config.clone(), "race".into());
        after(89).await;
        assert!(!t_over.is_paused.load(Ordering::Relaxed), "not before SETTLE + 60 s");
        after(2).await;
        eventually("the pass stopped the seed over its ratio", || t_over.is_paused.load(Ordering::Relaxed)).await;
        assert!(!t_under.is_paused.load(Ordering::Relaxed));
        assert!(s.store.read().unwrap().paused_hashes("race").unwrap().contains(&over), "as the operator's own stop");
    }

    // ---- the store reconcile ----------------------------------------------------

    /// A row the store holds for a torrent the engine does not, written by an
    /// EARLIER process (so not inside the grace period).
    fn orphan(s: &TestState, engine: &str, hash: &str) {
        s.store.lock().unwrap().execute_batch_for_tests(&format!(
            "INSERT INTO torrents (info_hash, session, torrent) VALUES ('{hash}', '{engine}', x'00');"
        ));
    }

    fn rows(s: &TestState, engine: &str) -> Vec<String> {
        s.store.read().unwrap().all_hashes(engine).unwrap()
    }

    fn live_one(s: &TestState, engine: &str, name: &str) -> String {
        let mgr = engine_manager(s, engine);
        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true).unwrap();
        typhon_engine::torrent::hex_encode(&ih)
    }

    const O1: &str = "1111111111111111111111111111111111111111";
    const O2: &str = "2222222222222222222222222222222222222222";

    /// A row whose torrent is gone is dropped; one the engine holds is not;
    /// and an engine that holds NOTHING is never trusted to say what is gone.
    #[test]
    fn the_reconcile_drops_rows_without_a_torrent_but_never_for_an_empty_engine() {
        let s = st("dec-recon", "");
        let live = live_one(&s, "race", "ra");
        s.store.lock().unwrap().insert_torrent(&live, "race", b"x", "/tmp", "", 0.0, false, "").unwrap();
        orphan(&s, "race", O1);
        orphan(&s, "hoard", O2);
        reconcile_pass(&s.engines, &s.store);
        assert_eq!(rows(&s, "race"), vec![live.clone()], "the orphan went, the live row stayed");
        assert_eq!(rows(&s, "hoard"), vec![O2.to_string()], "the hoard holds no torrent: nothing may be concluded");
    }

    /// A row this process wrote moments ago may be an add still on its way to
    /// the engine: it is spared, even though the engine does not hold it yet.
    #[test]
    fn a_row_written_a_moment_ago_is_spared() {
        let s = st("dec-recon-fresh", "");
        live_one(&s, "race", "rb");
        s.store.lock().unwrap().insert_torrent(O1, "race", b"x", "/tmp", "", 0.0, false, "").unwrap();
        orphan(&s, "race", O2);
        reconcile_pass(&s.engines, &s.store);
        let left = rows(&s, "race");
        assert!(left.contains(&O1.to_string()), "fresh: spared");
        assert!(!left.contains(&O2.to_string()), "old: dropped");
    }

    /// A torrent being moved is out of its engine on purpose; its row holds
    /// the only metainfo the target will adopt it with.
    #[test]
    fn a_row_with_a_job_in_flight_is_spared() {
        let s = st("dec-recon-job", "");
        live_one(&s, "race", "rc");
        orphan(&s, "race", O1);
        orphan(&s, "race", O2);
        s.store.lock().unwrap().create_job("graduate", O1, "{}", 1).unwrap();
        reconcile_pass(&s.engines, &s.store);
        let left = rows(&s, "race");
        assert!(left.contains(&O1.to_string()), "being moved");
        assert!(!left.contains(&O2.to_string()));
    }

    /// A library that mostly failed to load looks exactly like a mass
    /// deletion, and this worker cannot tell the two apart: past the ceiling
    /// it drops nothing.
    #[test]
    fn the_reconcile_refuses_to_drop_a_flood_of_rows() {
        let s = st("dec-recon-flood", "");
        live_one(&s, "race", "rd");
        let hashes: Vec<String> = (0..60).map(|i| format!("{i:040x}")).collect();
        for h in &hashes {
            orphan(&s, "race", h);
        }
        reconcile_pass(&s.engines, &s.store);
        assert_eq!(rows(&s, "race").len(), 60, "60 of 60 orphans is over the ceiling of 50: all kept");
        // Under the ceiling they go.
        let s2 = st("dec-recon-few", "");
        live_one(&s2, "race", "re");
        for h in &hashes[..10] {
            orphan(&s2, "race", h);
        }
        reconcile_pass(&s2.engines, &s2.store);
        assert!(rows(&s2, "race").is_empty());
    }

    /// The timer runs a pass at once, then every five minutes.
    #[tokio::test]
    async fn the_reconcile_worker_runs_a_pass_as_soon_as_it_starts() {
        let s = st("dec-recon-worker", "");
        live_one(&s, "race", "rf");
        orphan(&s, "race", O1);
        spawn_store_reconcile(s.engines.clone(), s.store.clone());
        eventually("the first tick dropped the orphan", || rows(&s, "race").is_empty()).await;
    }

    // ---- the queue: the branches the happy paths do not reach -----------------------

    fn seeders(c: &Cache, t: &TorrentState, n: i64) {
        c.record(
            &typhon_engine::torrent::hex_encode(&t.info_hash),
            crate::announce::cache::Entry {
                complete: n,
                incomplete: 0,
                tracker: "tracker.example".into(),
                at: std::time::Instant::now(),
                interval: Duration::from_secs(1800),
            },
        );
    }

    fn now() -> std::time::Instant {
        std::time::Instant::now()
    }

    fn lim(toml_src: &str) -> QueueLimits {
        QueueLimits::from_session(&toml::from_str(toml_src).unwrap())
    }

    /// Over the ceiling, the torrent with the FEWEST seeders in the swarm
    /// gives its slot back -- it keeps its progress and waits.
    #[test]
    fn over_the_ceiling_the_least_seeded_running_download_is_parked() {
        let s = st("dec-queue-over", "");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["qa", "qb", "qc"]);
        let cache = Cache::default();
        for (t, n) in ts.iter().zip([9, 5, 1]) {
            t.is_paused.store(false, Ordering::Relaxed);
            seeders(&cache, t, n);
        }
        enforce_queue(&mgr, &cache, &lim("active_downloads = 2"), &HashSet::new(), &HashSet::new(), &mut HashSet::new(), &mut SlowClock::new(), now());
        assert_eq!(ts.iter().map(|t| t.is_paused.load(Ordering::Relaxed)).collect::<Vec<_>>(), vec![false, false, true]);
        assert_eq!(ts[2].status.load(Ordering::Relaxed), TorrentStatus::Stopped as u8);
    }

    /// And the free slot goes to the best-seeded waiting torrent.
    #[test]
    fn a_free_slot_goes_to_the_best_seeded_waiting_download() {
        let s = st("dec-queue-best", "");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["qd", "qe", "qf"]);
        let cache = Cache::default();
        for (t, n) in ts.iter().zip([2, 40, 7]) {
            seeders(&cache, t, n);
        }
        enforce_queue(&mgr, &cache, &lim("active_downloads = 1"), &HashSet::new(), &HashSet::new(), &mut HashSet::new(), &mut SlowClock::new(), now());
        assert_eq!(ts.iter().map(|t| !t.is_paused.load(Ordering::Relaxed)).collect::<Vec<_>>(), vec![false, true, false]);
    }

    /// A torrent in error is out of the queue: it takes no slot and is not
    /// started.
    #[test]
    fn a_torrent_in_error_neither_takes_a_slot_nor_is_started() {
        let s = st("dec-queue-err", "");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["qg", "qh"]);
        ts[0].status.store(TorrentStatus::Error as u8, Ordering::Relaxed);
        let cache = Cache::default();
        seeders(&cache, &ts[0], 1000);
        enforce_queue(&mgr, &cache, &lim("active_downloads = 1"), &HashSet::new(), &HashSet::new(), &mut HashSet::new(), &mut SlowClock::new(), now());
        assert!(ts[0].is_paused.load(Ordering::Relaxed), "the errored one stays as it was");
        assert!(!ts[1].is_paused.load(Ordering::Relaxed), "the slot went to the healthy one");
    }

    /// `active_limit` is shared: downloads take their slots first and the
    /// seeds get what is left.
    #[test]
    fn the_total_ceiling_gives_seeds_what_the_downloads_leave() {
        let s = st("dec-queue-total", "");
        let mgr = engine_manager(&s, "race");
        let dl = queued_downloads(&mgr, &["qi"]);
        dl[0].is_paused.store(false, Ordering::Relaxed);
        let seeds: Vec<_> = ["qj", "qk"]
            .iter()
            .map(|n| {
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", false, true).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                t.is_paused.store(false, Ordering::Relaxed);
                t
            })
            .collect();
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &lim("queueing = true\nactive_limit = 2"), &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert!(!dl[0].is_paused.load(Ordering::Relaxed), "the download keeps its slot");
        assert_eq!(running(&seeds), 1, "one slot left for the seeds");
        assert_eq!(parked.len(), 1, "and the other is parked by the queue, to be resumed by it");
        let stopped = seeds.iter().find(|t| t.is_paused.load(Ordering::Relaxed)).unwrap();
        assert!(parked.contains(&stopped.info_hash));
    }

    /// A parked seed that left the catalogue is forgotten, not remembered
    /// forever.
    #[test]
    fn a_parked_seed_that_was_removed_is_forgotten() {
        let s = st("dec-queue-forget", "");
        let mgr = engine_manager(&s, "race");
        let seeds: Vec<_> = ["ql", "qm", "qn"]
            .iter()
            .map(|n| {
                // In the test's own directory: the removal below deletes files.
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), s.dir.to_str().unwrap(), false, true).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                t.is_paused.store(false, Ordering::Relaxed);
                t
            })
            .collect();
        let mut parked = HashSet::new();
        let l = lim("queueing = true\nactive_seeds = 1");
        enforce_queue(&mgr, &Cache::default(), &l, &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert_eq!(parked.len(), 2);
        let gone = *parked.iter().next().unwrap();
        mgr.remove_torrent(&gone, true).unwrap();
        enforce_queue(&mgr, &Cache::default(), &l, &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert_eq!(parked.len(), 1);
        assert!(!parked.contains(&gone));
        assert_eq!(running(&seeds) + 1, 2, "the removed one is no longer counted");
    }

    /// A forced torrent that was parked is started and is no longer the
    /// queue's to resume.
    #[test]
    fn forcing_a_parked_seed_starts_it_and_forgets_it() {
        let s = st("dec-queue-force", "");
        let mgr = engine_manager(&s, "race");
        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes("qo"), "/tmp", true, true).unwrap();
        let t = mgr.get(&ih).unwrap();
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        let mut parked: HashSet<InfoHash> = [ih].into();
        let forced: HashSet<String> = [typhon_engine::torrent::hex_encode(&ih)].into();
        enforce_queue(&mgr, &Cache::default(), &lim("queueing = true\nactive_seeds = 1"), &HashSet::new(), &forced, &mut parked, &mut SlowClock::new(), now());
        assert!(!t.is_paused.load(Ordering::Relaxed));
        assert!(parked.is_empty());
    }

    // ---- the queue's small rules -------------------------------------------------

    /// What counts as a slow torrent: under the threshold for at least the
    /// timer; the clock starts when it is first seen under it, and a torrent
    /// back over the threshold is forgotten.
    #[test]
    fn a_torrent_is_slow_only_after_the_timer_and_forgotten_when_it_speeds_up() {
        let ih: InfoHash = [7u8; 20];
        let t0 = now();
        let after = Duration::from_secs(60);
        let mut next = SlowClock::new();
        // First sight under the threshold: noted, not slow yet.
        assert!(!slow_long_enough(after, 2048, 100, &ih, &SlowClock::new(), &mut next, t0));
        assert_eq!(next.get(&ih), Some(&t0));
        // Under it since t0: 59 s is not enough, 60 s is.
        let prev = next.clone();
        let mut n2 = SlowClock::new();
        assert!(!slow_long_enough(after, 2048, 100, &ih, &prev, &mut n2, t0 + Duration::from_secs(59)));
        assert_eq!(n2.get(&ih), Some(&t0), "the clock keeps its start");
        assert!(slow_long_enough(after, 2048, 100, &ih, &prev, &mut n2, t0 + Duration::from_secs(60)));
        // At or over the threshold: not slow, and not carried forward.
        let mut n3 = SlowClock::new();
        assert!(!slow_long_enough(after, 2048, 2048, &ih, &prev, &mut n3, t0 + Duration::from_secs(600)));
        assert!(n3.is_empty());
        // A zero timer makes it slow at first sight.
        assert!(slow_long_enough(Duration::ZERO, 2048, 0, &ih, &SlowClock::new(), &mut SlowClock::new(), t0));
    }

    #[test]
    fn the_slow_rule_reads_its_thresholds_in_kib_and_defaults_the_timer() {
        let l = lim("dont_count_slow_torrents = true");
        let rule = l.slow.expect("on");
        assert_eq!((rule.dl_bps, rule.ul_bps, rule.after), (2048, 2048, Duration::from_secs(60)));
        let l = lim("dont_count_slow_torrents = true\nslow_torrent_dl_rate_threshold = 10\nslow_torrent_ul_rate_threshold = -5\nslow_torrent_inactive_timer = -1");
        let rule = l.slow.unwrap();
        assert_eq!((rule.dl_bps, rule.ul_bps, rule.after), (10 * 1024, 0, Duration::ZERO), "a negative figure is none");
        assert!(lim("dont_count_slow_torrents = false").slow.is_none());
        // Slow torrents do not make a ceiling: only the three caps decide is_empty.
        assert!(lim("dont_count_slow_torrents = true").is_empty());
    }

    /// Whether a torrent holds all of its data, as far as the status and the
    /// picker can tell.
    #[test]
    fn a_torrent_holds_everything_by_status_then_by_its_picker() {
        let s = st("dec-holds", "");
        let mgr = engine_manager(&s, "race");
        let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes("hold"), "/tmp", true, true).unwrap();
        let t = mgr.get(&ih).unwrap();
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        assert!(holds_everything(&t), "seeding says so");
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        assert!(!holds_everything(&t), "downloading says not");
        // Added in seed mode: no picker, its data trusted whole.
        t.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
        assert!(t.picker.get().is_none());
        assert!(holds_everything(&t));
    }

    // ---- the queue: a check is neither a finished download nor a seed ---------------------

    /// A check holds a download slot while it has something to read, but a
    /// torrent that already holds every byte is not a download: it must not
    /// take the slot of one that is still waiting for its data.
    #[test]
    fn a_checking_torrent_that_holds_every_byte_does_not_take_a_download_slot() {
        let s = st("kill-queue-checked", "");
        let mgr = engine_manager(&s, "race");
        let ts = queued_downloads(&mgr, &["xa", "xb"]);
        ts[0].status.store(TorrentStatus::Checking as u8, Ordering::Relaxed);
        ts[0].total_downloaded.store(SIZE, Ordering::Relaxed);
        let cache = Cache::default();
        seeders(&cache, &ts[0], 50);
        seeders(&cache, &ts[1], 1);
        enforce_queue(&mgr, &cache, &lim("active_downloads = 1"), &HashSet::new(), &HashSet::new(), &mut HashSet::new(), &mut SlowClock::new(), now());
        assert!(ts[0].is_paused.load(Ordering::Relaxed), "the finished check was not started in a download slot");
        assert!(!ts[1].is_paused.load(Ordering::Relaxed), "the slot went to the download that still needs it");
    }

    /// A seed that is being checked is not a candidate of the seed queue: it
    /// is neither counted against the ceiling nor stopped to respect it.
    #[test]
    fn a_seed_being_checked_is_left_out_of_the_seed_queue() {
        let s = st("kill-queue-checked-seed", "");
        let mgr = engine_manager(&s, "race");
        let add = |n: &str| {
            let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", false, true).unwrap();
            mgr.get(&ih).unwrap()
        };
        let (seed, checked) = (add("xc"), add("xd"));
        seed.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        checked.status.store(TorrentStatus::Checking as u8, Ordering::Relaxed);
        checked.total_downloaded.store(SIZE, Ordering::Relaxed);
        for t in [&seed, &checked] {
            t.is_paused.store(false, Ordering::Relaxed);
        }
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &lim("queueing = true\nactive_seeds = 1"), &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert!(!seed.is_paused.load(Ordering::Relaxed), "the seed has the one seed slot");
        assert!(!checked.is_paused.load(Ordering::Relaxed), "the check was not counted, so not stopped");
        assert!(parked.is_empty());
    }

    // ---- the workers' timings and thresholds ---------------------------------------------

    /// An obligation declared at 0 hours is met by a torrent that has not
    /// seeded at all, and is the operator opting that tracker INTO deletion:
    /// only an UNdeclared tracker (below zero) waits.
    #[tokio::test(start_paused = true)]
    async fn the_transit_sweep_removes_a_torrent_whose_tracker_declares_zero_hours() {
        let s = st("kill-transit-zero", "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let mgr = engine_manager(&s, "hoard");
        write_categories(
            &s,
            serde_json::json!({
                "ssd": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "race", "graduate_to": "transit"},
                "transit": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "hoard", "transit": true},
            }),
        );
        let (zero, _, zero_file) = put(&s, "hoard", "tz", "transit", TRACKER, 0);
        let (second, _, second_file) = put(&s, "hoard", "tw", "transit", TRACKER, 0);
        let (undeclared, _, undeclared_file) = put(&s, "hoard", "ty", "transit", "https://other.example/announce", 0);
        let log = capture();
        spawn_transit_sweep(s.state.clone(), mgr.clone(), s.config.clone(), "hoard".into());
        after(91).await;
        for (h, f) in [(&zero, &zero_file), (&second, &second_file)] {
            assert!(!in_store(&s, h) && !f.exists(), "0 hours declared, 0 seeded: the obligation is met");
        }
        assert!(in_store(&s, &undeclared) && undeclared_file.exists(), "nothing declared: it waits");
        assert_eq!(lines(&log, "transit: seeding time served, removed").len(), 2, "one line per torrent");
        let summary = lines(&log, "transit sweep");
        assert_eq!(summary.len(), 1, "{summary:?}");
        assert!(summary[0].starts_with("INFO") && summary[0].contains("removed=2"), "{summary:?}");
        // The next pass finds nothing to remove and says nothing.
        after(300).await;
        assert_eq!(lines(&log, "transit sweep").len(), 1, "a pass that removed nothing is silent");
    }

    /// One torrent on a volume whose declared capacity is `quota`, with the
    /// drain on and its high watermark at `high`.
    fn race_drain_setup(tag: &str, extra: &str, quota: u64, high: i64) -> (TestState, Arc<TorrentManager>, String, std::path::PathBuf, String) {
        let s = st(tag, &format!("\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n{extra}"));
        let mgr = engine_manager(&s, "race");
        let (h, _, f) = put(&s, "race", "va", "", TRACKER, 100);
        let mount = crate::volumes::mount_point_of(&s.dir).to_string_lossy().to_string();
        crate::volumes::save_quota(&s.state, &mount, Some(quota)).unwrap();
        let on = crate::volumes::Policy { enabled: true, high, low: 0, inherited: false };
        crate::volumes::save_policy(&s.state, &mount, &on).unwrap();
        (s, mgr, h, f, mount)
    }

    /// With no interval configured (0) the drain looks every five minutes,
    /// not continuously.
    #[tokio::test(start_paused = true)]
    async fn the_race_drain_defaults_to_one_pass_every_five_minutes() {
        let (s, mgr, h, f, _) = race_drain_setup("kill-drain-default", "", SIZE, 100);
        spawn_race_drain(s.state.clone(), mgr.clone(), s.config.clone(), "race".into());
        after(11).await;
        after(280).await;
        std::thread::sleep(Duration::from_millis(300));
        after(0).await;
        assert!(in_store(&s, &h) && f.exists() && mgr.all().len() == 1, "291 s in: no pass yet");
        after(25).await;
        eventually("the first pass, five minutes after the start, drained it", || {
            std::thread::sleep(Duration::from_millis(10));
            mgr.all().is_empty()
        })
        .await;
        assert!(!in_store(&s, &h) && !f.exists());
    }

    /// A volume is drained from its high watermark, inclusive: allocated 50%
    /// against a high of 100 is left alone, allocated exactly 100% against
    /// 100 is drained.
    #[tokio::test(start_paused = true)]
    async fn the_race_drain_starts_at_the_high_watermark_exactly() {
        let (s, mgr, h, f, mount) = race_drain_setup("kill-drain-high", "[race_drain]\ncheck_interval_seconds = 60\n", 2 * SIZE, 100);
        spawn_race_drain(s.state.clone(), mgr.clone(), s.config.clone(), "race".into());
        after(11).await;
        after(60).await;
        after(60).await;
        std::thread::sleep(Duration::from_millis(300));
        after(0).await;
        assert!(in_store(&s, &h) && f.exists(), "50% allocated against a high of 100: left alone");

        crate::volumes::save_quota(&s.state, &mount, Some(SIZE)).unwrap();
        after(60).await;
        eventually("exactly at its high watermark the volume is drained", || {
            std::thread::sleep(Duration::from_millis(10));
            mgr.all().is_empty()
        })
        .await;
        assert!(!in_store(&s, &h) && !f.exists());
    }

    /// A graduation is charged to the budget when it is queued: one that
    /// covers the gap ends the pass, the others wait for the next one.
    #[test]
    fn a_graduation_that_covers_the_gap_ends_the_pass() {
        let s = graduation_state("kill-drain-budget", "72", "hoard");
        let mgr = engine_manager(&s, "race");
        let owing: Vec<_> = ["ma", "mb", "mc"].iter().map(|n| put(&s, "race", n, "ssd", TRACKER, 3600).0).collect();
        // 10 000 to free, a torrent is 16 384.
        let out = drain_once(&s.state, &mgr, &volume_on(&s, NO_SUCH_MOUNT, 100_000, 90_000, 80), &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (0, 1, 0));
        let queued = owing.iter().filter(|h| s.store.read().unwrap().job_pending_for("graduate", h)).count();
        assert_eq!(queued, 1, "one job covers the gap; the rest wait for the next pass");
    }

    /// A volume under `id` that has already given space back, so the disk
    /// delta of a pass measured at `id` is positive.
    fn volume_that_freed(s: &TestState, id: &str) -> crate::volumes::Volume {
        volume_on(s, id, u64::MAX, u64::MAX / 2, 0)
    }

    async fn trimmed_within_a_moment(s: &TestState, id: &str) -> bool {
        for _ in 0..60 {
            if crate::volumes::last_trim(&s.state, id).is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
            tokio::task::yield_now().await;
        }
        false
    }

    /// ⭐ A volume is trimmed after a drain that DELETED something and gave
    /// space back -- not after one that only queued graduations (nothing is
    /// freed yet), and not after one whose deletion returned no bytes.
    #[tokio::test]
    async fn a_volume_is_trimmed_only_after_a_drain_that_deleted_and_freed() {
        let tag = "kill-drain-trim";
        // Deleted, and the disk gave space back.
        let s = st(tag, "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let id = s.dir.to_string_lossy().to_string();
        put(&s, "race", "pa", "", TRACKER, 100);
        crate::volumes::save_trim(&s.state, &id, true).unwrap();
        let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_that_freed(&s, &id), &s.cfg(), "race");
        assert_eq!(out.deleted, 1);
        assert!(out.freed_bytes > 0);
        assert!(trimmed_within_a_moment(&s, &id).await, "deleted and freed: trimmed");

        // Deleted, but this id is no mount: nothing measured, nothing freed.
        let s = st(&format!("{tag}-nofree"), "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
        let id = "/nonexistent-trim-no-bytes".to_string();
        put(&s, "race", "pb", "", TRACKER, 100);
        crate::volumes::save_trim(&s.state, &id, true).unwrap();
        let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_that_freed(&s, &id), &s.cfg(), "race");
        assert_eq!((out.deleted, out.freed_bytes), (1, 0));
        assert!(!trimmed_within_a_moment(&s, &id).await, "nothing came back: no trim");

        // Space was measured, but only a graduation was queued: nothing deleted.
        let s = graduation_state(&format!("{tag}-grad"), "72", "hoard");
        let id = s.dir.to_string_lossy().to_string();
        put(&s, "race", "pc", "ssd", TRACKER, 3600);
        crate::volumes::save_trim(&s.state, &id, true).unwrap();
        let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_that_freed(&s, &id), &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated), (0, 1));
        assert!(out.freed_bytes > 0, "the disk figure alone would have asked for a trim");
        assert!(!trimmed_within_a_moment(&s, &id).await, "a graduation frees nothing yet: no trim");
    }

    /// The stagger start brings a batch of 2000 torrents up, pauses, and goes
    /// on -- without a pause after the last batch, which would hold the task
    /// for nothing.
    #[tokio::test(start_paused = true)]
    async fn the_stagger_start_brings_up_two_thousand_then_pauses_before_the_next() {
        let root = std::env::temp_dir().join(format!("hydra-stagger-batches-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        for i in 0..4000 {
            mgr.add_torrent_bytes(&torrent_bytes(&format!("{i:06}")), "/tmp", true, false).unwrap();
        }
        for t in mgr.all() {
            t.is_paused.store(false, Ordering::Relaxed);
            t.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
        }
        let started = || mgr.all().iter().filter(|t| t.status.load(Ordering::Relaxed) != TorrentStatus::Stopped as u8).count();
        let base = alive_tasks();
        spawn_stagger_start(mgr.clone());
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started(), 2000, "the first batch, then the pause");
        assert_eq!(alive_tasks(), base + 1);
        after(0).await;
        assert_eq!(started(), 2000, "the pause has not elapsed");
        tokio::time::advance(Duration::from_millis(100)).await;
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(started(), 4000, "the second batch");
        assert_eq!(alive_tasks(), base, "and nothing to wait for after the last one");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The store reconcile looks every five minutes.
    #[tokio::test(start_paused = true)]
    async fn the_reconcile_worker_waits_five_minutes_between_passes() {
        let s = st("kill-recon-period", "");
        live_one(&s, "race", "rg");
        orphan(&s, "race", O1);
        spawn_store_reconcile(s.engines.clone(), s.store.clone());
        eventually("the first tick dropped the first orphan", || rows(&s, "race").iter().all(|h| h != O1)).await;
        orphan(&s, "race", O2);
        after(299).await;
        std::thread::sleep(Duration::from_millis(300));
        after(0).await;
        assert!(rows(&s, "race").contains(&O2.to_string()), "299 s later: no second pass yet");
        after(2).await;
        eventually("the second pass, five minutes after the first", || rows(&s, "race").iter().all(|h| h != O2)).await;
    }

    // ---- what the workers tell the Logs tab ------------------------------------------------
    //
    // The Logs tab is where an operator looks when a disk fills or a torrent
    // waits, and several of these lines are the only trace of what a worker
    // decided: they are read back through the same ring the tab serves.

    /// Every tracing event of the test binary, with the thread that made it:
    /// the tests run in parallel and each reads back only its own thread's.
    static EVENTS: std::sync::Mutex<Vec<(std::thread::ThreadId, String, String)>> = std::sync::Mutex::new(Vec::new());

    /// Copies each event into `EVENTS`, message first and fields after, the
    /// way the Logs tab prints them.
    struct Record;

    #[derive(Default)]
    struct Said {
        message: String,
        fields: String,
    }

    impl tracing::field::Visit for Said {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            } else {
                self.fields.push_str(&format!(" {}={:?}", field.name(), value));
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Record {
        fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
            let mut said = Said::default();
            event.record(&mut said);
            EVENTS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push((std::thread::current().id(), event.metadata().level().to_string(), said.message + &said.fields));
        }
    }

    /// What this thread has logged since `capture()`.
    ///
    /// ⚠ One subscriber for the whole binary, installed once, not one per
    /// test: a scoped subscriber per test made each callsite's cached interest
    /// change under the feet of the tests running beside it, and a line was
    /// now and then not recorded at all.
    struct Capture {
        thread: std::thread::ThreadId,
        from: usize,
    }

    fn capture() -> Capture {
        use tracing_subscriber::layer::SubscriberExt;
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Record));
        });
        Capture { thread: std::thread::current().id(), from: EVENTS.lock().unwrap_or_else(|p| p.into_inner()).len() }
    }

    /// The lines whose text contains `needle`, as `LEVEL message fields`.
    fn lines(log: &Capture, needle: &str) -> Vec<String> {
        let all = EVENTS.lock().unwrap_or_else(|p| p.into_inner());
        all[log.from..]
            .iter()
            .filter(|(t, _, msg)| *t == log.thread && msg.contains(needle))
            .map(|(_, level, msg)| format!("{level} {msg}"))
            .collect()
    }

    /// A drain that cannot get a volume down says so, and says which
    /// categories keep their torrents on it, with how many.
    #[test]
    fn a_drain_that_cannot_leave_a_volume_names_the_categories_that_hold_it() {
        let log = capture();
        // graduate_to files under the same engine: nothing can move.
        let s = graduation_state("kill-log-same", "72", "race");
        for n in ["la", "lb"] {
            put(&s, "race", n, "ssd", TRACKER, 3600);
        }
        let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80), &s.cfg(), "race");
        assert_eq!((out.deleted, out.graduated, out.stuck), (0, 0, 2));
        let back = lines(&log, "graduate_to points back at this engine");
        assert_eq!(back.len(), 1, "{back:?}");
        assert!(back[0].starts_with("WARN") && back[0].contains("categories=ssd x2"), "{back:?}");
        assert!(lines(&log, "files of the same name already sit").is_empty(), "the other cause is not blamed");
        let short = lines(&log, "race drain could not free enough");
        assert_eq!(short.len(), 1, "{short:?}");
        assert!(short[0].contains("stuck=2") && short[0].contains("deleted=0"), "{short:?}");

        // Files of the same name already at the destination.
        let s = graduation_state("kill-log-taken", "72", "hoard");
        for n in ["lc", "ld"] {
            put(&s, "race", n, "ssd", TRACKER, 3600);
            std::fs::write(s.dir.join("pool").join(n), b"someone else's bytes").unwrap();
        }
        let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_on(&s, NO_SUCH_MOUNT, 100_000, 99_000, 80), &s.cfg(), "race");
        assert_eq!((out.graduated, out.stuck), (0, 2));
        let taken = lines(&log, "files of the same name already sit");
        assert_eq!(taken.len(), 1, "{taken:?}");
        assert!(taken[0].starts_with("WARN") && taken[0].contains("categories=ssd x2"), "{taken:?}");
        assert_eq!(lines(&log, "graduate_to points back at this engine").len(), 1, "still only the first pass's line");
    }

    /// A drain that frees what it needs is silent about it -- whether it
    /// lands exactly on the target or overshoots.
    #[test]
    fn a_drain_that_freed_enough_does_not_say_it_could_not() {
        for (tag, used) in [("kill-log-exact", 96_384), ("kill-log-over", 90_000)] {
            let log = capture();
            let s = st(tag, "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n");
            for n in ["lm", "ln", "lo"] {
                put(&s, "race", n, "", TRACKER, 100);
            }
            // 16 384 to free exactly, or 10 000: a torrent is 16 384.
            let out = drain_once(&s.state, &engine_manager(&s, "race"), &volume_on(&s, NO_SUCH_MOUNT, 100_000, used, 80), &s.cfg(), "race");
            assert_eq!((out.deleted, out.stuck), (1, 0), "{tag}");
            assert!(lines(&log, "could not free enough").is_empty(), "{tag}: {:?}", lines(&log, "could not free"));
            assert!(lines(&log, "graduate_to points back").is_empty() && lines(&log, "files of the same name").is_empty(), "{tag}");
            assert_eq!(lines(&log, "drained").len(), 1, "{tag}");
        }
    }

    fn running_seeds(mgr: &Arc<TorrentManager>, names: &[&str]) -> Vec<Arc<TorrentState>> {
        names
            .iter()
            .map(|n| {
                let (ih, _) = mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", false, true).unwrap();
                let t = mgr.get(&ih).unwrap();
                t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
                t.is_paused.store(false, Ordering::Relaxed);
                t
            })
            .collect()
    }

    /// The seed queue logs each pass that stopped or started seeds, with the
    /// counts and the ceiling, and nothing on a pass that changed nothing.
    #[test]
    fn the_seed_queue_logs_what_it_started_and_stopped() {
        let log = capture();
        let s = st("kill-log-queue", "");
        let mgr = engine_manager(&s, "race");
        let seeds = running_seeds(&mgr, &["lq", "lr", "ls"]);
        let mut parked = HashSet::new();
        let one = lim("queueing = true\nactive_seeds = 1");
        enforce_queue(&mgr, &Cache::default(), &one, &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert_eq!(running(&seeds), 1);
        let said = lines(&log, "queue: seeds");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("INFO") && said[0].contains("started=0 stopped=2 queued=2 ceiling=1"), "{said:?}");

        enforce_queue(&mgr, &Cache::default(), &one, &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert_eq!(lines(&log, "queue: seeds").len(), 1, "a pass that changed nothing says nothing");

        let three = lim("queueing = true\nactive_seeds = 3");
        enforce_queue(&mgr, &Cache::default(), &three, &HashSet::new(), &HashSet::new(), &mut parked, &mut SlowClock::new(), now());
        assert_eq!(running(&seeds), 3);
        let said = lines(&log, "queue: seeds");
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[1].contains("started=2 stopped=0 queued=0 ceiling=3"), "{said:?}");
    }

    /// The slot manager announces itself once the engine has settled -- when
    /// there is a ceiling to enforce, and only then.
    #[tokio::test(start_paused = true)]
    async fn the_slot_manager_announces_itself_only_when_a_ceiling_is_set() {
        let log = capture();
        let s = st("kill-log-slots", "\n[race]\nlisten_port = 16171\nactive_downloads = 2\n");
        spawn_download_slots(engine_manager(&s, "race"), Arc::new(Cache::default()), s.config.clone(), s.store.clone(), "race".into());
        after(0).await;
        assert!(lines(&log, "queue manager started").is_empty(), "not before the engine has settled");
        after(31).await;
        let said = lines(&log, "queue manager started");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("downloads=Some(2)"), "{said:?}");
        assert!(lines(&log, "no ceiling configured").is_empty());

        let log = capture();
        let s = st("kill-log-slots-none", "");
        spawn_download_slots(engine_manager(&s, "race"), Arc::new(Cache::default()), s.config.clone(), s.store.clone(), "race".into());
        after(0).await;
        after(31).await;
        assert_eq!(lines(&log, "no ceiling configured").len(), 1);
        assert!(lines(&log, "queue manager started").is_empty(), "nothing to announce without a ceiling");
    }

    /// The daily seed-time sync reports how many rows it wrote, and only when
    /// it wrote some.
    #[tokio::test(start_paused = true)]
    async fn the_seed_time_sync_logs_the_rows_it_wrote_and_is_silent_when_none_moved() {
        let log = capture();
        let s = st("kill-log-seedsync", "");
        let mgr = engine_manager(&s, "race");
        let (_, ta, _) = put(&s, "race", "za", "", TRACKER, 1234);
        put(&s, "race", "zb", "", TRACKER, 99);
        spawn_seed_time_sync(mgr.clone(), s.store.clone(), "race".into());
        after(3599).await;
        assert!(lines(&log, "seed time synced").is_empty());
        after(2).await;
        for _ in 0..5 {
            after(1).await;
        }
        let said = lines(&log, "seed time synced");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("INFO") && said[0].contains("rows=2"), "{said:?}");

        ta.seed_secs.store(5000, Ordering::Relaxed);
        after(24 * 3600).await;
        for _ in 0..5 {
            after(1).await;
        }
        let said = lines(&log, "seed time synced");
        assert_eq!(said.len(), 2, "{said:?}");
        assert!(said[1].contains("rows=1"), "{said:?}");

        // A day when nothing moved writes nothing and says nothing.
        after(24 * 3600).await;
        for _ in 0..5 {
            after(1).await;
        }
        assert_eq!(lines(&log, "seed time synced").len(), 2);
    }

    /// The reconcile reports the rows it dropped, and stays quiet on a pass
    /// that had none to drop and no refused record to protect.
    #[test]
    fn the_reconcile_logs_the_rows_it_dropped_and_is_silent_otherwise() {
        let log = capture();
        let s = st("kill-log-recon", "");
        live_one(&s, "race", "lt");
        orphan(&s, "race", O1);
        orphan(&s, "race", O2);
        reconcile_pass(&s.engines, &s.store);
        let said = lines(&log, "rows without a torrent removed");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].starts_with("INFO") && said[0].contains("dropped=2") && said[0].contains("engine=race"), "{said:?}");

        reconcile_pass(&s.engines, &s.store);
        assert_eq!(lines(&log, "rows without a torrent removed").len(), 1, "nothing was dropped the second time");
        assert!(lines(&log, "rows kept for records the loader refused").is_empty(), "no record was refused");
    }

    /// The stagger start reports its progress per batch and its total.
    #[tokio::test(start_paused = true)]
    async fn the_stagger_start_logs_its_progress_and_its_total() {
        let log = capture();
        let root = std::env::temp_dir().join(format!("hydra-stagger-log-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        for n in ["ua1", "ub1", "uc1"] {
            mgr.add_torrent_bytes(&torrent_bytes(n), "/tmp", true, false).unwrap();
        }
        let mut all = mgr.all();
        all.sort_by_key(|t| t.meta.name.clone());
        all[0].is_paused.store(true, Ordering::Relaxed);
        for t in &all[1..] {
            t.is_paused.store(false, Ordering::Relaxed);
        }
        spawn_stagger_start(mgr.clone());
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        let done = lines(&log, "stagger start done");
        assert_eq!(done.len(), 1, "{done:?}");
        assert!(done[0].contains("started=2") && done[0].contains("total=3"), "a stopped torrent is not counted as started: {done:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The memory watcher raises ONE alert when the process crosses its
    /// ceiling, stays quiet while it stays above, and says once when it is
    /// back under. Driven with real memory: the ceiling is set above where the
    /// process is, a gigabyte is held and let go.
    #[tokio::test(start_paused = true)]
    async fn the_memory_watch_alerts_once_per_crossing_of_the_ceiling() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let log = capture();
        const LIMIT: u64 = 1 << 30;
        // u64::MAX stands for "no reading": the reader returns None then.
        let rss = std::sync::Arc::new(AtomicU64::new(LIMIT - 1));
        let r = rss.clone();
        watch_memory(LIMIT, move || match r.load(Ordering::SeqCst) {
            u64::MAX => None,
            v => Some(v),
        });
        after(31).await;
        assert!(lines(&log, "resident memory").is_empty(), "under the ceiling: nothing to say");

        rss.store(LIMIT, Ordering::SeqCst);
        after(30).await;
        assert!(lines(&log, "resident memory").is_empty(), "at the ceiling exactly: not over it");

        rss.store(LIMIT + 1, Ordering::SeqCst);
        after(30).await;
        let over = lines(&log, "resident memory over the configured ceiling");
        assert_eq!(over.len(), 1, "{:?}", lines(&log, "resident memory"));
        assert!(over[0].starts_with("ERROR"));
        after(30).await;
        rss.store(u64::MAX, Ordering::SeqCst);
        after(30).await;
        assert_eq!(lines(&log, "resident memory over").len(), 1, "edge-triggered, and a missing reading changes nothing");
        assert!(lines(&log, "back under").is_empty());

        rss.store(LIMIT, Ordering::SeqCst);
        after(30).await;
        let back = lines(&log, "resident memory back under the ceiling");
        assert_eq!(back.len(), 1, "back at the ceiling is back under it: {:?}", lines(&log, "resident memory"));
        assert!(back[0].starts_with("INFO"));
        after(30).await;
        assert_eq!(lines(&log, "back under").len(), 1, "and only once");
    }
}
