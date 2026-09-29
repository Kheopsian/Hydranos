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
        loop {
            tick.tick().await;
            if let Err(e) = sample_once(&engines, &bench, &store, &mut previous, &mut system) {
                tracing::warn!("bench sample failed: {e}");
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
    let (hoard_up, _hoard_down, hoard_peers, _hoard_torrents, hoard_with, hoard_uploading) =
        gauges("hoard");

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
    });

    let db = bench.lock().unwrap_or_else(|e| e.into_inner());
    db.record_sample(&sample)?;

    // Per-tracker rows, from the announce cache: it is the only place that
    // knows which tracker a torrent actually talks to.
    for engine in engines.engines() {
        for (host, (torrents, _age)) in engine.announce_cache.per_tracker() {
            db.record_tracker_sample(
                ts,
                &engine.id,
                &host,
                0.0,
                0.0,
                torrents as f64,
                0,
                0,
            )?;
        }
    }
    Ok(())
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
}
