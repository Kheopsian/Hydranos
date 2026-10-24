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
            if !crate::shutdown::sleep(TICK).await {
                return;
            }

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
                let progress = row.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0);

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
                        time_since_add: if added_time > 0.0 {
                            ts - added_time
                        } else {
                            0.0
                        },
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

    use super::*;
    use crate::api::testing::{state_from, TestState};
    use std::sync::atomic::Ordering::Relaxed;
    use typhon_engine::torrent::meta::{PeerStats, TorrentState};

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut piece = [0xABu8; 20];
        piece[..name.len().min(20)].copy_from_slice(&name.as_bytes()[..name.len().min(20)]);
        let mut info = format!(
            "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:",
            name.len()
        )
        .into_bytes();
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = b"d4:info".to_vec();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn add_race(s: &TestState, name: &str) -> (String, Arc<TorrentState>) {
        let race = s.engines.get("race").expect("race engine");
        let (ih, _) = race
            .manager
            .add_torrent_bytes(
                &torrent_bytes(name),
                "/tmp/hydra-raceevents-test",
                false,
                false,
            )
            .expect("added");
        (
            typhon_engine::torrent::hex_encode(&ih),
            race.manager.get(&ih).expect("loaded"),
        )
    }

    fn peer(t: &TorrentState, n: u8) -> Arc<PeerStats> {
        let addr: std::net::SocketAddr = format!("192.0.2.{n}:6881").parse().unwrap();
        let p = Arc::new(PeerStats::new(
            addr,
            [n; 20],
            format!("TestClient {n}"),
            false,
            false,
        ));
        t.peer_stats.insert(addr, p.clone());
        p
    }

    #[test]
    fn a_torrent_with_no_peer_has_no_peer_rows() {
        let s = state_from("re-nopeers", "");
        let (_, t) = add_race(&s, "np");
        assert_eq!(
            peers_for(&t),
            "",
            "nothing to store, not an empty JSON array"
        );
    }

    /// The panel reads `dl_speed`/`ul_speed`, the live peer table `dl_rate`/
    /// `ul_rate`: the stored rows use the panel's names, keep the ten fastest
    /// and nothing else of the live payload.
    #[test]
    fn the_stored_peer_rows_are_the_ten_fastest_under_the_panels_names() {
        let s = state_from("re-peers", "");
        let (_, t) = add_race(&s, "pp");
        let peers: Vec<_> = (1..=12u8).map(|n| peer(&t, n)).collect();
        peers[4].is_seed.store(true, Relaxed);
        peers[4].interested.store(true, Relaxed);
        peers[4].choked.store(false, Relaxed);
        peers[4].num_pieces_have.store(1, Relaxed);
        // The first sample only seeds the rate trackers; the second, after the
        // 0.5 s guard, measures: peer n has downloaded n MB since.
        typhon_engine::rpc::dispatch::peers_json(&t);
        std::thread::sleep(std::time::Duration::from_millis(700));
        for (i, p) in peers.iter().enumerate() {
            p.total_downloaded
                .store((i as u64 + 1) * 1_000_000, Relaxed);
        }
        peers[0].total_uploaded.store(5_000_000, Relaxed);

        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&peers_for(&t)).expect("a JSON array");
        assert_eq!(
            rows.len(),
            PEER_ROWS,
            "the head of the swarm, not all twelve"
        );
        let ips: Vec<&str> = rows.iter().map(|r| r["ip"].as_str().unwrap()).collect();
        assert_eq!(ips[0], "192.0.2.12", "fastest first");
        assert_eq!(ips[9], "192.0.2.3");
        assert!(
            !ips.contains(&"192.0.2.1") && !ips.contains(&"192.0.2.2"),
            "the two slowest are dropped: {ips:?}"
        );
        let speeds: Vec<u64> = rows
            .iter()
            .map(|r| r["dl_speed"].as_u64().unwrap())
            .collect();
        assert!(
            speeds.windows(2).all(|w| w[0] >= w[1]),
            "sorted by download rate: {speeds:?}"
        );
        let five = rows.iter().find(|r| r["ip"] == "192.0.2.5").unwrap();
        assert_eq!(five["client"], "TestClient 5");
        assert_eq!(five["flags"], "iUS", "interested, unchoked, seed");
        assert_eq!(five["progress"].as_f64(), Some(1.0), "its one piece of one");
        assert_eq!(
            five.as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["client", "dl_speed", "flags", "ip", "progress", "ul_speed"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            "exactly the panel's columns"
        );
    }

    #[test]
    fn now_is_unix_seconds_with_a_fraction() {
        let a = now_secs();
        assert!(a > 1_600_000_000.0, "{a}");
        assert!(now_secs() >= a);
    }

    /// The recorder end to end, against a real race engine and a real bench
    /// database: a torrent that appears while it watches, with a peer and
    /// some upload, gets its three firsts on the next look, plus one snapshot
    /// with the peer detail the panel needs. The tick is five seconds and is
    /// not injectable, so this waits for the first one.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_recorder_writes_the_moments_it_witnesses_and_a_snapshot() {
        let s = state_from("re-spawn", "");
        let bench: crate::benchdb::Shared = Arc::new(std::sync::Mutex::new(
            crate::benchdb::BenchDb::open_in_memory().unwrap(),
        ));
        spawn(s.engines.clone(), bench.clone());
        // After the watch began, and in a later whole second than its start: the
        // torrent's added time is in whole seconds.
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        let (hash, t) = add_race(&s, "rr");
        peer(&t, 9);
        t.peers_connected.store(1, Relaxed);
        t.total_uploaded.store(4096, Relaxed);

        let mut events = Vec::new();
        for _ in 0..120 {
            events = bench.lock().unwrap().events_for(&hash).unwrap();
            if events.len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let mut kinds: Vec<&str> = events.iter().map(|e| e.event.as_str()).collect();
        kinds.sort();
        assert_eq!(kinds, vec!["added", "first_peer", "first_upload"]);
        let added = events.iter().find(|e| e.event == "added").unwrap();
        assert_eq!(
            (
                added.name.as_str(),
                added.size,
                added.upload_total,
                added.peers
            ),
            ("rr", 16384, 4096, 1)
        );
        assert!(
            (0.0..60.0).contains(&added.time_since_add),
            "seconds since it was added: {}",
            added.time_since_add
        );
        // Exactly the event's instant minus the torrent's own added time (whole
        // seconds, so always short of the real moment): neither a ratio nor 0.
        let added_time = typhon_engine::rpc::dispatch::torrent_to_json(&t)["added_time"]
            .as_i64()
            .unwrap() as f64;
        assert!(added_time > 0.0);
        assert!(added.time_since_add > 0.0, "{}", added.time_since_add);
        assert!(
            (added.time_since_add - (added.ts - added_time)).abs() < 1e-6,
            "{} vs {} - {}",
            added.time_since_add,
            added.ts,
            added_time
        );
        assert_eq!((added.uploader.as_str(), added.injected_peers), ("", 0));

        let snaps = bench.lock().unwrap().snapshots_for(&hash).unwrap();
        assert_eq!(
            snaps.len(),
            1,
            "one look, one snapshot while it is still downloading"
        );
        assert_eq!(snaps[0].progress, 0.0);
        assert_eq!((snaps[0].peers, snaps[0].total_upload), (1, 4096));
        let detail: Vec<serde_json::Value> =
            serde_json::from_str(&snaps[0].peers_json).expect("peer detail kept");
        assert_eq!(detail.len(), 1);
        assert_eq!(detail[0]["ip"], "192.0.2.9");
    }

    /// Only a race still downloading is sampled: one that is exactly complete
    /// is past the part of the curve worth keeping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_race_that_is_exactly_complete_is_not_sampled() {
        use typhon_engine::torrent::meta::TorrentStatus;
        let s = state_from("re-complete", "");
        let bench: crate::benchdb::Shared = Arc::new(std::sync::Mutex::new(
            crate::benchdb::BenchDb::open_in_memory().unwrap(),
        ));
        spawn(s.engines.clone(), bench.clone());
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        let (done, d) = add_race(&s, "cd");
        d.status.store(TorrentStatus::Seeding as u8, Relaxed);
        peer(&d, 8);
        d.peers_connected.store(1, Relaxed);
        let (running, r) = add_race(&s, "dl");
        peer(&r, 7);
        r.peers_connected.store(1, Relaxed);

        // The first peer of each is the marker that the look has been made: a
        // look writes its snapshot before it reports a moment.
        let mut seen = (false, false);
        for _ in 0..120 {
            let db = bench.lock().unwrap();
            let has = |h: &str| {
                db.events_for(h)
                    .unwrap()
                    .iter()
                    .any(|e| e.event == "first_peer")
            };
            seen = (has(&done), has(&running));
            drop(db);
            if seen == (true, true) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(seen, (true, true), "both were looked at");
        let db = bench.lock().unwrap();
        assert_eq!(
            db.snapshots_for(&running).unwrap().len(),
            1,
            "the one still downloading is sampled"
        );
        assert!(
            db.snapshots_for(&done).unwrap().is_empty(),
            "progress 1.0 is not below 1.0"
        );
    }
}
