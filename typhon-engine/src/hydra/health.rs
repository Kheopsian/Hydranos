//! The invariants, and what each one catches.
//!
//! Every entry here exists because something went wrong once and nothing
//! noticed. They are checks on statements that must be true of a healthy
//! engine: "seeding means I hold the data", "a leecher with seeds in the swarm
//! has peers", "an active torrent's files are on disk".

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;

use typhon_engine::torrent::meta::{TorrentState, TorrentStatus};

/// Downloading far more bytes than the torrent's own size means pieces were
/// re-fetched. This is the invariant that would have said "80 GB downloaded
/// for 3 GB" out loud.
pub const REDL: &str = "redl";
/// Advertising the seeding state while not holding the data: we announce
/// pieces we cannot serve.
pub const FAKE_SEED: &str = "fake_seed";
/// A leecher whose swarm has seeds, yet no peers connected. The shape of the
/// `left=0` announce bug: the tracker withheld the peer list because we had
/// mislabelled ourselves a seed, so the download never started.
pub const STARVED: &str = "starved";
/// A file we are supposed to hold is gone from disk.
pub const FILES_MISSING: &str = "files_missing";
/// The same info hash seeded by BOTH engines. Wasteful: a tracker credits
/// upload as the maximum per user and torrent, not the sum, and it splits
/// demand across two peers of ours.
pub const DUAL_SEED: &str = "dual_seed";
/// An active torrent whose save path has vanished. THE recurrent ghost: every
/// received piece fails its hash and is re-requested forever, and because
/// thrown pieces are never counted it is invisible in total_download -- `redl`
/// does not catch it. Only a stat does.
pub const GHOST: &str = "ghost";
/// A whole tracker host erroring. External, not an integrity bug, and kept out
/// of the alert path so a maintenance window does not wake anyone.
pub const TRACKER_OUTAGE: &str = "tracker_outage";

/// Re-download is only flagged past this multiple of the torrent's size, and
/// past a floor: a torrent a few bytes over its size is noise.
const REDL_FACTOR: f64 = 1.2;
const REDL_FLOOR_BYTES: i64 = 1 << 30;

/// One finding.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Anomaly {
    #[serde(rename = "type")]
    pub kind: String,
    pub engine: String,
    pub info_hash: String,
    pub name: String,
    pub detail: String,
    #[serde(skip_serializing_if = "is_zero")]
    pub wasted_bytes: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// Everything one pass found.
#[derive(Default)]
pub struct Report {
    pub anomalies: Vec<Anomaly>,
    pub counts: BTreeMap<String, i64>,
    pub wasted_bytes: i64,
    /// Bytes pulled by torrents that downloaded anything, and the part of it
    /// that landed (capped at each torrent's size): `efficiency` is the ratio.
    pub exchanged: i64,
    pub useful: i64,
    /// Torrents past `ROUTE_REDL_FACTOR` and `ROUTE_REDL_FLOOR_BYTES`, the
    /// looser "historical" re-download figure the API has always published.
    pub redl_historical: i64,
    pub redl_historical_bytes: i64,
    /// Torrents looked at, per engine.
    pub scanned: BTreeMap<String, i64>,
    /// The save paths of the active torrents, for the free-space check.
    pub save_paths: HashSet<std::path::PathBuf>,
    /// Off in the background pass: the breaker owns outages, and resolving
    /// every tracker URL of a million torrents to answer "no" is not free.
    pub skip_outage: bool,
    /// One stat per directory, not one per torrent: thousands of torrents
    /// share a category's save path.
    ghost_dirs: std::collections::HashMap<std::path::PathBuf, bool>,
}

/// The anomaly list is capped; the counts are not.
pub const MAX_LISTED: usize = 500;

/// The route's historical re-download gates: 1.2 x the size AND 50 MiB.
/// Looser than `REDL_FLOOR_BYTES`, which only flags what is worth an alert.
pub const ROUTE_REDL_FACTOR: f64 = 1.20;
pub const ROUTE_REDL_FLOOR_BYTES: i64 = 50 << 20;

impl Report {
    fn add(&mut self, a: Anomaly) {
        *self.counts.entry(a.kind.clone()).or_insert(0) += 1;
        if a.kind == REDL {
            self.wasted_bytes += a.wasted_bytes;
        }
        if self.anomalies.len() < MAX_LISTED {
            self.anomalies.push(a);
        }
    }

