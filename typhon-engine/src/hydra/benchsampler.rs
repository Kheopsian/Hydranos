//! The writer behind the benchmark graphs.
//!
//! Every five seconds it appends one row of headline counters to
//! `bench_samples`, and one row per tracker to `tracker_samples`. 3.x sampled
//! at exactly this interval and the graphs assume that spacing, so the figure
//! is a contract rather than a tuning knob.
//!
//! Without this task the graphs are not wrong, they are empty -- and an empty
//! graph and a node that moved no bytes look identical. That is the whole
//! reason the sampler is a module of its own: the endpoints can only ever
//! answer what something else recorded.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::benchdb::Shared;
use crate::engines::EngineHost;
use crate::store::Store;

/// How often a sample is taken. Matches what 3.x wrote.
const INTERVAL: Duration = Duration::from_secs(5);

/// Announce counters as of the previous sample, per engine, so a rate can be
/// differenced out of them -- the counters themselves are monotonic totals.
type Previous = std::collections::HashMap<String, (u64, u64, f64)>;

/// Host figures over the last interval: iowait and the ZFS ARC.
///
/// ⚠ Recorded as a constant 0 from the Rust port (2026-09-07) until 4.3: the
/// IOWait and ARC graphs were flat for three weeks and read as "no disk
/// pressure", which nothing had measured.
#[derive(Debug, Clone, Copy, Default)]
pub struct System {
    pub measured: bool,
    pub iowait_pct: f64,
    pub arc_size_bytes: f64,
    pub arc_hit_rate_pct: f64,
    pub arc_demand_hit_rate_pct: f64,
    pub arc_miss_per_sec: f64,
    pub arc_demand_miss_per_sec: f64,
    pub arc_ghost_hits_per_sec: f64,
}

/// The counters the next interval is differenced against.
#[derive(Default)]
struct SystemPrev {
    cpu: Option<(u64, u64)>,
    arc: Option<([f64; 5], f64)>,
}

static LATEST_SYSTEM: std::sync::Mutex<System> = std::sync::Mutex::new(System {
    measured: false, iowait_pct: 0.0, arc_size_bytes: 0.0, arc_hit_rate_pct: 0.0,
    arc_demand_hit_rate_pct: 0.0, arc_miss_per_sec: 0.0, arc_demand_miss_per_sec: 0.0,
    arc_ghost_hits_per_sec: 0.0,
});

/// What the sampler measured last, for the live cards.
pub fn latest_system() -> System {
    LATEST_SYSTEM.lock().map(|g| *g).unwrap_or_default()
}

/// Per-tracker rows are taken every sixth tick, 30 s: a pass reads every
/// torrent, and the chart they feed spans hours.
const TRACKER_EVERY: u64 = 6;
/// A pass slower than this is worth a warning: several times the 1.0-1.6 s it
/// takes over the 1.1M torrents of the production node, and a sixth of the
/// interval, past which passes start being skipped.
const TRACKER_PASS_SLOW: Duration = Duration::from_secs(5);

/// Whether a tracker pass took long enough to say so in the log (warn,
/// shown in the Logs tab) rather than at debug: strictly over the limit.
fn slow_tracker_pass(took: Duration) -> bool {
    took > TRACKER_PASS_SLOW
}

/// One (engine, tracker) row of the Trackers tab, live and as stored.
///
/// Fields in alphabetical order: that is the order 3.x published them in.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TrackerRow {
    /// Torrents with at least one peer connected.
    pub active: i64,
    pub cum_downloaded: i64,
    pub cum_uploaded: i64,
    pub download_rate: i64,
    pub engine: String,
    pub peers: i64,
    /// Bytes of the torrents seeding to this tracker: what the tracker credits
    /// as seed size. A cross-seed counts under each of its trackers, as each
    /// one counts it, so the sum across rows is not the space on disk -- the
    /// link scanner answers that one (`linkindex::usage`).
    pub seed_size: i64,
    pub torrents: i64,
    pub tracker: String,
    pub ts: i64,
    pub upload_rate: i64,
}

static LATEST_TRACKERS: std::sync::Mutex<Option<Vec<TrackerRow>>> = std::sync::Mutex::new(None);

/// The rows of the last tracker pass, or `None` before the first one.
pub fn latest_trackers() -> Option<Vec<TrackerRow>> {
    LATEST_TRACKERS.lock().ok().and_then(|g| g.clone())
}

