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

pub fn spawn(engines: Arc<EngineHost>, bench: Shared, store: Arc<std::sync::Mutex<Store>>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(INTERVAL);
        // A sampler that fell behind must not then burst: the graph would show
        // several rows sharing one instant.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous: Previous = std::collections::HashMap::new();
        loop {
            tick.tick().await;
            if let Err(e) = sample_once(&engines, &bench, &store, &mut previous) {
                tracing::warn!("bench sample failed: {e}");
            }
        }
    });
}

fn sample_once(
    engines: &Arc<EngineHost>,
    bench: &Shared,
    store: &Arc<std::sync::Mutex<Store>>,
    previous: &mut Previous,
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