    /// Useful over exchanged; 1.0 when nothing was downloaded.
    pub fn efficiency(&self) -> f64 {
        if self.exchanged > 0 {
            self.useful as f64 / self.exchanged as f64
        } else {
            1.0
        }
    }

    pub fn truncated(&self) -> bool {
        self.counts.values().sum::<i64>() > self.anomalies.len() as i64
    }
}

static LATEST: std::sync::RwLock<Option<std::sync::Arc<(i64, u64, Report)>>> = std::sync::RwLock::new(None);

/// Keep a finished pass: (unix time, duration in ms, report).
pub fn publish(at: i64, took_ms: u64, report: Report) {
    if let Ok(mut g) = LATEST.write() {
        *g = Some(std::sync::Arc::new((at, took_ms, report)));
    }
}

/// The last pass, `None` before the first.
pub fn latest() -> Option<std::sync::Arc<(i64, u64, Report)>> {
    LATEST.read().ok().and_then(|g| g.clone())
}

/// Check one engine's torrents.
///
/// `seeds_in_swarm` comes from the announce cache: the engine itself only
/// knows connected peers, and a starved torrent has none by definition -- that
/// is the whole symptom.
pub fn scan_engine(
    engine: &str,
    torrents: &[std::sync::Arc<TorrentState>],
    seeds_in_swarm: impl Fn(&str) -> i64,
    host_in_outage: impl Fn(&str) -> bool,
    report: &mut Report,
) {
    *report.scanned.entry(engine.to_string()).or_insert(0) += torrents.len() as i64;
    for t in torrents {
        // Formatted only for a torrent that is reported or looked up: a
        // million hex strings per pass were most of its allocations.
        let hash = || typhon_engine::torrent::hex_encode(&t.info_hash);
        let status = t.status.load(Ordering::Relaxed);
        let downloaded = t.total_downloaded.load(Ordering::Relaxed) as i64;
        let size = t.meta.total_size as i64;
        let anomaly = |kind: &str, detail: String, wasted_bytes: i64| Anomaly {
            kind: kind.into(),
            engine: engine.into(),
            info_hash: hash(),
            name: t.meta.name.clone(),
            detail,
            wasted_bytes,
        };

        // Efficiency and the historical re-download figure.
        if downloaded > 0 {
            let done = typhon_engine::rpc::dispatch::torrent_core(t).total_done as i64;
            report.exchanged += downloaded;
            report.useful += done.min(size);
        }
        let extra = downloaded - size;
        if size > 0 && downloaded > (size as f64 * ROUTE_REDL_FACTOR) as i64 && extra >= ROUTE_REDL_FLOOR_BYTES {
            report.redl_historical += 1;
            report.redl_historical_bytes += extra;
        }

        // redl
        if size > 0 && downloaded > (size as f64 * REDL_FACTOR) as i64 && extra >= REDL_FLOOR_BYTES
        {
            report.add(anomaly(REDL, format!("downloaded {downloaded} for a size of {size}"), extra));
        }

        // files_missing: the serve path sets Error on ENOENT and never clears
        // it, precisely so this can be seen.
        if status == TorrentStatus::Error as u8 {
            report.add(anomaly(FILES_MISSING, "a read hit ENOENT: this torrent can serve nothing".into(), 0));
        }

        // fake_seed: announced complete, while the piece map says otherwise.
        // Only a torrent that still has a picker can say so; a seed-mode
        // torrent has none and its data is trusted whole.
        if status == TorrentStatus::Seeding as u8 {
            if let Some(picker) = t.picker.get() {
                let (have, total) = {
                    let p = picker.lock().unwrap_or_else(|e| e.into_inner());
                    (p.num_have(), t.meta.num_pieces())
                };
                let done = (have as u64 * t.meta.piece_length as u64).min(t.meta.total_size);
                if have < total && is_fake_seed(status, t.meta.total_size, done) {
                    report.add(anomaly(FAKE_SEED, format!("seeding with {have} of {total} pieces"), 0));
                }
            }
        }

        // ghost: an active torrent whose directory is gone. A stat, because
        // nothing else can see it.
        if status == TorrentStatus::Downloading as u8 || status == TorrentStatus::Seeding as u8 {
            let save_path = t.save_path.read().clone();
            if save_path.as_os_str().len() > 0 {
                let exists = match report.ghost_dirs.get(&save_path) {
                    Some(e) => *e,
                    None => {
                        let e = save_path.exists();
                        report.ghost_dirs.insert(save_path.clone(), e);
                        if e {
                            report.save_paths.insert(save_path.clone());
                        }
                        e
                    }
                };
                if !exists {
                    report.add(anomaly(GHOST, format!("save path {} is gone from disk", save_path.display()), 0));
                }
            }
        }

        // starved: leeching, the swarm has seeds, and we hold no peer.
        let peers = t.peers_connected.load(Ordering::Relaxed);
        if status == TorrentStatus::Downloading as u8
            && peers == 0
            && !t.is_paused.load(Ordering::Relaxed)
        {
            let seeds = seeds_in_swarm(&hash());
            if seeds > 0 {
                report.add(anomaly(STARVED, format!("{seeds} seeds in the swarm and no peer connected"), 0));
            }
        }

        // tracker_outage, collapsed per host by the caller's breaker.
        if report.skip_outage {
            continue;
        }
        'tiers: for tier in &t.meta.trackers {
            for url in tier {
                let host = crate::announce::overrides::override_host(url);
                if !host.is_empty() && host_in_outage(&host) {
                    report.add(anomaly(TRACKER_OUTAGE, format!("{host} stopped answering"), 0));
                    break 'tiers;
                }
            }
        }
    }
}