/// Per-tracker figures across every loaded torrent, plus the stored baseline.
///
/// The stored counters are what each tracker accounted for before the running
/// engines started (and what removed torrents carried away); the live torrents
/// are added on top, and the torrent count comes from them alone. Reporting the
/// stored figure alone froze the Trackers tab at the last restart.
///
/// Reads four atomics and the tracker list of each torrent, never the JSON row:
/// building `torrent_to_json` for each one cost 8 s over a million torrents.
pub fn tracker_totals(engines: &EngineHost, store: &crate::store::StoreLock, ts: i64) -> Vec<TrackerRow> {
    let mut rows: std::collections::BTreeMap<(String, String), TrackerRow> =
        std::collections::BTreeMap::new();
    {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        for (engine, tracker, ul, dl) in store.tracker_counters().unwrap_or_default() {
            rows.insert(
                (engine.clone(), tracker.clone()),
                TrackerRow { cum_uploaded: ul, cum_downloaded: dl, engine, tracker, ..Default::default() },
            );
        }
    }

    for engine in engines.engines() {
        let mut live: std::collections::HashMap<String, TrackerRow> = std::collections::HashMap::new();
        for torrent in engine.manager.all().iter() {
            let trackers = torrent.live_trackers.read();
            // The host baked into the torrent, NOT a result of announcing:
            // live_trackers is filled when the torrent is built. So a torrent
            // that has never announced still counts under its own tracker, and
            // this bucket holds only the ones carrying no announce URL at all.
            let host = trackers
                .iter()
                .flatten()
                .next()
                .map(|u| typhon_engine::rpc::dispatch::tracker_host_str(u))
                .filter(|h| !h.is_empty())
                .unwrap_or("(no tracker)");
            let row = match live.get_mut(host) {
                Some(row) => row,
                None => live.entry(host.to_string()).or_default(),
            };
            let peers = torrent.peers_connected.load(Ordering::Relaxed) as i64;
            row.cum_uploaded += torrent.total_uploaded.load(Ordering::Relaxed) as i64;
            row.cum_downloaded += torrent.total_downloaded.load(Ordering::Relaxed) as i64;
            row.upload_rate += torrent.upload_rate.get() as i64;
            row.download_rate += torrent.download_rate.get() as i64;
            row.peers += peers;
            row.active += (peers > 0) as i64;
            row.seed_size += seeding_size(torrent);
            row.torrents += 1;
        }
        for (host, part) in live {
            let row = rows.entry((engine.id.clone(), host.clone())).or_insert_with(|| TrackerRow {
                engine: engine.id.clone(),
                tracker: host,
                ..Default::default()
            });
            row.cum_uploaded += part.cum_uploaded;
            row.cum_downloaded += part.cum_downloaded;
            row.upload_rate += part.upload_rate;
            row.download_rate += part.download_rate;
            row.peers += part.peers;
            row.active += part.active;
            row.seed_size += part.seed_size;
            row.torrents += part.torrents;
        }
    }

    rows.into_values().map(|r| TrackerRow { ts, ..r }).collect()
}

/// What one torrent adds to its tracker's seed size: all of it while it
/// seeds, nothing otherwise.
///
/// Two atomics, never the picker: a tracker only credits what is announced as
/// complete, and the engine's `Seeding` state is that claim. A paused torrent
/// keeps the state word but announces nothing, so it counts for nothing.
fn seeding_size(t: &typhon_engine::torrent::meta::TorrentState) -> i64 {
    let seeding = t.status.load(Ordering::Relaxed)
        == typhon_engine::torrent::meta::TorrentStatus::Seeding as u8;
    if seeding && !t.is_paused.load(Ordering::Relaxed) {
        t.meta.total_size as i64
    } else {
        0
    }
}

/// Share of all CPU time spent waiting on I/O since the previous call, from
/// the host-wide `cpu` line of /proc/stat.
fn iowait_pct(stat: &str, prev: &mut Option<(u64, u64)>) -> f64 {
    let Some(line) = stat.lines().find(|l| l.starts_with("cpu ")) else { return 0.0 };
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 {
        return 0.0;
    }
    // user nice system idle iowait irq softirq steal; guest time is already
    // inside user and nice, so it is not added twice.
    let total: u64 = v.iter().take(8).sum();
    let io = v[4];
    let pct = match *prev {
        Some((p_io, p_total)) if total > p_total => {
            io.saturating_sub(p_io) as f64 / (total - p_total) as f64 * 100.0
        }
        _ => 0.0,
    };
    *prev = Some((io, total));
    pct
}

/// ARC hit rates and misses over the last interval, from arcstats counters.
fn arc_interval(text: &str, ts: f64, prev: &mut Option<([f64; 5], f64)>, out: &mut System) {
    let field = |name: &str| -> f64 {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .and_then(|l| l.split_whitespace().nth(2))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    out.arc_size_bytes = field("size");
    let now = [
        field("hits"),
        field("misses"),
        field("demand_data_hits") + field("demand_metadata_hits"),
        field("demand_data_misses") + field("demand_metadata_misses"),
        field("mru_ghost_hits") + field("mfu_ghost_hits"),
    ];
    if let Some((p, p_ts)) = *prev {
        let dt = ts - p_ts;
        if dt > 0.0 {
            let d: Vec<f64> = now.iter().zip(p.iter()).map(|(a, b)| (a - b).max(0.0)).collect();
            let pct = |h: f64, m: f64| if h + m > 0.0 { h / (h + m) * 100.0 } else { 100.0 };
            out.arc_hit_rate_pct = pct(d[0], d[1]);
            out.arc_miss_per_sec = d[1] / dt;
            out.arc_demand_hit_rate_pct = pct(d[2], d[3]);
            out.arc_demand_miss_per_sec = d[3] / dt;
            out.arc_ghost_hits_per_sec = d[4] / dt;
            out.measured = true;
        }
    }
    *prev = Some((now, ts));
}

pub fn spawn(engines: Arc<EngineHost>, bench: Shared, store: Arc<crate::store::StoreLock>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        // A sampler that fell behind must not then burst: the graph would show
        // several rows sharing one instant.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous: Previous = std::collections::HashMap::new();
        let mut system = SystemPrev::default();
        let mut n: u64 = 0;
        let tracker_pass_running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        loop {
            tick.tick().await;
            if let Err(e) = sample_once(&engines, &bench, &store, &mut previous, &mut system) {
                tracing::warn!("bench sample failed: {e}");
            }
            // On a blocking thread of its own: the pass reads every torrent,
            // ~1.2 s on 1.1M in production, and run here it held one of the
            // runtime's workers for all of it. Not awaited, so the 5 s samples
            // keep their spacing; never two at once, so a slow pass is skipped
            // rather than stacked.
            if n % TRACKER_EVERY == 0 && !tracker_pass_running.swap(true, Ordering::AcqRel) {
                let (engines, bench, store) = (engines.clone(), bench.clone(), store.clone());
                let running = tracker_pass_running.clone();
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = sample_trackers(&engines, &bench, &store) {
                        tracing::warn!("tracker sample failed: {e}");
                    }
                    running.store(false, Ordering::Release);
                });
            }
            n += 1;
            // Older samples are folded into 5-minute rows a bounded slice at a
            // time, so catching up on months of history never holds the writer
            // for more than one slice.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let db = bench.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(e) = db.compact_tracker_samples(now) {
                tracing::warn!("tracker sample compaction failed: {e}");
            }
        }
    });
}

