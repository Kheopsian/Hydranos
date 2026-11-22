use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        name.chars().next().unwrap().to_string().repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// `name` held by `engine`, with a store row in that engine's session
/// carrying `category`.
fn hold(s: &TestState, engine: &str, name: &str, category: &str) -> String {
    let blob = torrent_bytes(name);
    let (ih, _) = s
        .state
        .engines
        .get(engine)
        .unwrap()
        .manager
        .add_torrent_bytes(&blob, "/tmp", true, true)
        .expect("added");
    let hash = typhon_engine::torrent::hex_encode(&ih);
    s.state
        .store
        .lock()
        .unwrap()
        .insert_torrent(&hash, engine, &blob, "/tmp", category, 0.0, false, "")
        .unwrap();
    hash
}

async fn get(s: &TestState, uri: &str) -> serde_json::Value {
    let req = axum::http::Request::builder()
        .uri(uri)
        .header("X-API-Key", KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    body_json(resp).await
}

/// ⭐⭐ The panel depends on its own row and on no other. A neighbour in the
/// same session whose row cannot be read used to fail the whole-session
/// read, and the healthy torrent's panel lost its category with it.
#[tokio::test]
async fn a_torrent_panel_does_not_depend_on_the_other_rows_of_its_session() {
    let s = state_from(
        "detail-own-row",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let hash = hold(&s, "hoard", "alpha", "movies");
    let neighbour = "b".repeat(40);
    {
        let store = s.state.store.lock().unwrap();
        store
            .insert_torrent(&neighbour, "hoard", b"x", "", "", 0.0, false, "")
            .unwrap();
        store.execute_batch_for_tests(&format!(
            "UPDATE torrents SET added_time = 'not a number' WHERE info_hash = '{neighbour}'"
        ));
        assert!(
            store.facts_by_session("hoard").is_err(),
            "the fixture: the neighbour row must be unreadable as a session"
        );
    }
    let body = get(&s, &format!("/api/hoard/torrents/{hash}")).await;
    assert_eq!(
        body["category"], "movies",
        "the panel keeps its own row's category"
    );
}

/// A hoard torrent with a readable row of its own, beside a neighbour in
/// the same session whose row cannot be read: `facts_by_session` fails on
/// that session, `facts_for_hashes` of the healthy torrent does not. The
/// healthy row's `added_time` is 1234567, a value the engine cannot make
/// up (it stamps the torrent with the clock when it is added).
fn beside_an_unreadable_neighbour(s: &TestState) -> String {
    let hash = hold(s, "hoard", "alpha", "movies");
    let neighbour = "b".repeat(40);
    let store = s.state.store.lock().unwrap();
    store
        .insert_torrent(&neighbour, "hoard", b"x", "", "", 0.0, false, "")
        .unwrap();
    store.execute_batch_for_tests(&format!(
        "UPDATE torrents SET added_time = 1234567 WHERE info_hash = '{hash}';
         UPDATE torrents SET added_time = 'not a number' WHERE info_hash = '{neighbour}'"
    ));
    assert!(
        store.facts_by_session("hoard").is_err(),
        "the fixture: the neighbour row must be unreadable as a session"
    );
    hash
}

/// ⭐⭐ The qBittorrent properties of a torrent come from its own row and
/// from no other. `addition_date` is the store's `added_time`, so it is
/// what a read of the wrong rows loses: the engine only knows the clock.
#[tokio::test]
async fn the_qbit_properties_of_a_torrent_do_not_depend_on_the_other_rows_of_its_session() {
    let s = state_from(
        "qbit-properties-own-row",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let hash = beside_an_unreadable_neighbour(&s);
    let body = get(&s, &format!("/api/v2/torrents/properties?hash={hash}")).await;
    assert_eq!(
        body["addition_date"], 1_234_567,
        "the panel keeps its own row's addition date"
    );
}

/// The same for the listing asked for named hashes: the category is the
/// store's, and a neighbour's unreadable row does not take it away.
#[tokio::test]
async fn a_qbit_listing_of_named_hashes_does_not_depend_on_the_other_rows_of_its_session() {
    let s = state_from(
        "qbit-info-own-row",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let hash = beside_an_unreadable_neighbour(&s);
    let body = get(&s, &format!("/api/v2/torrents/info?hashes={hash}")).await;
    assert_eq!(body.as_array().map(Vec::len), Some(1));
    assert_eq!(
        body[0]["category"], "movies",
        "the row keeps its own category"
    );
    assert_eq!(body[0]["added_on"], 1_234_567);
}

/// An install whose second engine of each role has an id that is not the
/// role. Each engine has its own session in the store.
fn tunnelled(tag: &str) -> TestState {
    state_from(
        tag,
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n\
             [[engine]]\nengine_id = \"hoard2\"\nrole = \"hoard\"\n\
             [engine.session]\nlisten_port = 26981\n\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n\
             [engine.session]\nlisten_port = 26982\n"
        ),
    )
}

/// ⭐⭐ The panel reads the session of the engine that carries the torrent,
/// not the one called after the role. A node may run several hoard
/// engines; the panel of a torrent held by `hoard2` was filled from the
/// session named "hoard" and showed no category.
#[tokio::test]
async fn a_hoard_panel_reads_the_session_of_the_engine_holding_the_torrent() {
    let s = tunnelled("detail-hoard2");
    assert_eq!(
        s.state.engines.get("hoard2").expect("fixture engine").role,
        "hoard"
    );
    let hash = hold(&s, "hoard2", "gamma", "tv");
    let body = get(&s, &format!("/api/hoard/torrents/{hash}?engine=hoard2")).await;
    assert_eq!(body["category"], "tv");
}

/// The same on the race side, which passed the literal "race".
#[tokio::test]
async fn a_race_panel_reads_the_session_of_the_engine_holding_the_torrent() {
    let s = tunnelled("detail-vpn1");
    assert_eq!(
        s.state.engines.get("vpn1").expect("fixture engine").role,
        "race"
    );
    let hash = hold(&s, "vpn1", "delta", "Race");
    let body = get(&s, &format!("/api/race/torrents/{hash}?engine=vpn1")).await;
    assert_eq!(body["category"], "Race");
}

// ---- Same panel as before the fast path -------------------------------
//
// a5d18acd swapped the whole-session read for a by-key read, and 61555df0
// changed which session the routes pass. Faster and the same size does not
// prove the same content, so the panel the route serves now is compared,
// field by field, with the one the code before a5d18acd built from the
// same engine state and the same store.

/// `detail_payload` as it stood at a5d18acd^, verbatim but for its name:
/// the whole session read into a map, one entry kept. The only line that
/// differs from today's is the facts read.
fn detail_payload_before(
    state: &AppState,
    engine_id: &str,
    hash: &str,
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    admission: &crate::announce::scheduler::Admission,
) -> serde_json::Value {
    let raw = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
    let facts = {
        let store = state.store.read().unwrap();
        store.facts_by_session(engine_id).unwrap_or_default()
    };
    let empty = crate::row::StoreFacts::default();
    let row = crate::row::build(&raw, facts.get(hash).unwrap_or(&empty), "");

    let i = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let total_download = i(&row, "total_download");
    let total_upload = i(&row, "total_upload");
    let ratio = row.get("ratio").and_then(|v| v.as_f64()).unwrap_or(0.0);

    serde_json::json!({
        "active_time": 0,
        "added_time": i(&row, "added_time"),
        "avg_download_rate": 0,
        "avg_upload_rate": 0,
        "category": row.get("category").cloned().unwrap_or_else(|| "".into()),
        "completed_time": i(&row, "completed_time"),
        "connections_limit": 0,
        "download_rate": i(&row, "download_rate"),
        "engine_save_path": row.get("engine_save_path").cloned().unwrap_or_else(|| "".into()),
        "info_hash": hash,
        "list_peers": i(&raw, "list_peers"),
        "list_seeds": i(&raw, "list_seeds"),
        "multi_file": row.get("multi_file").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "name": row.get("name").cloned().unwrap_or_else(|| "".into()),
        "num_peers": i(&row, "num_peers"),
        "num_pieces": i(&raw, "num_pieces"),
        "num_seeds": i(&row, "num_seeds"),
        "peers": typhon_engine::rpc::dispatch::peers_json(torrent),
        "piece_length": torrent.meta.piece_length,
        "pieces_avail": serde_json::Value::Null,
        "pieces_have": serde_json::Value::Null,
        "progress": row.get("progress").cloned().unwrap_or(serde_json::json!(0)),
        "ratio": crate::row::num_json(ratio),
        "ratio_efficiency": 0,
        "save_path": row.get("save_path").cloned().unwrap_or_else(|| "".into()),
        "seeding_time": i(&row, "seeding_time"),
        "state": row.get("state").cloned().unwrap_or_else(|| "".into()),
        "swarm_leechers": i(&row, "swarm_leechers"),
        "swarm_seeds": i(&row, "swarm_seeds"),
        "torrent_error": row.get("torrent_error").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "torrent_error_msg": row.get("torrent_error_msg").cloned().unwrap_or_else(|| "".into()),
        "total_done": i(&raw, "total_done"),
        "total_download": total_download,
        "total_size": i(&row, "total_size"),
        "total_upload": total_upload,
        "tracker_error": row.get("tracker_error").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "tracker_host": row.get("tracker_host").cloned().unwrap_or_else(|| "".into()),
        "trackers": tracker_rows(torrent, admission),
        "upload_rate": i(&row, "upload_rate"),
        "uploads_limit": 0,
        "upload_limit": torrent.rate_limits().0,
        "download_limit": torrent.rate_limits().1,
        "engine_upload_limit": torrent.engine_rates().engine.up.rate(),
        "engine_download_limit": torrent.engine_rates().engine.down.rate(),
        "sequential": torrent.download_order_flags().0,
        "first_last_piece": torrent.download_order_flags().1,
        "seed_only": torrent.is_seed_only(),
        "share_limits": crate::sharelimits::native_json(
            engine_id,
            &state.store.read().map(|s| s.share_limits_of(hash)).unwrap_or_default(),
            &crate::sharelimits::engine_limits(state, engine_id),
        ),
        "tag_since": state
            .store
            .read()
            .ok()
            .and_then(|s| s.tag_since(hash).ok())
            .unwrap_or_default(),
    })
}

/// Two files, three trackers over two tiers: enough for `multi_file`,
/// `num_pieces`, `total_size` and the tracker list to carry information.
fn multi_torrent(name: &str) -> Vec<u8> {
    let fill = name.chars().next().unwrap().to_string().repeat(40);
    let info = format!(
        "d5:filesld6:lengthi16384e4:pathl5:a.bineed6:lengthi8192e4:pathl5:b.bineee\
         4:name{}:{name}12:piece lengthi16384e6:pieces40:{fill}e",
        name.len()
    );
    let t = |u: &str| format!("{}:{u}", u.len());
    let (a, b, c) = (
        "https://one.example/announce",
        "https://two.example/announce",
        "udp://three.example:6969/announce",
    );
    format!(
        "d8:announce{}13:announce-listll{}{}el{}ee4:info{info}e",
        t(a),
        t(a),
        t(b),
        t(c)
    )
    .into_bytes()
}

/// What the store says about the torrent, chosen so that no value equals
/// what the engine would report on its own: a panel that lost its row
/// cannot pass for one that read it.
const STORE_SAVE: &str = "/store/side/path";
const STORE_ADDED: i64 = 1_500_000_000;
const STORE_COMPLETED: i64 = 1_650_000_000;
const STORE_SEEDING: i64 = 4242;
const ENGINE_COMPLETED: i64 = 1_600_000_000;

/// A torrent held by `engine`, with a store row in that engine's session.
/// `live` gives it a running session: seeding, two peers moving bytes,
/// rates, a scrape, an answered announce, its own rate caps. Otherwise it
/// is stopped by the user, with history but no peer.
async fn fixture(
    s: &TestState,
    engine: &str,
    name: &str,
    category: &str,
    live: bool,
) -> (
    String,
    std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    Vec<typhon_engine::torrent::meta::PeerGuard>,
) {
    use std::sync::atomic::Ordering;
    use typhon_engine::torrent::meta::{PeerGuard, PeerStats, TorrentStatus};

    let blob = multi_torrent(name);
    let engine_save = s.dir.join(format!("engine-side-{name}"));
    let manager = &s.state.engines.get(engine).unwrap().manager;
    let (ih, _) = manager
        .add_torrent_bytes(&blob, &engine_save.to_string_lossy(), !live, false)
        .expect("added");
    let hash = typhon_engine::torrent::hex_encode(&ih);
    let t = manager.get(&ih).expect("held");
    {
        let store = s.state.store.lock().unwrap();
        store
            .insert_torrent(
                &hash,
                engine,
                &blob,
                STORE_SAVE,
                category,
                STORE_ADDED as f64,
                !live,
                "x,y",
            )
            .unwrap();
        store.execute_batch_for_tests(&format!(
            "UPDATE torrents SET completed_time = {STORE_COMPLETED}, seeding_time = {STORE_SEEDING},
             content_folder = 1 WHERE info_hash = '{hash}' AND session = '{engine}'"
        ));
    }

    t.total_uploaded.store(3_000_000, Ordering::Relaxed);
    t.total_downloaded.store(24_576, Ordering::Relaxed);
    t.completed_time.store(ENGINE_COMPLETED, Ordering::Relaxed);
    t.set_rate_limits(Some(1_000_000), Some(2_000_000));
    let mut guards = Vec::new();
    if !live {
        // Half of it on disk: a progress that is neither 0 nor 1.
        t.picker
            .get()
            .expect("a download has a picker")
            .lock()
            .unwrap()
            .set_have(0);
    }
    if live {
        t.set_status(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        // Accumulated, with no open interval: a figure that does not tick
        // between the two reads.
        t.seed_secs.store(777, Ordering::Relaxed);
        t.seed_since.store(0, Ordering::Relaxed);
        t.scrape_seeders.store(7, Ordering::Relaxed);
        t.scrape_leechers.store(3, Ordering::Relaxed);
        t.last_announce_ok.store(true, Ordering::Relaxed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        t.last_announce_at.store(now - 30, Ordering::Relaxed);
        t.next_announce_at.store(now + 1800, Ordering::Relaxed);
        *t.current_tracker.lock().unwrap() = Some("https://one.example/announce".into());

        for (n, (port, client, enc, fast)) in [
            (51413u16, "qBittorrent 4.6.0", true, true),
            (6881, "Transmission 4.0", false, false),
        ]
        .into_iter()
        .enumerate()
        {
            let addr: std::net::SocketAddr = format!("192.0.2.{}:{port}", n + 10).parse().unwrap();
            let p = std::sync::Arc::new(PeerStats::new(
                addr,
                [n as u8; 20],
                client.into(),
                enc,
                fast,
            ));
            p.total_uploaded
                .store(1_000_000 * (n as u64 + 1), Ordering::Relaxed);
            p.total_downloaded
                .store(8192 * (n as u64 + 1), Ordering::Relaxed);
            p.num_pieces_have.store(n as u32 + 1, Ordering::Relaxed);
            p.is_seed.store(n == 1, Ordering::Relaxed);
            p.interested.store(n == 0, Ordering::Relaxed);
            p.choked.store(n == 1, Ordering::Relaxed);
            p.dl_rate.update(0);
            p.ul_rate.update(0);
            guards.push(PeerGuard::new(t.clone(), p));
        }
        t.upload_rate.update(0);
        t.download_rate.update(0);
        // A rate is a delta over at least half a second.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        t.upload_rate.update(600_000);
        t.download_rate.update(60_000);
        for p in t.peers.snapshot() {
            p.dl_rate.update(p.total_downloaded.load(Ordering::Relaxed));
            p.ul_rate.update(p.total_uploaded.load(Ordering::Relaxed));
        }
    }
    (hash, t, guards)
}

/// Every path at which `a` and `b` differ, with both values.
fn differences(a: &serde_json::Value, b: &serde_json::Value, at: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: std::collections::BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                let (va, vb) = (
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                );
                if x.contains_key(k) != y.contains_key(k) {
                    out.push(format!(
                        "{at}.{k}: present {} vs {}",
                        x.contains_key(k),
                        y.contains_key(k)
                    ));
                } else {
                    differences(va, vb, &format!("{at}.{k}"), out);
                }
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (n, (va, vb)) in x.iter().zip(y).enumerate() {
                differences(va, vb, &format!("{at}[{n}]"), out);
            }
        }
        _ if a != b => out.push(format!("{at}: {a} vs {b}")),
        _ => {}
    }
}

/// The fields that are a clock reading -- seconds since a peer connected,
/// since and until an announce -- may tick between the two reads. Each is
/// held to one second, then set equal; peers are put in address order,
/// which a map iteration does not promise.
fn settle_clocks(now: &mut serde_json::Value, before: &mut serde_json::Value) {
    fn by_addr(v: &mut serde_json::Value) {
        if let Some(peers) = v["peers"].as_array_mut() {
            peers.sort_by_key(|p| format!("{}:{}", p["ip"], p["port"]));
        }
    }
    by_addr(now);
    by_addr(before);
    let settle = |a: &mut serde_json::Value, b: &mut serde_json::Value, what: &str| {
        let (x, y) = (a.as_i64().unwrap(), b.as_i64().unwrap());
        assert!(
            (x - y).abs() <= 1,
            "{what}: {x} vs {y}, more than a clock tick apart"
        );
        *a = b.clone();
    };
    let n = now["peers"].as_array().map_or(0, |p| p.len());
    if n == before["peers"].as_array().map_or(0, |p| p.len()) {
        for i in 0..n {
            let b = &mut before["peers"][i]["connection_duration"];
            settle(
                &mut now["peers"][i]["connection_duration"],
                b,
                "connection_duration",
            );
        }
    }
    let n = now["trackers"].as_array().map_or(0, |t| t.len());
    if n == before["trackers"].as_array().map_or(0, |t| t.len()) {
        for i in 0..n {
            for k in ["last_announce", "next_announce"] {
                let b = &mut before["trackers"][i]["endpoints"][0][k];
                settle(&mut now["trackers"][i]["endpoints"][0][k], b, k);
            }
        }
    }
}

/// The route's panel and the panel built the old way from `old_session`,
/// read back to back, with their clocks settled.
async fn both(
    s: &TestState,
    uri: &str,
    engine: &str,
    old_session: &str,
    hash: &str,
    t: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
) -> (serde_json::Value, serde_json::Value, Vec<String>) {
    let mut now = get(s, uri).await;
    let mut before = detail_payload_before(
        &s.state,
        old_session,
        hash,
        t,
        &admission_of(&s.state, engine),
    );
    settle_clocks(&mut now, &mut before);
    // Added after the copy was frozen, on purpose: carried over so that
    // the comparison stays about everything else.
    for k in [
        "pieces_have",
        "pieces_total",
        "isolated_from",
        "checking_progress",
        "check_queued",
    ] {
        before[k] = now[k].clone();
    }
    let mut diff = Vec::new();
    differences(&now, &before, "", &mut diff);
    (now, before, diff)
}

/// The panel against the sources it is built from, so that a field the
/// old and the new code both got wrong does not pass as equal.
fn assert_sources(
    p: &serde_json::Value,
    t: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    hash: &str,
    name: &str,
    category: &str,
    live: bool,
) {
    use std::sync::atomic::Ordering;
    // The store row.
    assert_eq!(p["category"], category, "category from the row");
    assert_eq!(
        p["save_path"], STORE_SAVE,
        "save_path from the row, not the engine's"
    );
    assert_eq!(p["added_time"], STORE_ADDED, "added_time from the row");
    assert_eq!(
        p["completed_time"], STORE_COMPLETED,
        "completed_time from the row"
    );
    // The engine.
    assert_eq!(p["info_hash"], hash);
    assert_eq!(p["name"], name);
    assert_eq!(
        p["engine_save_path"],
        &*t.save_path.read().to_string_lossy()
    );
    assert_eq!(p["multi_file"], true);
    assert_eq!(p["total_size"], 24_576);
    assert_eq!(p["num_pieces"], 2);
    assert_eq!(p["piece_length"], 16_384);
    if live {
        assert_eq!(
            serde_json::json!([p["progress"], p["total_done"]]),
            serde_json::json!([1, 24_576])
        );
    } else {
        assert_eq!(
            serde_json::json!([p["progress"], p["total_done"]]),
            serde_json::json!([0.5, 16_384])
        );
    }
    assert_eq!(p["total_upload"], 3_000_000);
    assert_eq!(p["total_download"], 24_576);
    assert!(p["ratio"].as_f64().unwrap() > 100.0, "ratio {}", p["ratio"]);
    assert_eq!(p["upload_limit"], 1_000_000);
    assert_eq!(p["download_limit"], 2_000_000);
    assert_eq!(p["torrent_error"], false);
    let trackers = p["trackers"].as_array().expect("trackers");
    let urls: Vec<(&str, i64)> = trackers
        .iter()
        .map(|r| (r["url"].as_str().unwrap(), r["tier"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        urls,
        [
            ("https://one.example/announce", 0),
            ("https://two.example/announce", 0),
            ("udp://three.example:6969/announce", 1)
        ],
        "every tracker of every tier"
    );
    assert_eq!(p["tracker_host"], "one.example");
    assert_eq!(p["tracker_error"], false);
    if live {
        assert_eq!(p["state"], "seeding");
        assert_eq!(
            p["seeding_time"], 777,
            "the engine's live seed time wins over the row's"
        );
        assert_eq!(p["upload_rate"], t.upload_rate.get() as i64);
        assert_eq!(p["download_rate"], t.download_rate.get() as i64);
        assert!(
            t.upload_rate.get() > 0 && t.download_rate.get() > 0,
            "the fixture has rates"
        );
        assert_eq!(p["num_peers"], 2);
        assert_eq!(
            serde_json::json!([p["list_seeds"], p["list_peers"]]),
            serde_json::json!([7, 3])
        );
        assert_eq!(
            serde_json::json!([p["num_seeds"], p["swarm_seeds"]]),
            serde_json::json!([7, 7])
        );
        assert_eq!(p["swarm_leechers"], 3);
        for r in trackers {
            let e = &r["endpoints"][0];
            assert_eq!(
                (e["status"].as_str(), e["scrape_complete"].as_i64()),
                (Some("ok"), Some(7))
            );
        }
        let peers = p["peers"].as_array().expect("peers");
        assert_eq!(peers.len(), t.peers.len());
        assert_eq!(peers.len(), 2);
        for peer in peers {
            let addr: std::net::SocketAddr =
                format!("{}:{}", peer["ip"].as_str().unwrap(), peer["port"])
                    .parse()
                    .unwrap();
            let st = t.peers.get(&addr).expect("a peer of the session");
            assert_eq!(peer["client"], st.client.as_str());
            assert_eq!(
                peer["total_upload"],
                st.total_uploaded.load(Ordering::Relaxed)
            );
            assert_eq!(
                peer["total_download"],
                st.total_downloaded.load(Ordering::Relaxed)
            );
            assert_eq!(
                peer["num_pieces"],
                st.num_pieces_have.load(Ordering::Relaxed)
            );
            assert_eq!(peer["ul_rate"], st.ul_rate.get());
            assert_eq!(peer["dl_rate"], st.dl_rate.get());
            assert!(st.ul_rate.get() > 0, "the fixture has peer rates");
        }
        let flags: Vec<&str> = peers.iter().map(|p| p["flags"].as_str().unwrap()).collect();
        assert!(
            flags.contains(&"iUEF") && flags.contains(&"S"),
            "flags {flags:?}"
        );
    } else {
        assert_eq!(
            p["state"], "stopped",
            "the row's user pause makes it stopped, not queued"
        );
        assert_eq!(
            p["seeding_time"], STORE_SEEDING,
            "no live seed time: the row's figure"
        );
        assert_eq!(p["num_peers"], 0);
        assert_eq!(p["peers"], serde_json::json!([]));
        assert_eq!(
            serde_json::json!([p["upload_rate"], p["download_rate"]]),
            serde_json::json!([0, 0])
        );
    }
}

/// ⭐⭐ A seeding torrent of the first hoard engine, with peers, trackers,
/// rates and bytes both ways: the panel the route serves is the one the
/// code before a5d18acd built, every field of it.
#[tokio::test]
async fn the_fast_panel_of_a_live_torrent_is_the_panel_it_replaced() {
    let s = state_from(
        "detail-same-live",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    // A neighbour in the same session, so the old whole-session read had
    // more than one row to pick from.
    let _other = fixture(&s, "hoard", "other", "music", true).await;
    let (hash, t, _peers) = fixture(&s, "hoard", "alpha", "movies", true).await;
    let (now, _, diff) = both(
        &s,
        &format!("/api/hoard/torrents/{hash}"),
        "hoard",
        "hoard",
        &hash,
        &t,
    )
    .await;
    assert_eq!(
        diff,
        Vec::<String>::new(),
        "the new panel differs from the old one"
    );
    assert_sources(&now, &t, &hash, "alpha", "movies", true);
}

/// The detail carries the date of each tag as `tag_since`, and adds
/// nothing else to the panel.
#[tokio::test]
async fn the_detail_carries_the_date_of_each_tag() {
    let s = state_from(
        "detail-tag-since",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let (hash, _t, _) = fixture(&s, "hoard", "gamma", "books", false).await;
    let uri = format!("/api/hoard/torrents/{hash}");
    // The fixture arrived with tags: dated when it was added.
    assert_eq!(
        get(&s, &uri).await["tag_since"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["x", "y"]
    );
    {
        let store = s.state.store.lock().unwrap();
        store.put_meta("tag_since_migrated_at", "1000").unwrap();
        store.set_tags(&hash, &["kept".to_string()]).unwrap();
        // A tag worn before the dates: a column with no row behind it.
        store.execute_batch_for_tests("UPDATE torrents SET tags = 'old'");
        store
            .set_tags(&hash, &["old".to_string(), "fresh".to_string()])
            .unwrap();
    }
    let v = get(&s, &uri).await;
    assert_eq!(v["tag_since"]["old"], 1000, "worn before the dates");
    assert!(v["tag_since"]["fresh"].as_i64().unwrap() > 1000);
    assert!(v["tag_since"].get("kept").is_none(), "no longer worn");
}

/// The detail says how many pieces the engine counts as held, not null:
/// a torrent whose data is all on disk but which the engine counts at 80 %
/// could not be told from a complete one by `progress` alone. The count is
/// the picker's, the same one `progress` divides.
#[tokio::test]
async fn the_detail_counts_the_pieces_the_engine_holds() {
    let s = state_from(
        "detail-pieces-have",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    // Two pieces, the fixture holds the first.
    let (hash, t, _) = fixture(&s, "hoard", "delta", "books", false).await;
    let uri = format!("/api/hoard/torrents/{hash}");
    let v = get(&s, &uri).await;
    assert_eq!(v["pieces_have"], 1, "one of two pieces held");
    assert_eq!(v["pieces_total"], 2);
    assert_eq!(v["progress"], 0.5, "the same count progress divides");

    // A bitfield imported on top (the resume path): the second piece.
    t.picker
        .get()
        .expect("a download has a picker")
        .lock()
        .unwrap()
        .import_bitfield(&[0b0100_0000]);
    let v = get(&s, &uri).await;
    assert_eq!(v["pieces_have"], 2, "the imported piece is counted");
    assert_eq!(v["progress"], 1.0);

    // Seeding, with no picker walk needed: all of them.
    let (hash, _t, _peers) = fixture(&s, "hoard", "omega", "books", true).await;
    let v = get(&s, &format!("/api/hoard/torrents/{hash}")).await;
    assert_eq!(v["pieces_have"], 2);
    assert_eq!(v["pieces_total"], 2);
}

/// A torrent stopped by the user: no peer, no rate, the row's seed time and
/// its pause deciding the state. Same panel as before.
#[tokio::test]
async fn the_fast_panel_of_a_stopped_torrent_is_the_panel_it_replaced() {
    let s = state_from(
        "detail-same-stopped",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let (hash, t, _) = fixture(&s, "hoard", "beta", "books", false).await;
    let (now, _, diff) = both(
        &s,
        &format!("/api/hoard/torrents/{hash}"),
        "hoard",
        "hoard",
        &hash,
        &t,
    )
    .await;
    assert_eq!(
        diff,
        Vec::<String>::new(),
        "the new panel differs from the old one"
    );
    assert_sources(&now, &t, &hash, "beta", "books", false);

    // In the store, held by no engine: no panel, as before -- the lookup
    // that decides it (find_selected) is the one both versions use.
    let ghost = "c".repeat(40);
    s.state
        .store
        .lock()
        .unwrap()
        .insert_torrent(&ghost, "hoard", b"x", "", "", 0.0, true, "")
        .unwrap();
    let req = axum::http::Request::builder()
        .uri(format!("/api/hoard/torrents/{ghost}"))
        .header("X-API-Key", KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// ⭐⭐ A torrent held by an engine that is not the first of its role. The
/// old routes passed the role as the session, so the old panel read no row
/// for it; these are the differences, each the fix of 61555df0 and nothing
/// else.
async fn a_second_engine_differs_only_by_its_row(
    role: &str,
    engine: &str,
    name: &str,
    category: &str,
) {
    let s = tunnelled(&format!("detail-same-{engine}"));
    let (hash, t, _peers) = fixture(&s, engine, name, category, true).await;
    let uri = format!("/api/{role}/torrents/{hash}?engine={engine}");
    let (now, before, diff) = both(&s, &uri, engine, role, &hash, &t).await;
    assert_sources(&now, &t, &hash, name, category, true);

    let engine_save = t.save_path.read().to_string_lossy().into_owned();
    let added = t.added_time;
    // Before: no row in the session called after the role, so every store
    // fact fell back to the engine's own figure or to empty.
    assert_eq!(before["category"], "", "old: no row, no category");
    assert_eq!(
        before["save_path"],
        engine_save.as_str(),
        "old: the engine's save path"
    );
    assert_eq!(before["added_time"], added, "old: the engine's added time");
    assert_eq!(
        before["completed_time"], ENGINE_COMPLETED,
        "old: the engine's completed time"
    );
    // ... and the share limits were named after the role, not the engine.
    assert_eq!(before["share_limits"]["engine"], role);
    assert_eq!(now["share_limits"]["engine"], engine);
    let expected = vec![
        format!(".added_time: {} vs {added}", STORE_ADDED),
        format!(".category: \"{category}\" vs \"\""),
        format!(".completed_time: {STORE_COMPLETED} vs {ENGINE_COMPLETED}"),
        // As JSON prints it: a Windows path's backslashes come escaped.
        format!(
            ".save_path: \"{STORE_SAVE}\" vs {}",
            serde_json::Value::from(engine_save.as_str())
        ),
        format!(".share_limits.engine: \"{engine}\" vs \"{role}\""),
    ];
    assert_eq!(diff, expected, "exactly the fields 61555df0 fixed differ");
}

#[tokio::test]
async fn the_fast_panel_of_a_second_hoard_engine_differs_only_by_its_row() {
    a_second_engine_differs_only_by_its_row("hoard", "hoard2", "gamma", "tv").await;
}

#[tokio::test]
async fn the_fast_panel_of_a_second_race_engine_differs_only_by_its_row() {
    a_second_engine_differs_only_by_its_row("race", "vpn1", "delta", "Race").await;
}
