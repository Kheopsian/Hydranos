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
}

impl Report {
    fn add(&mut self, a: Anomaly) {
        *self.counts.entry(a.kind.clone()).or_insert(0) += 1;
        if a.kind == REDL {
            self.wasted_bytes += a.wasted_bytes;
        }
        self.anomalies.push(a);
    }
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
    for t in torrents {
        let hash: String = t.info_hash.iter().map(|b| format!("{b:02x}")).collect();
        let name = t.meta.name.clone();
        let status = t.status.load(Ordering::Relaxed);
        let downloaded = t.total_downloaded.load(Ordering::Relaxed) as i64;
        let size = t.meta.total_size as i64;

        // redl
        let extra = downloaded - size;
        if size > 0 && downloaded > (size as f64 * REDL_FACTOR) as i64 && extra >= REDL_FLOOR_BYTES
        {
            report.add(Anomaly {
                kind: REDL.into(),
                engine: engine.into(),
                info_hash: hash.clone(),
                name: name.clone(),
                detail: format!("downloaded {downloaded} for a size of {size}"),
                wasted_bytes: extra,
            });
        }

        // files_missing: the serve path sets Error on ENOENT and never clears
        // it, precisely so this can be seen.
        if status == TorrentStatus::Error as u8 {
            report.add(Anomaly {
                kind: FILES_MISSING.into(),
                engine: engine.into(),
                info_hash: hash.clone(),
                name: name.clone(),
                detail: "a read hit ENOENT: this torrent can serve nothing".into(),
                wasted_bytes: 0,
            });
        }

        // ghost: an active torrent whose directory is gone. A stat, because
        // nothing else can see it.
        if status == TorrentStatus::Downloading as u8 || status == TorrentStatus::Seeding as u8 {
            let save_path = t.save_path.read().clone();
            if save_path.as_os_str().len() > 0 && !save_path.exists() {
                report.add(Anomaly {
                    kind: GHOST.into(),
                    engine: engine.into(),
                    info_hash: hash.clone(),
                    name: name.clone(),
                    detail: format!("save path {} is gone from disk", save_path.display()),
                    wasted_bytes: 0,
                });
            }
        }

        // starved: leeching, the swarm has seeds, and we hold no peer.
        let peers = t.peers_connected.load(Ordering::Relaxed);
        if status == TorrentStatus::Downloading as u8
            && peers == 0
            && seeds_in_swarm(&hash) > 0
            && !t.is_paused.load(Ordering::Relaxed)
        {
            report.add(Anomaly {
                kind: STARVED.into(),
                engine: engine.into(),
                info_hash: hash.clone(),
                name: name.clone(),
                detail: format!("{} seeds in the swarm and no peer connected", seeds_in_swarm(&hash)),
                wasted_bytes: 0,
            });
        }

        // tracker_outage, collapsed per host by the caller's breaker.
        for tier in &t.meta.trackers {
            for url in tier {
                let host = crate::announce::overrides::override_host(url);
                if !host.is_empty() && host_in_outage(&host) {
                    report.add(Anomaly {
                        kind: TRACKER_OUTAGE.into(),
                        engine: engine.into(),
                        info_hash: hash.clone(),
                        name: name.clone(),
                        detail: format!("{host} stopped answering"),
                        wasted_bytes: 0,
                    });
                    break;
                }
            }
        }
    }
}

/// The same info hash held by two engines at once.
///
/// Checked across engines rather than inside one, which is why it does not
/// live in `scan_engine`.
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