fn sample_once(
    engines: &Arc<EngineHost>,
    bench: &Shared,
    store: &Arc<crate::store::StoreLock>,
    previous: &mut Previous,
    system: &mut SystemPrev,
) -> anyhow::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let gauges = |id: &str| -> (f64, f64, f64, f64, f64, f64) {
        match engines.get(id) {
            Some(e) => {
                let m = &e.manager;
                (
                    m.upload_rate.get() as f64,
                    m.download_rate.get() as f64,
                    m.cached_active_peers.load(Ordering::Relaxed) as f64,
                    m.all().len() as f64,
                    m.cached_torrents_with_peers.load(Ordering::Relaxed) as f64,
                    m.cached_torrents_uploading.load(Ordering::Relaxed) as f64,
                )
            }
            None => (0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
        }
    };

    let (race_up, race_down, race_peers, race_torrents, _race_with, race_uploading) =
        gauges("race");
    let (hoard_up, hoard_down, hoard_peers, _hoard_torrents, hoard_with, hoard_uploading) =
        gauges("hoard");
    // Every engine beyond the two the columns are named after: one row each
    // in `engine_samples`, and their sum in the `extra_*` columns so the
    // Records and the totals count them. 4.3 sampled `race` and `hoard` by
    // name and an `[[engine]]` block was in no chart and no record.
    let extra: Vec<crate::benchdb::EngineSample> = engines
        .engines()
        .iter()
        .filter(|e| e.id != "race" && e.id != "hoard")
        .map(|e| {
            let (up, down, peers, torrents, _with, uploading) = gauges(&e.id);
            crate::benchdb::EngineSample {
                engine: e.id.clone(),
                upload_rate: up,
                download_rate: down,
                peers,
                uploading,
                torrents,
            }
        })
        .collect();
    let extra_sum = |f: fn(&crate::benchdb::EngineSample) -> f64| extra.iter().map(f).sum::<f64>();

    // The lifetime figure is the stored baseline plus what the loaded torrents
    // account for, the same sum the status route publishes. Sampling the
    // session half alone would walk the petabyte milestones backwards on every
    // restart.
    // Announce rates, differenced from the monotonic totals. The first sample
    // after a start has no predecessor and reports zero rather than dividing by
    // the whole uptime, which would draw a spike that never happened.
    let mut announce_rate = |id: &str| -> (f64, f64) {
        let Some(engine) = engines.get(id) else {
            return (0.0, 0.0);
        };
        let (ok, failed) = engine.announce_cache.outcomes();
        let rate = match previous.get(id) {
            Some((prev_ok, prev_failed, prev_ts)) if ts > *prev_ts => {
                let dt = ts - prev_ts;
                (
                    ok.saturating_sub(*prev_ok) as f64 / dt,
                    failed.saturating_sub(*prev_failed) as f64 / dt,
                )
            }
            _ => (0.0, 0.0),
        };
        previous.insert(id.to_string(), (ok, failed, ts));
        rate
    };
    let (race_ann, race_ann_fail) = announce_rate("race");
    let (hoard_ann, hoard_ann_fail) = announce_rate("hoard");

    // The schedule's own health, published by each engine's scheduler: what
    // the catalogue needs per second, how many torrents are past their
    // deadline and by how much, and the concurrency it runs at.
    let health = |id: &str| -> [f64; 7] {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(e) = engines.get(id) else { return [0.0; 7] };
        let a = &e.admission;
        [
            a.needed_milli.load(Relaxed) as f64 / 1000.0,
            a.late.load(Relaxed) as f64,
            a.lag_p50_s.load(Relaxed) as f64,
            a.lag_p90_s.load(Relaxed) as f64,
            a.concurrency.load(Relaxed) as f64,
            a.latency_ms.load(Relaxed) as f64,
            a.throttled_permille.load(Relaxed) as f64 / 10.0,
        ]
    };
    let (race_h, hoard_h) = (health("race"), health("hoard"));
    let flight = |id: &str| -> f64 {
        engines.get(id).map(|e| e.admission.in_flight.load(Ordering::Relaxed) as f64).unwrap_or(0.0)
    };

    let mut sys = System::default();
    if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
        sys.iowait_pct = iowait_pct(&stat, &mut system.cpu);
    }
    if let Ok(arc) = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats") {
        arc_interval(&arc, ts, &mut system.arc, &mut sys);
    }
    if let Ok(mut g) = LATEST_SYSTEM.lock() {
        *g = sys;
    }

    let (base_up, base_down) = {
        let store = store.lock().unwrap_or_else(|e| e.into_inner());
        store.counter("global")
    };
    let (session_up, session_down) = engines.session_totals();

    let sample = serde_json::json!({
        "ts": ts,
        "race_upload_rate": race_up,
        "race_download_rate": race_down,
        "race_peers": race_peers,
        "race_torrents": race_torrents,
        "race_uploading": race_uploading,
        "hoard_upload_rate": hoard_up,
        "hoard_peers": hoard_peers,
        // 3.x names the count of torrents with a live peer "active" here.
        "hoard_active": hoard_with,
        "hoard_with_peers": hoard_with,
        "hoard_uploading": hoard_uploading,
        "open_fds": open_fd_count(),
        "race_session_uploaded": session_up,
        "global_uploaded": base_up + session_up,
        "global_downloaded": base_down + session_down,
        "race_announce_rate": race_ann,
        "hoard_announce_rate": hoard_ann,
        "race_announce_fail_rate": race_ann_fail,
        "hoard_announce_fail_rate": hoard_ann_fail,
        "race_announce_in_flight": flight("race"),
        "hoard_announce_in_flight": flight("hoard"),
        "iowait_pct": sys.iowait_pct,
        "arc_size_bytes": sys.arc_size_bytes,
        "arc_hit_rate_pct": sys.arc_hit_rate_pct,
        "arc_demand_hit_rate_pct": sys.arc_demand_hit_rate_pct,
        "arc_miss_per_sec": sys.arc_miss_per_sec,
        "arc_demand_miss_per_sec": sys.arc_demand_miss_per_sec,
        "arc_ghost_hits_per_sec": sys.arc_ghost_hits_per_sec,
        "race_announce_needed": race_h[0],
        "race_announce_late": race_h[1],
        "race_announce_lag_p50": race_h[2],
        "race_announce_lag_p90": race_h[3],
        "race_announce_concurrency": race_h[4],
        "race_announce_latency_ms": race_h[5],
        "race_announce_throttled_pct": race_h[6],
        "hoard_announce_needed": hoard_h[0],
        "hoard_announce_late": hoard_h[1],
        "hoard_announce_lag_p50": hoard_h[2],
        "hoard_announce_lag_p90": hoard_h[3],
        "hoard_announce_concurrency": hoard_h[4],
        "hoard_announce_latency_ms": hoard_h[5],
        "hoard_announce_throttled_pct": hoard_h[6],
        "hoard_download_rate": hoard_down,
        "extra_upload_rate": extra_sum(|e| e.upload_rate),
        "extra_download_rate": extra_sum(|e| e.download_rate),
        "extra_peers": extra_sum(|e| e.peers),
        "extra_uploading": extra_sum(|e| e.uploading),
    });

    let db = bench.lock().unwrap_or_else(|e| e.into_inner());
    db.record_sample(&sample)?;
    db.record_engine_samples(ts, &extra)?;
    Ok(())
}