/// The same info hash held by two engines at once.
///
/// Checked across engines rather than inside one, which is why it does not
/// live in `scan_engine`.
#[cfg(test)]
pub fn scan_dual_seed(
    per_engine: &[(String, Vec<std::sync::Arc<TorrentState>>)],
    report: &mut Report,
) {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (engine, torrents) in per_engine {
        for t in torrents {
            if t.status.load(Ordering::Relaxed) != TorrentStatus::Seeding as u8 {
                continue;
            }
            let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
            seen.entry(hash).or_default().push(engine.clone());
        }
    }
    for (hash, engines) in seen {
        let unique: HashSet<&String> = engines.iter().collect();
        if unique.len() > 1 {
            report.add(Anomaly {
                kind: DUAL_SEED.into(),
                engine: engines.join("+"),
                info_hash: hash,
                name: String::new(),
                detail: "seeded by two engines: the credit is the maximum, not the sum".into(),
                wasted_bytes: 0,
            });
        }
    }
}

/// `scan_dual_seed` over live engines, by lookup instead of a map of every
/// hash: at a million torrents that map was a hundred MB built and thrown
/// away every pass, to find the handful held twice.
pub fn scan_dual_seed_live(
    engines: &[(String, Vec<std::sync::Arc<TorrentState>>, std::sync::Arc<typhon_engine::torrent::TorrentManager>)],
    report: &mut Report,
) {
    let seeding = |t: &TorrentState| t.status.load(Ordering::Relaxed) == TorrentStatus::Seeding as u8;
    for (i, (engine, torrents, _)) in engines.iter().enumerate() {
        for t in torrents.iter().filter(|t| seeding(t)) {
            // Reported once, by the first engine that holds it.
            if engines[..i].iter().any(|(_, _, m)| m.get(&t.info_hash).is_some_and(|o| seeding(&o))) {
                continue;
            }
            let others: Vec<&str> = engines[i + 1..]
                .iter()
                .filter(|(_, _, m)| m.get(&t.info_hash).is_some_and(|o| seeding(&o)))
                .map(|(id, _, _)| id.as_str())
                .collect();
            if !others.is_empty() {
                report.add(Anomaly {
                    kind: DUAL_SEED.into(),
                    engine: std::iter::once(engine.as_str()).chain(others).collect::<Vec<_>>().join("+"),
                    info_hash: typhon_engine::torrent::hex_encode(&t.info_hash),
                    name: t.meta.name.clone(),
                    detail: "seeded by two engines: the credit is the maximum, not the sum".into(),
                    wasted_bytes: 0,
                });
            }
        }
    }
}

