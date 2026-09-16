//! Recording what happens to a race torrent.
//!
//! The race timeline exists to answer one question after the fact: how fast did
//! this release go from added to seeding, and what did it manage to upload.
//! That cannot be reconstructed later -- rates and peer counts are
//! instantaneous -- so it is sampled while it happens, and written to
//! `bench.db` rather than kept in memory: a timeline that dies with the process
//! cannot answer a question asked tomorrow.
//!
//! The decisions about WHAT counts as an event live in `benchdb::RaceRecorder`.
//! This module only supplies sightings. (That sentence used to end "where they
//! are unit-tested" -- `RaceRecorder::new` appeared in no test at all until
//! 2026-09-16. It does now.)

use std::collections::HashSet;
use std::sync::Arc;

/// How often the recorder looks. Fast enough that a download time is accurate
/// to the tick, cheap enough to be free next to the engine itself.
const TICK: std::time::Duration = std::time::Duration::from_secs(5);

/// How many peers a snapshot keeps. The panel shows ten, sorted by download
/// rate, and 3.x kept the same number for the same reason: a swarm of 200 would
/// be 200 rows per torrent every 5s, stored for nothing.
const PEER_ROWS: usize = 10;

/// The peer rows the timeline panel reads, as its own JSON shape.
///
/// `dispatch::peers_json` publishes `dl_rate`/`ul_rate`; the panel reads
/// `dl_speed`. Renaming here rather than in the shared function keeps the live
/// peer panel's payload untouched.
fn peers_for(torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>) -> String {
    let Some(mut peers) = typhon_engine::rpc::dispatch::peers_json(torrent)
        .as_array()
        .cloned()
    else {
        return String::new();
    };
    if peers.is_empty() {
        return String::new();
    }
    // Fastest first, then keep the head: the ones worth looking at after the
    // fact are the ones that actually fed the race.
    peers.sort_by(|a, b| {
        let rate = |v: &serde_json::Value| v.get("dl_rate").and_then(|r| r.as_i64()).unwrap_or(0);
        rate(b).cmp(&rate(a))
    });
    peers.truncate(PEER_ROWS);
    let rows: Vec<serde_json::Value> = peers
        .iter()
        .map(|p| {
            let get = |k: &str| p.get(k).cloned().unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "ip": get("ip"),
                "client": get("client"),
                "dl_speed": get("dl_rate"),
                "ul_speed": get("ul_rate"),
                "progress": get("progress"),
                "flags": get("flags"),
            })
        })
        .collect();
    serde_json::to_string(&rows).unwrap_or_default()
}

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
                        // ⚠ The per-peer detail, which the panel's peer table
                        // and the per-peer download column both read. Left
                        // empty on 2026-09-16 as "a cost the graph does not
                        // need" -- it is exactly what the panel is for, and
                        // every one of the 14516 snapshots taken that evening
                        // was useless for it.
                        //
                        // Only for torrents still downloading, which is already
                        // this branch: that is a few dozen rows in the race
                        // engine, not the catalogue.
                        //
                        // ⚠⚠ `peers_json` SAMPLES the rate trackers: it calls
                        // `update()`, which swaps each peer's last total before
                        // its own 0.5s guard. The live peer panel calls the same
                        // function, so the two share one sampling point. At a
                        // steady 5s this recorder becomes the regular sampler
                        // and the panel reads a 5s window instead of its own --
                        // steadier, but no longer its own. Say so rather than
                        // discover it later.
                        peers_json: peers_for(torrent),
                    };
                    let db = match bench.lock() {
                        Ok(db) => db,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    if let Err(e) = db.record_snapshot(&snap) {
                        tracing::warn!(info_hash = %snap.info_hash, "race snapshot not recorded: {e}");
                    }
                }

                // A tick can cross several moments at once (a first peer and a
                // first byte of upload between two looks), so this is a list.
                let moments = recorder.sight(
                    &info_hash,
                    added_time,
                    progress,
                    num("num_peers"),
                    num("total_upload"),
                    // Stamped by the announce runner on every successful
                    // announce; a new one means the tracker just answered.
                    engine.announce_cache.get(&info_hash).map(|e| e.at),
                    now,
                );
                if moments.is_empty() {
                    continue;
                }

                for (kind, ts, download_time) in moments {
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