/// One tracker pass: published for the live table, then stored for the chart.
///
/// ⚠ From the Rust port (2026-09-07) until this pass existed, the rows were
/// written from the announce cache with every rate, peer and byte count at a
/// constant 0: the chart was flat and read as "this tracker moves nothing".
fn sample_trackers(
    engines: &Arc<EngineHost>,
    bench: &Shared,
    store: &Arc<crate::store::StoreLock>,
) -> anyhow::Result<()> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The pass reads every torrent, and none of it under the bench lock. Its
    // cost grows with the catalogue, so it says when it stops being cheap --
    // measured against what it costs normally, not against a round number:
    // 1.0-1.6 s on 1.1M torrents is the steady state, and a warning every
    // 30 s for that is noise that hides the one that matters.
    let started = std::time::Instant::now();
    let rows = tracker_totals(engines, store, ts);
    let took = started.elapsed();
    if slow_tracker_pass(took) {
        tracing::warn!(ms = took.as_millis() as u64, "tracker pass is slow");
    } else {
        tracing::debug!(ms = took.as_millis() as u64, "tracker pass");
    }
    if let Ok(mut g) = LATEST_TRACKERS.lock() {
        *g = Some(rows.clone());
    }
    let db = bench.lock().unwrap_or_else(|e| e.into_inner());
    db.record_tracker_samples(ts as f64, &rows)
}

