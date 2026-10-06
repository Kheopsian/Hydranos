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
        tokio::time::sleep(SETTLE).await;
        if !first.is_empty() {
            tracing::info!(engine = %engine_id, downloads = ?first.downloads, seeds = ?first.seeds,
                total = ?first.total, "queue manager started");
        }
        // Seeds THIS loop stopped. Only those are ever started again by it: a
        // seed stopped by anything else (an operator, `start_paused`) is not
        // the queue's to resume.
        let mut parked: std::collections::HashSet<typhon_engine::torrent::meta::InfoHash> = Default::default();
        loop {
            tokio::time::sleep(DOWNLOAD_SLOT_INTERVAL).await;
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
            enforce_queue(&manager, &cache, &limits, &paused, &mut parked);
        }
    });
}

/// The three ceilings of one engine, `None` = not enforced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueLimits {
    pub downloads: Option<usize>,
    pub seeds: Option<usize>,
    pub total: Option<usize>,
}

impl QueueLimits {
    /// `active_downloads` as it always was: > 0 is a ceiling, anything else
    /// none. `active_seeds` / `active_limit` only under `queueing = true`
    /// (see `config::Session::queueing` for why the switch exists), and only
    /// when > 0: qBittorrent's -1 is "unlimited", and a 0 that stops every
    /// seed of the library is never what a file that says 0 meant.
    pub fn from_session(s: &crate::config::Session) -> QueueLimits {
        let cap = |v: i64| (v > 0).then_some(v as usize);
        let queueing = s.queueing.unwrap_or(false);
        QueueLimits {
            downloads: cap(s.active_downloads),
            seeds: if queueing { s.active_seeds.and_then(cap) } else { None },
            total: if queueing { s.active_limit.and_then(cap) } else { None },
        }
    }

    pub fn is_empty(&self) -> bool {
        self.downloads.is_none() && self.seeds.is_none() && self.total.is_none()
    }
}

/// Has this torrent all of its data? Read without the picker lock whenever
/// the status already says so.
fn holds_everything(t: &typhon_engine::torrent::meta::TorrentState) -> bool {
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
fn enforce_queue(
    manager: &Arc<TorrentManager>,
    cache: &Cache,
    limits: &QueueLimits,
    user_paused: &std::collections::HashSet<String>,
    parked: &mut std::collections::HashSet<typhon_engine::torrent::meta::InfoHash>,
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
        return;
    }

    let mut incomplete: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, i64, bool)> = Vec::new();
    let mut seeds: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, bool)> = Vec::new();
    for t in manager.all() {
        let status = t.status.load(Ordering::Relaxed);
        if status == TorrentStatus::Error as u8 {
            continue;
        }
        let hash = typhon_engine::torrent::hex_encode(&t.info_hash);
        if user_paused.contains(&hash) {
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
        let (mut started, mut stopped) = (0usize, 0usize);
        for (i, (torrent, running)) in seeds.iter().enumerate() {
            if i < ceiling {
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
        tokio::time::sleep(SETTLE + Duration::from_secs(60)).await;
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
            tokio::time::sleep(SHARE_LIMIT_INTERVAL).await;
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
                    let _ = store.set_paused(&hash, engine_id, true);
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
        tokio::time::sleep(Duration::from_secs(120)).await;
        let mut tick = tokio::time::interval(Duration::from_secs(5 * 60));
        loop {
            tick.tick().await;
            let engines = engines.clone();
            let _ = tokio::task::spawn_blocking(move || health_pass(&engines)).await;
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
        per_engine.push((engine.id.clone(), torrents, engine.manager.clone()));
    }
    crate::health::scan_dual_seed_live(&per_engine, &mut report);
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
        tokio::time::sleep(SEED_SYNC_FIRST).await;
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
                tokio::time::sleep(SEED_SYNC_YIELD).await;
            }
            if wrote_total > 0 && !failed {
                tracing::info!(engine = %engine_id, rows = wrote_total, "seed time synced");
            }
            tokio::time::sleep(SEED_SYNC_EVERY).await;
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
        tokio::time::sleep(Duration::from_secs(90)).await;
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
            tokio::time::sleep(Duration::from_secs(300)).await;
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
        tokio::time::sleep(Duration::from_secs(10)).await;
        tracing::info!(
            engine = %engine_id,
            check_interval_s = interval.as_secs(),
            "race drain started, one pass per volume"
        );
        loop {
            tokio::time::sleep(interval).await;
            // Re-read every tick: an obligation typed into the UI has to apply
            // to the NEXT pass, not to the next restart. Volumes are rediscovered
            // for the same reason -- a disk added today is a disk watched today.
            let cfg = match cfg_handle.read() {
                Ok(g) => g.clone(),
                Err(e) => e.into_inner().clone(),
            };
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
    store: Arc<crate::store::StoreLock>,
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
                        if store.delete_copy(&hash, &engine.id).unwrap_or(false) {
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
    });
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
        enforce_queue(&mgr, &cache, &zero, &paused, &mut Default::default());
        assert_eq!(mgr.count(), 0);
    }

    #[tokio::test]
    async fn enforcing_slots_on_an_empty_engine_is_a_no_op() {
        let s = st("slots-empty");
        let mgr = race_manager(&s);
        let cache = Cache::default();
        let paused = std::collections::HashSet::new();
        let five = QueueLimits { downloads: Some(5), ..Default::default() };
        enforce_queue(&mgr, &cache, &five, &paused, &mut Default::default());
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
    fn the_seed_queue_keeps_the_oldest_and_parks_the_rest_then_lets_them_go() {
        let (mgr, root) = manager("seeds");
        let all = seeds(&mgr);
        let cache = Cache::default();
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &cache, &limits("queueing = true\nactive_seeds = 1"), &HashSet::new(), &mut parked);
        let running: Vec<bool> = all.iter().map(|t| !t.is_paused.load(Ordering::Relaxed)).collect();
        assert_eq!(running.iter().filter(|r| **r).count(), 1, "one seed slot");
        assert_eq!(parked.len(), 2);
        // Stable: a second pass moves nothing.
        enforce_queue(&mgr, &cache, &limits("queueing = true\nactive_seeds = 1"), &HashSet::new(), &mut parked);
        assert_eq!(all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count(), 1);
        // A parked seed the operator then stops stays stopped when the
        // ceiling is lifted; the others come back.
        let operator: HashSet<String> = all
            .iter()
            .filter(|t| t.is_paused.load(Ordering::Relaxed))
            .take(1)
            .map(|t| typhon_engine::torrent::hex_encode(&t.info_hash))
            .collect();
        enforce_queue(&mgr, &cache, &QueueLimits::default(), &operator, &mut parked);
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
        enforce_queue(&mgr, &Cache::default(), &limits("queueing = true\nactive_limit = 2"), &HashSet::new(), &mut parked);
        assert_eq!(all.iter().filter(|t| !t.is_paused.load(Ordering::Relaxed)).count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The download ceiling alone never touches a seed.
    #[test]
    fn a_download_ceiling_leaves_the_seeds_alone() {
        let (mgr, root) = manager("dl-only");
        let all = seeds(&mgr);
        let mut parked = HashSet::new();
        enforce_queue(&mgr, &Cache::default(), &limits("active_downloads = 1"), &HashSet::new(), &mut parked);
        assert!(all.iter().all(|t| !t.is_paused.load(Ordering::Relaxed)));
        assert!(parked.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }
}
