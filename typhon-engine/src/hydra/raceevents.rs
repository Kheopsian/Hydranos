//! Recording what happens to a race torrent.
//!
//! The race timeline exists to answer one question after the fact: how fast did
//! this release go from added to seeding, and what did it manage to upload.
//! That cannot be reconstructed later -- rates and peer counts are
//! instantaneous -- so it is sampled while it happens, and written to
//! `bench.db` rather than kept in memory: a timeline that dies with the process
//! cannot answer a question asked tomorrow.
//!
//! The decisions about WHAT counts as an event live in `benchdb::RaceRecorder`,
//! where they are unit-tested. This module only supplies sightings.

use std::collections::HashSet;
use std::sync::Arc;

/// How often the recorder looks. Fast enough that a download time is accurate
/// to the tick, cheap enough to be free next to the engine itself.
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Watch the race engine and record lifecycle events into the measurement db.
pub fn spawn(engines: Arc<crate::engines::EngineHost>, bench: crate::benchdb::Shared) {
    let started_at = now_secs();
    tokio::spawn(async move {
        let mut recorder = crate::benchdb::RaceRecorder::new(started_at);
        loop {
            tokio::time::sleep(TICK).await;

            let Some(engine) = engines.get("race") else {
                continue;
            };
            let now = now_secs();
            let mut live: HashSet<String> = HashSet::new();

            for torrent in engine.manager.all().iter() {
                let row = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
                let num = |k: &str| row.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
                let text = |k: &str| {
                    row.get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };

                let info_hash = text("info_hash");
                if info_hash.is_empty() {
                    continue;
                }
                live.insert(info_hash.clone());

                let added_time = num("added_time") as f64;
                let progress = row
                    .get("progress")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);

                // Sampled BEFORE the lifecycle check, because most ticks have
                // no event to report and those are exactly the ticks the graph
                // is made of. A race that is still downloading is the only one
                // worth sampling: past completion the curve is flat, the front
                // truncates it anyway, and at 5s a tick the whole race engine
                // would otherwise write hundreds of rows a minute for ever.
                if progress < 1.0 {
                    let snap = crate::benchdb::RaceSnapshot {
                        ts: now,
                        info_hash: info_hash.clone(),
                        progress,
                        upload_rate: num("upload_rate") as f64,
                        download_rate: num("download_rate") as f64,
                        total_upload: num("total_upload"),
                        total_download: num("total_download"),
                        peers: num("num_peers"),
                        seeds: num("list_seeds"),
                        swarm_seeds: num("list_seeds"),
                        swarm_leechers: num("list_peers"),
                        ratio: row.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.0),
                        // The per-peer detail is not on the list row, and
                        // asking the engine per torrent every 5s is a cost the
                        // graph does not need. Empty publishes no key.
                        peers_json: String::new(),
                    };
                    let db = match bench.lock() {
                        Ok(db) => db,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if let Err(e) = db.record_snapshot(&snap) {
                        tracing::warn!(info_hash = %snap.info_hash, "race snapshot not recorded: {e}");
                    }
                }

                let Some((kind, ts, download_time)) =
                    recorder.sight(&info_hash, added_time, progress, now)
                else {
                    continue;
                };

                let event = crate::benchdb::RaceEvent {
                    ts,
                    info_hash: info_hash.clone(),
                    event: kind,
                    name: text("name"),
                    size: num("total_size"),
                    download_time,
                    upload_total: num("total_upload"),
                    download_total: num("total_download"),
                    upload_rate: num("upload_rate") as f64,
                    download_rate: num("download_rate") as f64,
                    peers: num("num_peers"),
                    seeds: num("list_seeds"),
                    swarm_seeds: num("list_seeds"),
                    swarm_leechers: num("list_peers"),
                    category: text("category"),
                    // Seconds since the torrent was added, not an absolute
                    // stamp: the panel shows "seeding 4h12 after being added",
                    // and a stamp would make that depend on the reader clock.
                    time_since_add: if added_time > 0.0 { ts - added_time } else { 0.0 },
                    // The recorder never knows an uploader or an injected peer:
                    // both come from the injector's own writes, and both are
                    // omitempty, so leaving them blank publishes no key.
                    uploader: String::new(),
                    injected_peers: 0,
                };

                let db = match bench.lock() {
                    Ok(db) => db,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Err(e) = db.record(&event) {
                    tracing::warn!(info_hash = %event.info_hash, "race event not recorded: {e}");
                }
            }

            recorder.prune(&live);
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_tick_stays_in_the_useful_band() {
        // Faster burns CPU on a six-figure library for no extra precision;
        // slower and a fast race completes between two looks, losing its
        // download time entirely.
        let secs = super::TICK.as_secs();
        assert!((1..=10).contains(&secs), "tick de {secs}s hors bande");
    }
}