/// A torrent that says it seeds while it does not hold the data.
///
/// Takes the completed byte count from the caller: it is derived from the
/// piece picker rather than stored, and the caller already has it from the row
/// it built. Computing it a second time here would walk every picker twice per
/// scan, over a quarter of a million torrents.
pub fn is_fake_seed(status: u8, total_size: u64, total_done: u64) -> bool {
    status == TorrentStatus::Seeding as u8 && total_size > 0 && total_done < total_size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_little_over_size_is_not_a_re_download() {
        // A torrent a few bytes past its own size is noise: overhead, a
        // re-requested block. Flagging it would bury the real ones.
        let size = 3_000_000_000i64;
        let over = |dl: i64| {
            let extra = dl - size;
            dl > (size as f64 * REDL_FACTOR) as i64 && extra >= REDL_FLOOR_BYTES
        };
        assert!(!over(size + 1024));
        assert!(!over(size + REDL_FLOOR_BYTES - 1));
        // 80 GB for 3 GB: the case this exists for.
        assert!(over(80_000_000_000));
    }

    #[test]
    fn the_report_counts_by_kind_and_sums_only_wasted_redl() {
        let mut r = Report::default();
        r.add(Anomaly {
            kind: REDL.into(),
            engine: "hoard".into(),
            info_hash: "aa".into(),
            name: "x".into(),
            detail: String::new(),
            wasted_bytes: 500,
        });
        r.add(Anomaly {
            kind: GHOST.into(),
            engine: "hoard".into(),
            info_hash: "bb".into(),
            name: "y".into(),
            detail: String::new(),
            wasted_bytes: 999,
        });
        assert_eq!(r.counts.get(REDL), Some(&1));
        assert_eq!(r.counts.get(GHOST), Some(&1));
        // Only re-download waste is bytes we can point at; a ghost's cost is
        // real but not measurable from here.
        assert_eq!(r.wasted_bytes, 500);
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use std::sync::Arc;
    use typhon_engine::torrent::TorrentManager;

    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-health-{tag}-{}-{:?}",
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

    /// Bencode lengths are COMPUTED, never counted by hand.
    fn torrent_bytes(name: &str, length: u64) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi{length}e4:name{}:{name}", name.len()).as_bytes());
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

    fn torrent(mgr: &Arc<TorrentManager>, name: &str, size: u64) -> Arc<TorrentState> {
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes(name, size), "/tmp", true, true)
            .expect("the fixture torrent parses");
        mgr.get(&ih).expect("just added")
    }

    /// The live dual-seed check finds a hash seeded by two engines once,
    /// and says which two.
    #[test]
    fn a_hash_seeded_by_two_live_engines_is_reported_once() {
        let (a, ra) = manager("dual-live-a");
        let (b, rb) = manager("dual-live-b");
        let ta = torrent(&a, "same", 16384);
        let tb = torrent(&b, "same", 16384);
        let _other = torrent(&a, "only-a", 16384);
        for t in [&ta, &tb] {
            t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        }
        let mut r = Report::default();
        scan_dual_seed_live(
            &[("race".into(), a.all(), a.clone()), ("vpn7".into(), b.all(), b.clone())],
            &mut r,
        );
        assert_eq!(kinds(&r), vec![DUAL_SEED.to_string()]);
        assert_eq!(r.anomalies[0].engine, "race+vpn7");
        let _ = (std::fs::remove_dir_all(ra), std::fs::remove_dir_all(rb));
    }

    /// The figures the route has always published, now from the pass:
    /// efficiency and the looser historical re-download count.
    #[test]
    fn the_pass_measures_efficiency_and_historical_redownloads() {
        let (m, root) = manager("efficiency");
        let t = torrent(&m, "eff", 16384);
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        // 16 KiB kept for 60 MiB pulled: past 1.2x and past 50 MiB.
        t.total_downloaded.store(60 << 20, Ordering::Relaxed);
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert_eq!(r.redl_historical, 1);
        assert_eq!(r.redl_historical_bytes, (60 << 20) - 16384);
        assert!((r.efficiency() - 16384.0 / (60u64 << 20) as f64).abs() < 1e-9);
        assert_eq!(r.scanned["race"], 1);
        let _ = std::fs::remove_dir_all(root);
    }

    /// The list is capped, the counts are not, and the cap is said.
    #[test]
    fn the_list_is_capped_and_says_so() {
        let mut r = Report::default();
        for i in 0..MAX_LISTED + 3 {
            r.add(Anomaly {
                kind: FILES_MISSING.into(),
                engine: "race".into(),
                info_hash: format!("{i}"),
                name: String::new(),
                detail: String::new(),
                wasted_bytes: 0,
            });
        }
        assert_eq!(r.anomalies.len(), MAX_LISTED);
        assert_eq!(r.counts[FILES_MISSING], (MAX_LISTED + 3) as i64);
        assert!(r.truncated());
    }

    fn no_seeds(_: &str) -> i64 {
        0
    }
    fn no_outage(_: &str) -> bool {
        false
    }

    fn kinds(r: &Report) -> Vec<String> {
        r.anomalies.iter().map(|a| a.kind.clone()).collect()
    }

    /// A healthy engine reports nothing. An invariant that fires on a clean
    /// library is worse than no invariant: it trains the operator to ignore it.
    #[test]
    fn a_healthy_torrent_raises_nothing() {
        let (mgr, root) = manager("clean");
        let t = torrent(&mgr, "clean", 16384);
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp");

        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert!(r.anomalies.is_empty(), "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ Two gates, not one. The floor alone counts a huge torrent that
    /// re-fetched a rounding error; the ratio alone counts a two-piece ebook
    /// that re-requested one piece.
    #[test]
    fn re_download_needs_both_the_ratio_and_the_floor() {
        let (mgr, root) = manager("redl");

        // Past the ratio but far under the floor: a small torrent that fetched
        // itself three times is still noise.
        let small = torrent(&mgr, "small", 16384);
        small.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        *small.save_path.write() = std::path::PathBuf::from("/tmp");
        small.total_downloaded.store(16384 * 3, Ordering::Relaxed);
        let mut r = Report::default();
        scan_engine("race", &[small], no_seeds, no_outage, &mut r);
        assert!(!kinds(&r).contains(&REDL.to_string()), "the floor must hold: {:?}", kinds(&r));

        // Past the floor AND the ratio: a real offender.
        let big = torrent(&mgr, "big", 16384);
        big.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        *big.save_path.write() = std::path::PathBuf::from("/tmp");
        big.total_downloaded.store(3 << 30, Ordering::Relaxed);
        let mut r2 = Report::default();
        scan_engine("race", &[big], no_seeds, no_outage, &mut r2);
        assert!(kinds(&r2).contains(&REDL.to_string()), "got {:?}", kinds(&r2));
        assert!(r2.wasted_bytes > 0, "a re-download reports what it wasted");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The Error status is set by the serve path on ENOENT and never cleared,
    /// precisely so this can be seen.
    #[test]
    fn a_torrent_that_can_serve_nothing_is_reported() {
        let (mgr, root) = manager("enoent");
        let t = torrent(&mgr, "gone", 16384);
        t.status.store(TorrentStatus::Error as u8, Ordering::Relaxed);
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert!(kinds(&r).contains(&FILES_MISSING.to_string()), "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ THE recurrent ghost: an active torrent whose save path is gone. Every
    /// received piece fails its hash and is re-requested forever, and thrown
    /// pieces are never counted -- so `redl` cannot see it. Only a stat can.
    #[test]
    fn an_active_torrent_whose_save_path_vanished_is_a_ghost() {
        let (mgr, root) = manager("ghost");
        let t = torrent(&mgr, "ghost", 16384);
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp/typhon-vanished-7c3a/sub");
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert!(kinds(&r).contains(&GHOST.to_string()), "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A STOPPED torrent whose path is gone is not a ghost: nothing is being
    /// re-requested, and reporting it would bury the real ones.
    #[test]
    fn a_stopped_torrent_with_no_path_is_not_a_ghost() {
        let (mgr, root) = manager("stopped");
        let t = torrent(&mgr, "stopped", 16384);
        t.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp/typhon-vanished-7c3a/sub");
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert!(!kinds(&r).contains(&GHOST.to_string()), "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ The shape of the `left=0` announce bug: the tracker withheld the peer
    /// list because we had mislabelled ourselves a seed, so the download never
    /// started. Seeds in the swarm, none connected.
    #[test]
    fn a_leecher_with_seeds_in_the_swarm_and_no_peer_is_starved() {
        let (mgr, root) = manager("starved");
        let t = torrent(&mgr, "starved", 16384);
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp");
        t.peers_connected.store(0, Ordering::Relaxed);
        t.is_paused.store(false, Ordering::Relaxed);

        let mut r = Report::default();
        scan_engine("race", &[t.clone()], |_| 12, no_outage, &mut r);
        assert!(kinds(&r).contains(&STARVED.to_string()), "got {:?}", kinds(&r));

        // Connected to someone: not starved, whatever the swarm holds.
        t.peers_connected.store(3, Ordering::Relaxed);
        let mut r2 = Report::default();
        scan_engine("race", &[t.clone()], |_| 12, no_outage, &mut r2);
        assert!(!kinds(&r2).contains(&STARVED.to_string()));

        // Paused on purpose: having no peer is the point, not a fault.
        t.peers_connected.store(0, Ordering::Relaxed);
        t.is_paused.store(true, Ordering::Relaxed);
        let mut r3 = Report::default();
        scan_engine("race", &[t], |_| 12, no_outage, &mut r3);
        assert!(!kinds(&r3).contains(&STARVED.to_string()), "a paused torrent is not starved");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A swarm with no seeds explains having no peer, so it is not a fault.
    #[test]
    fn a_leecher_in_a_seedless_swarm_is_not_starved() {
        let (mgr, root) = manager("seedless");
        let t = torrent(&mgr, "seedless", 16384);
        t.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp");
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        assert!(!kinds(&r).contains(&STARVED.to_string()));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A host in outage is reported ONCE per torrent, not once per tracker
    /// tier that names it.
    #[test]
    fn a_tracker_outage_is_reported_once_per_torrent() {
        let (mgr, root) = manager("outage");
        let t = torrent(&mgr, "outage", 16384);
        t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        *t.save_path.write() = std::path::PathBuf::from("/tmp");
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, |_| true, &mut r);
        let n = kinds(&r).iter().filter(|k| *k == TRACKER_OUTAGE).count();
        assert_eq!(n, 1, "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A tracker credits upload as the MAXIMUM per user and torrent, not the
    /// sum: seeding one hash from two engines splits demand across two peers of
    /// ours and earns nothing extra.
    #[test]
    fn the_same_hash_seeded_by_two_engines_is_reported_once() {
        let (mgr_a, root_a) = manager("dual-a");
        let (mgr_b, root_b) = manager("dual-b");
        let a = torrent(&mgr_a, "shared", 16384);
        let b = torrent(&mgr_b, "shared", 16384);
        assert_eq!(a.info_hash, b.info_hash, "the fixture builds the same torrent twice");
        a.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        b.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);

        let mut r = Report::default();
        scan_dual_seed(
            &[("race".to_string(), vec![a]), ("hoard".to_string(), vec![b])],
            &mut r,
        );
        let n = kinds(&r).iter().filter(|k| *k == DUAL_SEED).count();
        assert_eq!(n, 1, "one finding for one hash, got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// Held by two engines but seeded by only one: nothing is being split.
    #[test]
    fn a_hash_seeded_by_one_engine_only_is_not_a_dual_seed() {
        let (mgr_a, root_a) = manager("single-a");
        let (mgr_b, root_b) = manager("single-b");
        let a = torrent(&mgr_a, "shared", 16384);
        let b = torrent(&mgr_b, "shared", 16384);
        a.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        b.status.store(TorrentStatus::Stopped as u8, Ordering::Relaxed);

        let mut r = Report::default();
        scan_dual_seed(
            &[("race".to_string(), vec![a]), ("hoard".to_string(), vec![b])],
            &mut r,
        );
        assert!(!kinds(&r).contains(&DUAL_SEED.to_string()), "got {:?}", kinds(&r));
        let _ = std::fs::remove_dir_all(root_a);
        let _ = std::fs::remove_dir_all(root_b);
    }

    /// The counters must agree with the list they summarise, or the card and
    /// the table disagree on screen.
    #[test]
    fn the_counts_agree_with_the_anomalies_they_summarise() {
        let (mgr, root) = manager("counts");
        let t = torrent(&mgr, "broken", 16384);
        t.status.store(TorrentStatus::Error as u8, Ordering::Relaxed);
        let mut r = Report::default();
        scan_engine("race", &[t], no_seeds, no_outage, &mut r);
        let total: i64 = r.counts.values().sum();
        assert_eq!(total, r.anomalies.len() as i64);
        let _ = std::fs::remove_dir_all(root);
    }

    /// An empty engine is a clean report, not a panic.
    #[test]
    fn scanning_nothing_reports_nothing() {
        let mut r = Report::default();
        scan_engine("race", &[], no_seeds, no_outage, &mut r);
        scan_dual_seed(&[], &mut r);
        assert!(r.anomalies.is_empty());
        assert_eq!(r.wasted_bytes, 0);
    }
}