fn open_fd_count() -> f64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count() as f64)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iowait_is_the_share_of_the_interval_not_since_boot() {
        let mut prev = None;
        let a = "cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 1 1 1 1 1 1 1\n";
        assert_eq!(iowait_pct(a, &mut prev), 0.0, "the first read has nothing to difference");
        // 100 more ticks, 25 of them iowait.
        let b = "cpu  150 0 110 715 125 0 0 0 0 0\n";
        assert!((iowait_pct(b, &mut prev) - 25.0).abs() < 1e-9);
    }

    #[test]
    fn arc_rates_are_differenced_per_interval() {
        let mut prev = None;
        let mut out = System::default();
        let a = "hits 4 1000\nmisses 4 10\nsize 4 2048\ndemand_data_hits 4 500\ndemand_data_misses 4 5\n";
        arc_interval(a, 100.0, &mut prev, &mut out);
        assert!(!out.measured);
        assert_eq!(out.arc_size_bytes, 2048.0);
        let b = "hits 4 1090\nmisses 4 20\nsize 4 4096\ndemand_data_hits 4 545\ndemand_data_misses 4 10\n";
        arc_interval(b, 105.0, &mut prev, &mut out);
        assert!(out.measured);
        assert!((out.arc_hit_rate_pct - 90.0).abs() < 1e-9, "90 hits, 10 misses in the interval");
        assert!((out.arc_miss_per_sec - 2.0).abs() < 1e-9, "10 misses in 5 s");
        assert!((out.arc_demand_hit_rate_pct - 90.0).abs() < 1e-9);
    }

    // ---- /proc readers: the edges the happy-path tests above do not reach ----

    #[test]
    fn iowait_reads_nothing_it_cannot_trust() {
        let mut prev = Some((10, 100));
        assert_eq!(iowait_pct("cpu0 1 2 3 4 5\nintr 9\n", &mut prev), 0.0, "no aggregate cpu line");
        assert_eq!(prev, Some((10, 100)), "a read that found nothing leaves the baseline alone");
        assert_eq!(iowait_pct("cpu  1 2 3 4\n", &mut prev), 0.0, "fewer than the five fields up to iowait");
        assert_eq!(prev, Some((10, 100)));
    }

    #[test]
    fn iowait_does_not_divide_by_an_interval_that_did_not_advance() {
        let mut prev = None;
        let a = "cpu  100 0 100 700 100 0 0 0 0 0\n";
        iowait_pct(a, &mut prev);
        // Same totals again (a counter that did not tick): 0, not NaN or inf.
        assert_eq!(iowait_pct(a, &mut prev), 0.0);
        assert_eq!(prev, Some((100, 1000)), "the baseline is still the latest read");
    }

    #[test]
    fn iowait_counts_eight_columns_and_not_the_guest_time_twice() {
        let mut prev = None;
        // Columns 9 and 10 (guest, guest_nice) are already inside user and nice.
        iowait_pct("cpu  100 0 100 700 100 0 0 0 5000 5000\n", &mut prev);
        assert_eq!(prev, Some((100, 1000)), "total = the first eight columns only");
        // 100 more ticks, 50 of them iowait; the guest columns grow a lot and must not dilute it.
        let pct = iowait_pct("cpu  100 0 100 750 150 0 0 0 9000 9000\n", &mut prev);
        assert!((pct - 50.0).abs() < 1e-9, "{pct}");
    }

    #[test]
    fn a_counter_that_goes_backwards_never_reports_a_negative_rate() {
        let mut prev = None;
        let mut out = System::default();
        let a = "hits 4 1000\nmisses 4 100\nsize 4 1\nmru_ghost_hits 4 10\nmfu_ghost_hits 4 10\n";
        arc_interval(a, 10.0, &mut prev, &mut out);
        // The module was reloaded: every counter restarted from a lower value.
        let b = "hits 4 5\nmisses 4 2\nsize 4 1\nmru_ghost_hits 4 1\nmfu_ghost_hits 4 1\n";
        arc_interval(b, 20.0, &mut prev, &mut out);
        assert!(out.measured);
        assert_eq!(out.arc_miss_per_sec, 0.0);
        assert_eq!(out.arc_ghost_hits_per_sec, 0.0);
        assert_eq!(out.arc_hit_rate_pct, 100.0, "no hits and no misses in the interval is not a 0% hit rate");
    }

    #[test]
    fn arc_ghost_hits_and_demand_misses_add_their_two_halves() {
        let mut prev = None;
        let mut out = System::default();
        let a = "hits 4 0\nmisses 4 0\nsize 4 4096\ndemand_data_misses 4 0\ndemand_metadata_misses 4 0\n\
                 demand_data_hits 4 0\ndemand_metadata_hits 4 0\nmru_ghost_hits 4 0\nmfu_ghost_hits 4 0\n";
        arc_interval(a, 0.0, &mut prev, &mut out);
        let b = "hits 4 30\nmisses 4 10\nsize 4 4096\ndemand_data_misses 4 2\ndemand_metadata_misses 4 6\n\
                 demand_data_hits 4 10\ndemand_metadata_hits 4 10\nmru_ghost_hits 4 3\nmfu_ghost_hits 4 7\n";
        arc_interval(b, 10.0, &mut prev, &mut out);
        assert!((out.arc_demand_miss_per_sec - 0.8).abs() < 1e-9, "(2 + 6) / 10 s");
        assert!((out.arc_ghost_hits_per_sec - 1.0).abs() < 1e-9, "(3 + 7) / 10 s");
        assert!((out.arc_demand_hit_rate_pct - 20.0 / 28.0 * 100.0).abs() < 1e-9);
        assert!((out.arc_hit_rate_pct - 75.0).abs() < 1e-9);
    }

    #[test]
    fn an_arc_read_with_no_elapsed_time_is_not_a_measurement() {
        let mut prev = None;
        let mut out = System::default();
        arc_interval("hits 4 1\nmisses 4 1\nsize 4 8\n", 50.0, &mut prev, &mut out);
        arc_interval("hits 4 9\nmisses 4 9\nsize 4 16\n", 50.0, &mut prev, &mut out);
        assert!(!out.measured, "dt = 0 cannot give a rate");
        assert_eq!(out.arc_size_bytes, 16.0, "the size is a gauge and still follows");
    }

    #[test]
    fn the_fd_count_is_this_processs_own() {
        // At least stdin, stdout, stderr and the directory handle being read.
        assert!(open_fd_count() >= 4.0);
    }

    // ---- the pass over the torrents, against a real (offline) engine host ----

    use crate::api::testing::{state_from, TestState};

    fn torrent_bytes(name: &str, announce: Option<&str>) -> Vec<u8> {
        let mut piece = [0xABu8; 20];
        let n = name.len().min(20);
        piece[..n].copy_from_slice(&name.as_bytes()[..n]);
        let mut info = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:", name.len()).into_bytes();
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = match announce {
            Some(a) => format!("d8:announce{}:{a}4:info", a.len()).into_bytes(),
            None => b"d4:info".to_vec(),
        };
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn add(s: &TestState, engine: &str, name: &str, announce: Option<&str>) -> std::sync::Arc<typhon_engine::torrent::meta::TorrentState> {
        let e = s.engines.get(engine).expect("engine");
        let (ih, _) = e
            .manager
            .add_torrent_bytes(&torrent_bytes(name, announce), "/tmp/hydra-benchsampler-test", false, false)
            .expect("added");
        e.manager.get(&ih).expect("loaded")
    }

    fn tracker_key(engine: &str, host: &str) -> String {
        crate::store::Store::tracker_counter_key(engine, host)
    }

    fn memory_bench() -> crate::benchdb::Shared {
        std::sync::Arc::new(std::sync::Mutex::new(crate::benchdb::BenchDb::open_in_memory().unwrap()))
    }

    /// The Trackers tab is the stored baseline plus what is loaded, one row per
    /// (engine, tracker); a tracker with nothing loaded keeps its stored row,
    /// and a torrent with no announce URL has a bucket of its own.
    #[test]
    fn tracker_rows_add_the_loaded_torrents_to_the_stored_baseline() {
        use std::sync::atomic::Ordering::Relaxed;
        use typhon_engine::torrent::meta::TorrentStatus;
        let s = state_from("bs-rows", "");
        {
            let store = s.store.lock().unwrap();
            store.set_counter(&tracker_key("race", "tk-a.example"), 1000, 500).unwrap();
            store.set_counter(&tracker_key("hoard", "stored-only.example"), 7, 3).unwrap();
        }
        let a1 = add(&s, "race", "a1", Some("https://tk-a.example/announce"));
        a1.total_uploaded.store(100, Relaxed);
        a1.total_downloaded.store(40, Relaxed);
        a1.peers_connected.store(3, Relaxed);
        a1.status.store(TorrentStatus::Downloading as u8, Relaxed);
        let a2 = add(&s, "race", "a2", Some("https://tk-a.example:8443/announce"));
        a2.status.store(TorrentStatus::Seeding as u8, Relaxed);
        let b = add(&s, "race", "b1", Some("https://tk-b.example/announce"));
        b.status.store(TorrentStatus::Seeding as u8, Relaxed);
        b.is_paused.store(true, Relaxed);
        add(&s, "race", "n1", None);
        let h = add(&s, "hoard", "h1", Some("https://tk-a.example/announce"));
        h.status.store(TorrentStatus::Seeding as u8, Relaxed);
        h.peers_connected.store(1, Relaxed);

        let rows = tracker_totals(&s.engines, &s.store, 1_700_000_123);
        let keys: Vec<(&str, &str)> = rows.iter().map(|r| (r.engine.as_str(), r.tracker.as_str())).collect();
        assert_eq!(
            keys,
            vec![
                ("hoard", "stored-only.example"),
                ("hoard", "tk-a.example"),
                ("race", "(no tracker)"),
                ("race", "tk-a.example"),
                ("race", "tk-b.example"),
            ],
            "sorted by engine then tracker, the port stripped from the host"
        );
        assert!(rows.iter().all(|r| r.ts == 1_700_000_123), "every row carries the pass's instant");

        let row = |e: &str, t: &str| rows.iter().find(|r| r.engine == e && r.tracker == t).unwrap().clone();
        let stored_only = row("hoard", "stored-only.example");
        assert_eq!((stored_only.cum_uploaded, stored_only.cum_downloaded, stored_only.torrents), (7, 3, 0));
        let a = row("race", "tk-a.example");
        assert_eq!(a.cum_uploaded, 1100, "stored 1000 + the loaded torrent's 100");
        assert_eq!(a.cum_downloaded, 540);
        assert_eq!(a.torrents, 2, "the torrent count comes from what is loaded");
        assert_eq!((a.peers, a.active), (3, 1), "one of the two has a peer");
        assert_eq!(a.seed_size, 16384, "only the seeding one counts as seed size");
        let b = row("race", "tk-b.example");
        assert_eq!((b.torrents, b.seed_size), (1, 0), "paused keeps its state word but is credited nothing");
        let n = row("race", "(no tracker)");
        assert_eq!((n.torrents, n.seed_size, n.peers), (1, 0, 0));
        let hoard_a = row("hoard", "tk-a.example");
        assert_eq!((hoard_a.torrents, hoard_a.seed_size, hoard_a.peers), (1, 16384, 1));
        assert_eq!(hoard_a.cum_uploaded, 0, "the race engine's stored row is not the hoard's");
    }

    #[test]
    fn a_tracker_pass_is_published_and_stored() {
        let s = state_from("bs-pass", "");
        add(&s, "race", "p1", Some("https://pass-only.example/announce"));
        let bench = memory_bench();
        // The published rows are process-wide and another test's sampler task
        // may publish its own pass between ours and the read: take another
        // pass until the rows read back are the ones we just made.
        let mut live = None;
        for _ in 0..50 {
            sample_trackers(&s.engines, &bench, &s.store).expect("pass");
            live = latest_trackers().filter(|rows| rows.iter().any(|r| r.tracker == "pass-only.example"));
            if live.is_some() {
                break;
            }
        }
        let live = live.expect("the live table shows the pass just made, not nothing or a placeholder row");
        let row = live.iter().find(|r| r.tracker == "pass-only.example").unwrap();
        assert_eq!((row.engine.as_str(), row.torrents), ("race", 1));
        let stored = bench.lock().unwrap().tracker_samples_latest().unwrap();
        let mine: Vec<_> = stored.iter().filter(|r| r["tracker"] == "pass-only.example").collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["engine"], "race");
        assert_eq!(mine[0]["torrents"].as_f64(), Some(1.0));
    }

    /// One 5-second sample: headline gauges from the engines, the extra
    /// engines apart, the schedule's health read from each admission, and the
    /// announce rate differenced between two samples rather than since boot.
    #[test]
    fn a_sample_records_the_engines_and_differences_the_announce_counters() {
        use std::sync::atomic::Ordering::Relaxed;
        let s = state_from(
            "bs-sample",
            "[[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26991\n",
        );
        s.store.lock().unwrap().set_counter("global", 1000, 2000).unwrap();
        let t = add(&s, "race", "s1", Some("https://tk-s.example/announce"));
        t.total_uploaded.store(300, Relaxed);
        add(&s, "vpn1", "x1", None);
        add(&s, "vpn1", "x2", None);
        let race = s.engines.get("race").unwrap();
        race.admission.needed_milli.store(2500, Relaxed);
        race.admission.late.store(4, Relaxed);
        race.admission.lag_p50_s.store(11, Relaxed);
        race.admission.lag_p90_s.store(33, Relaxed);
        race.admission.concurrency.store(16, Relaxed);
        race.admission.latency_ms.store(120, Relaxed);
        race.admission.throttled_permille.store(250, Relaxed);
        race.admission.in_flight.store(7, Relaxed);

        let bench = memory_bench();
        let (mut previous, mut system) = (Previous::new(), SystemPrev::default());
        let (session_up, session_down) = s.engines.session_totals();
        sample_once(&s.engines, &bench, &s.store, &mut previous, &mut system).expect("first sample");

        let rows = bench.lock().unwrap().samples_in_range(0.0, 1e12).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        let f = |k: &str| r[k].as_f64().unwrap_or_else(|| panic!("{k} missing from {r}"));
        assert_eq!(f("race_torrents"), 1.0, "the race engine's own count");
        assert_eq!(f("race_announce_needed"), 2.5, "milli-announces per second to announces per second");
        assert_eq!(f("race_announce_late"), 4.0);
        assert_eq!((f("race_announce_lag_p50"), f("race_announce_lag_p90")), (11.0, 33.0));
        assert_eq!((f("race_announce_concurrency"), f("race_announce_latency_ms")), (16.0, 120.0));
        assert_eq!(f("race_announce_throttled_pct"), 25.0, "permille to percent");
        assert_eq!(f("race_announce_in_flight"), 7.0);
        assert_eq!(f("hoard_announce_needed"), 0.0, "the hoard engine published nothing");
        assert_eq!(f("global_uploaded"), (1000 + session_up) as f64, "the stored baseline plus the loaded torrents");
        assert_eq!(f("global_downloaded"), (2000 + session_down) as f64);
        assert_eq!(f("race_announce_rate"), 0.0, "the first sample has no predecessor to difference against");
        assert!(f("open_fds") >= 4.0);

        // The extra engine has its own row; it is in neither the race nor the hoard column.
        let extra = bench.lock().unwrap().engine_samples_in_range(0.0, 1e12, 1.0).unwrap();
        assert_eq!(extra.keys().collect::<Vec<_>>(), vec!["vpn1"]);
        assert_eq!(extra["vpn1"][0]["torrents"].as_f64(), Some(2.0));

        // Five successful announces and two failures since: rates over the elapsed time.
        for _ in 0..5 {
            race.announce_cache.count_ok("tk-s.example");
        }
        race.announce_cache.count_failed();
        race.announce_cache.count_failed();
        std::thread::sleep(std::time::Duration::from_millis(30));
        sample_once(&s.engines, &bench, &s.store, &mut previous, &mut system).expect("second sample");
        let rows = bench.lock().unwrap().samples_in_range(0.0, 1e12).unwrap();
        assert_eq!(rows.len(), 2);
        let last = &rows[1];
        let ok = last["race_announce_rate"].as_f64().unwrap();
        let failed = last["race_announce_fail_rate"].as_f64().unwrap();
        assert!(ok > 0.0 && failed > 0.0, "the counters moved, so the rates are not zero: {ok} {failed}");
        assert!((ok / failed - 2.5).abs() < 1e-6, "5 ok to 2 failed over the same interval: {ok} / {failed}");
        let iowait = last["iowait_pct"].as_f64().unwrap();
        assert!((0.0..=100.0).contains(&iowait), "a share of the CPU time: {iowait}");
    }

    /// The rates of the loaded torrents are summed per tracker, then per
    /// (engine, tracker) over the stored baseline; `active` counts the
    /// torrents with a peer, not the peers.
    #[test]
    fn tracker_rows_sum_the_rates_and_count_the_torrents_with_a_peer() {
        use std::sync::atomic::Ordering::Relaxed;
        let s = state_from("bs-rates", "");
        s.store.lock().unwrap().set_counter(&tracker_key("race", "tk-r.example"), 10, 20).unwrap();
        let r1 = add(&s, "race", "r1", Some("https://tk-r.example/announce"));
        let r2 = add(&s, "race", "r2", Some("https://tk-r.example/announce"));
        let r3 = add(&s, "race", "r3", Some("https://tk-r.example/announce"));
        let h1 = add(&s, "hoard", "h1", Some("https://tk-r.example/announce"));
        r1.peers_connected.store(3, Relaxed);
        r2.peers_connected.store(5, Relaxed);
        r3.peers_connected.store(0, Relaxed);
        h1.peers_connected.store(2, Relaxed);
        // A rate is measured between two updates: the first only seeds it.
        let all = [(&r1, 6_000, 3_000), (&r2, 12_000, 9_000), (&r3, 0, 0), (&h1, 30_000, 15_000)];
        for (t, _, _) in &all {
            t.upload_rate.update(0);
            t.download_rate.update(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(700));
        for (t, up, down) in &all {
            t.upload_rate.update(*up);
            t.download_rate.update(*down);
        }
        let (u1, u2, d1, d2) = (r1.upload_rate.get(), r2.upload_rate.get(), r1.download_rate.get(), r2.download_rate.get());
        let (uh, dh) = (h1.upload_rate.get(), h1.download_rate.get());
        assert!(u1 > 0 && u2 > 0 && d1 > 0 && d2 > 0 && uh > 0 && dh > 0, "the fixtures do have rates");
        assert_eq!(r3.upload_rate.get(), 0);

        let rows = tracker_totals(&s.engines, &s.store, 1);
        let row = |e: &str| rows.iter().find(|r| r.engine == e && r.tracker == "tk-r.example").unwrap().clone();
        let race = row("race");
        assert_eq!(race.upload_rate, (u1 + u2) as i64, "the loaded torrents' upload rates add up");
        assert_eq!(race.download_rate, (d1 + d2) as i64);
        assert_eq!((race.torrents, race.peers), (3, 8));
        assert_eq!(race.active, 2, "two of the three torrents have a peer; the peers do not count");
        assert_eq!((race.cum_uploaded, race.cum_downloaded), (10, 20), "the stored baseline is kept under them");
        let hoard = row("hoard");
        assert_eq!((hoard.upload_rate, hoard.download_rate), (uh as i64, dh as i64));
        assert_eq!((hoard.torrents, hoard.peers, hoard.active), (1, 2, 1));
    }

    /// `/proc/stat` with exactly the five fields up to iowait is readable.
    #[test]
    fn iowait_reads_a_cpu_line_that_stops_at_iowait() {
        let mut prev = None;
        assert_eq!(iowait_pct("cpu  10 0 10 70 10\n", &mut prev), 0.0);
        assert_eq!(prev, Some((10, 100)), "five fields are enough to be the baseline");
        let pct = iowait_pct("cpu  10 0 10 90 30\n", &mut prev);
        assert!((pct - 50.0).abs() < 1e-9, "20 of the 40 new ticks were iowait: {pct}");
    }

    /// The hit rate is hits over hits plus misses, whatever the balance
    /// between them; no hit at all is 0 % when there were misses.
    #[test]
    fn the_arc_hit_rate_holds_when_the_misses_outnumber_the_hits() {
        let run = |h: u32, m: u32| {
            let (mut prev, mut out) = (None, System::default());
            arc_interval("hits 4 100\nmisses 4 100\nsize 4 1\ndemand_data_hits 4 100\ndemand_data_misses 4 100\n", 0.0, &mut prev, &mut out);
            let b = format!(
                "hits 4 {}\nmisses 4 {}\nsize 4 1\ndemand_data_hits 4 {}\ndemand_data_misses 4 {}\n",
                100 + h, 100 + m, 100 + h, 100 + m
            );
            arc_interval(&b, 10.0, &mut prev, &mut out);
            out
        };
        let few_hits = run(10, 90);
        assert!((few_hits.arc_hit_rate_pct - 10.0).abs() < 1e-9, "{}", few_hits.arc_hit_rate_pct);
        assert!((few_hits.arc_demand_hit_rate_pct - 10.0).abs() < 1e-9);
        assert!((few_hits.arc_miss_per_sec - 9.0).abs() < 1e-9);
        let none = run(0, 50);
        assert_eq!(none.arc_hit_rate_pct, 0.0, "every access missed");
        assert_eq!(none.arc_demand_hit_rate_pct, 0.0);
        let all = run(50, 0);
        assert_eq!(all.arc_hit_rate_pct, 100.0);
    }

    /// An announce rate is the counter's delta over the real time since the
    /// previous sample -- not a ratio, not a delta over nothing -- and a
    /// predecessor stamped in the future (a clock that stepped back) gives 0.
    #[test]
    fn the_announce_rate_is_the_delta_over_the_elapsed_seconds() {
        let s = state_from("bs-rate", "");
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        let (race, hoard) = (s.engines.get("race").unwrap(), s.engines.get("hoard").unwrap());
        for _ in 0..500 {
            race.announce_cache.count_ok("tk-s.example");
        }
        for _ in 0..100 {
            race.announce_cache.count_failed();
        }
        for _ in 0..50 {
            hoard.announce_cache.count_ok("tk-s.example");
        }
        let bench = memory_bench();
        let (mut previous, mut system) = (Previous::new(), SystemPrev::default());
        previous.insert("race".into(), (0, 0, now - 100.0));
        previous.insert("hoard".into(), (0, 0, now + 1.0e6));
        sample_once(&s.engines, &bench, &s.store, &mut previous, &mut system).expect("sample");
        let rows = bench.lock().unwrap().samples_in_range(0.0, 1e12).unwrap();
        let r = &rows[0];
        let ok = r["race_announce_rate"].as_f64().unwrap();
        let failed = r["race_announce_fail_rate"].as_f64().unwrap();
        assert!(ok > 4.5 && ok <= 5.0, "500 announces over ~100 s: {ok}");
        assert!(failed > 0.9 && failed <= 1.0, "100 failures over ~100 s: {failed}");
        assert_eq!(r["hoard_announce_rate"].as_f64(), Some(0.0), "a predecessor from the future is not a measurement");
        assert_eq!(previous["hoard"].0, 50, "and it is replaced by this sample as the baseline");
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Captured {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Captured {
            self.clone()
        }
    }

    /// A pass that took milliseconds is a debug line, not the warning for a
    /// pass past the slow threshold. Run alone in a child process: `tracing`
    /// caches per call site whether anyone listens.
    #[test]
    fn a_quick_tracker_pass_is_logged_quietly() {
        const NAME: &str = "benchsampler::tests::a_quick_tracker_pass_is_logged_quietly";
        if std::env::var_os("TYPHON_LOG_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
                .env("TYPHON_LOG_CHILD", "1")
                .output()
                .unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && text.contains("1 passed"), "{text}");
            return;
        }
        let s = state_from("bs-log", "");
        add(&s, "race", "l1", Some("https://tk-log.example/announce"));
        let bench = memory_bench();
        let out = Captured::default();
        let sub = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).with_ansi(false).with_writer(out.clone()).finish();
        let _g = tracing::subscriber::set_default(sub);
        sample_trackers(&s.engines, &bench, &s.store).expect("pass");
        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        assert!(text.contains("DEBUG") && text.contains("tracker pass"), "the pass says how long it took: {text}");
        assert!(!text.contains("WARN") && !text.contains("slow"), "a fast pass is no warning: {text}");
    }

    /// The task itself: the first tick is immediate, so a sample and a tracker
    /// pass both land without waiting for the 5 s interval.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sampler_task_writes_its_first_rows_at_once() {
        let s = state_from("bs-spawn", "");
        add(&s, "race", "z1", Some("https://tk-spawn.example/announce"));
        let bench = memory_bench();
        spawn(s.engines.clone(), bench.clone(), s.store.clone());
        let mut got = (0, 0);
        for _ in 0..100 {
            got = {
                let db = bench.lock().unwrap();
                (
                    db.samples_in_range(0.0, 1e12).unwrap().len(),
                    db.tracker_samples_latest().unwrap().iter().filter(|r| r["tracker"] == "tk-spawn.example").count(),
                )
            };
            if got.0 >= 1 && got.1 >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(got, (1, 1), "one sample row and one row for the tracker, from the very first tick");
    }

    /// A tracker pass is reported as slow only strictly over its limit.
    #[test]
    fn a_tracker_pass_is_slow_only_past_its_limit() {
        assert!(!slow_tracker_pass(Duration::from_secs(1)), "a steady-state pass");
        assert!(!slow_tracker_pass(TRACKER_PASS_SLOW), "exactly at the limit");
        assert!(slow_tracker_pass(TRACKER_PASS_SLOW + Duration::from_nanos(1)), "just past it");
        assert!(slow_tracker_pass(TRACKER_PASS_SLOW * 3), "well past it");
    }
}
