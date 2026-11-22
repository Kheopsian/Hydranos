use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn bstr(s: &str) -> String {
    format!("{}:{s}", s.len())
}

/// A multi-file torrent whose second file sits behind a BEP 47 pad: the
/// pad is dropped from the file list but still takes room in the stream.
fn multi_file_bytes() -> Vec<u8> {
    let file = |attr: &str, len: u64, path: &[&str]| {
        let mut f = String::from("d");
        if !attr.is_empty() {
            f += &format!("4:attr{}", bstr(attr));
        }
        f += &format!("6:lengthi{len}e4:pathl");
        for p in path {
            f += &bstr(p);
        }
        f + "ee"
    };
    let files = [
        file("", 10_000, &["a.mkv"]),
        file("p", 6_384, &[".pad", "6384"]),
        file("", 20_000, &["b.mkv"]),
    ]
    .concat();
    let mut out = format!(
        "d4:infod5:filesl{files}e4:name{}12:piece lengthi16384e6:pieces60:",
        bstr("Show S02")
    )
    .into_bytes();
    out.extend_from_slice(&[0u8; 60]);
    out.extend_from_slice(b"ee");
    out
}

fn single_file_bytes() -> Vec<u8> {
    let mut out = format!(
        "d4:infod6:lengthi16384e4:name{}12:piece lengthi16384e6:pieces20:",
        bstr("solo.mkv")
    )
    .into_bytes();
    out.extend_from_slice(&[0u8; 20]);
    out.extend_from_slice(b"ee");
    out
}

fn populated(tag: &str, bytes: &[u8]) -> (TestState, String) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (hash, _) =
        add_torrent_bytes(&s.state, bytes, "", "/tmp", "fr", true, true, "race").expect("added");
    (s, hash)
}

async fn call(s: &TestState, method: &str, uri: &str, form: &str) -> (StatusCode, Vec<u8>) {
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("X-API-Key", KEY)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_string()))
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("json")
}

fn listed<'a>(info: &'a serde_json::Value, hash: &str) -> &'a serde_json::Value {
    info.as_array()
        .unwrap()
        .iter()
        .find(|r| r["hash"] == hash)
        .expect("listed")
}

/// A cap the engine's state store could not keep is answered with an error,
/// not "Ok.", and the torrent keeps the cap it had.
#[tokio::test]
async fn a_torrent_cap_the_store_did_not_keep_is_not_acknowledged() {
    let (s, hash) = populated("shim-limit-store-down", &single_file_bytes());
    let conn = rusqlite::Connection::open(s.dir.join("race").join("state.db")).unwrap();
    conn.execute("DROP TABLE torrent_state", []).unwrap();
    drop(conn);

    let (st, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/setUploadLimit",
        &format!("hashes={hash}&limit=51200"),
    )
    .await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
    let (_, t) = find_torrent(&s.state, &hash).unwrap();
    assert_eq!(t.rate_limits(), (0, 0), "the engine keeps the cap it had");
}

/// ⭐⭐ A per-torrent cap set over POST, the hash in the BODY as cross-seed
/// and qBit's own WebUI send it: it reaches the engine (read back from
/// the torrent, not from the answer) and every read the shim has --
/// `torrents/uploadLimit`, `info`, `properties` -- reports it.
#[tokio::test]
async fn a_torrent_cap_set_over_post_reaches_the_engine_and_every_read() {
    let (s, hash) = populated("shim-limit-torrent", &single_file_bytes());
    let (st, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/setUploadLimit",
        &format!("hashes={hash}&limit=51200"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, t) = find_torrent(&s.state, &hash).unwrap();
    assert_eq!(t.rate_limits(), (51_200, 0), "the engine holds the cap");
    assert!(t
        .rate_chain(typhon_engine::torrent::ratelimit::Dir::Up)
        .is_limited());

    let (_, b) = call(
        &s,
        "POST",
        "/api/v2/torrents/uploadLimit",
        &format!("hashes={hash}"),
    )
    .await;
    assert_eq!(json(&b)[&hash], 51_200);
    let (_, b) = call(
        &s,
        "POST",
        "/api/v2/torrents/downloadLimit",
        &format!("hashes={hash}"),
    )
    .await;
    assert_eq!(
        json(&b)[&hash],
        0,
        "no download cap reads 0 here, as in qBit"
    );
    let (_, b) = call(&s, "POST", "/api/v2/torrents/info", "").await;
    let row = json(&b);
    assert_eq!(listed(&row, &hash)["up_limit"], 51_200);
    assert_eq!(
        listed(&row, &hash)["dl_limit"],
        -1,
        "and -1 in the listing, as in qBit"
    );
    let (_, b) = call(
        &s,
        "POST",
        "/api/v2/torrents/properties",
        &format!("hash={hash}"),
    )
    .await;
    assert_eq!(json(&b)["up_limit"], 51_200);
    assert_eq!(json(&b)["dl_limit"], -1);

    // 0 lifts it; so does any negative value.
    call(
        &s,
        "POST",
        "/api/v2/torrents/setUploadLimit",
        &format!("hashes={hash}&limit=-1"),
    )
    .await;
    assert_eq!(t.rate_limits(), (0, 0));
    // A write is POST only, as qBittorrent has it; a GET changes nothing.
    let (st, _) = call(
        &s,
        "GET",
        &format!("/api/v2/torrents/setUploadLimit?hashes={hash}&limit=1"),
        "",
    )
    .await;
    assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(t.rate_limits(), (0, 0));
    // No limit at all is refused rather than read as "lift".
    let (st, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/setUploadLimit",
        &format!("hashes={hash}"),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

/// `hashes=all` caps every torrent, like the shim's other bulk verbs, and
/// the parameters may come in the query of a POST.
#[tokio::test]
async fn hashes_all_caps_every_torrent_from_the_query_of_a_post() {
    let (s, a) = populated("shim-limit-all", &single_file_bytes());
    let (b, _) = add_torrent_bytes(
        &s.state,
        &multi_file_bytes(),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .unwrap();
    let (st, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/setDownloadLimit?hashes=all&limit=8192",
        "",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    for h in [&a, &b] {
        assert_eq!(
            find_torrent(&s.state, h).unwrap().1.rate_limits(),
            (0, 8192),
            "{h}"
        );
    }
}

/// ⭐ The global cap: `transfer/set*Limit` moves the client-wide bucket
/// above every engine, and `transfer/*Limit`, `transfer/info` and
/// `preferences` all read it back, bytes/s.
#[tokio::test]
async fn the_global_cap_is_set_over_post_and_read_everywhere() {
    let (s, _) = populated("shim-limit-global", &single_file_bytes());
    let (st, _) = call(
        &s,
        "POST",
        "/api/v2/transfer/setUploadLimit",
        "limit=1048576",
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    call(
        &s,
        "POST",
        "/api/v2/transfer/setDownloadLimit",
        "limit=524288",
    )
    .await;
    assert_eq!(s.state.engines.client_rates().up.rate(), 1_048_576);
    let race = s.state.engines.get("race").unwrap();
    assert_eq!(
        race.manager.rates().client().unwrap().up.rate(),
        1_048_576,
        "the engine is under it"
    );

    let (_, b) = call(&s, "POST", "/api/v2/transfer/uploadLimit", "").await;
    assert_eq!(String::from_utf8(b).unwrap(), "1048576");
    let (_, b) = call(&s, "GET", "/api/v2/transfer/downloadLimit", "").await;
    assert_eq!(String::from_utf8(b).unwrap(), "524288");
    let (_, b) = call(&s, "GET", "/api/v2/transfer/info", "").await;
    assert_eq!(json(&b)["up_rate_limit"], 1_048_576);
    assert_eq!(json(&b)["dl_rate_limit"], 524_288);
    let (_, b) = call(&s, "GET", "/api/v2/app/preferences", "").await;
    assert_eq!(json(&b)["up_limit"], 1_048_576);
    assert_eq!(json(&b)["dl_limit"], 524_288);

    // Persisted: a restart puts it back.
    s.state.engines.client_rates().up.set_rate(0);
    restore_client_rate_limits(&s.state);
    assert_eq!(s.state.engines.client_rates().up.rate(), 1_048_576);

    call(&s, "POST", "/api/v2/transfer/setUploadLimit", "limit=0").await;
    let (_, b) = call(&s, "GET", "/api/v2/transfer/info", "").await;
    assert_eq!(json(&b)["up_rate_limit"], 0, "0 = no limit");
}

fn lsd_prefs(on: bool) -> String {
    format!("json=%7B%22lsd%22%3A{on}%7D")
}

/// ⭐ `preferences.lsd` is the race engine's real setting (on when the
/// file says nothing), and `setPreferences` writes it to every engine --
/// but only when it changes: a client posting back the page it read must
/// not switch LSD on for the hoard.
#[tokio::test]
async fn preferences_lsd_is_real_and_an_echo_changes_nothing() {
    let (s, _) = populated("shim-lsd-prefs", &single_file_bytes());
    let (_, b) = call(&s, "GET", "/api/v2/app/preferences", "").await;
    assert_eq!(json(&b)["lsd"], true, "race default");

    let (st, _) = call(&s, "POST", "/api/v2/app/setPreferences", &lsd_prefs(true)).await;
    assert_eq!(st, StatusCode::OK);
    let cfg = s.state.cfg();
    assert_eq!(cfg.hoard.enable_lsd, None, "the echo wrote nothing");
    assert!(!cfg.hoard.lsd_on("hoard"));

    call(&s, "POST", "/api/v2/app/setPreferences", &lsd_prefs(false)).await;
    let cfg = s.state.cfg();
    assert_eq!(
        (cfg.race.enable_lsd, cfg.hoard.enable_lsd),
        (Some(false), Some(false))
    );
    let (_, b) = call(&s, "GET", "/api/v2/app/preferences", "").await;
    assert_eq!(json(&b)["lsd"], false);

    call(&s, "POST", "/api/v2/app/setPreferences", r#"{"lsd":true}"#).await;
    assert_eq!(s.state.cfg().race.enable_lsd, Some(true), "a JSON body too");
    let raw = std::fs::read_to_string(&s.config_path).unwrap();
    assert!(crate::deadkeys::config_warnings(&raw).is_empty(), "{raw}");
}

/// ⭐ The `[LSD]` row of `torrents/trackers` says what LSD does for the
/// torrent: disabled when the engine runs none or the torrent is private,
/// working with the LAN peers found otherwise.
#[tokio::test]
async fn the_lsd_tracker_row_shows_the_real_state() {
    let row = |b: &[u8]| {
        json(b)
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["url"] == "** [LSD] **")
            .cloned()
            .expect("an [LSD] row")
    };
    let (s, hash) = populated("shim-lsd-row", &single_file_bytes());
    let (_, b) = call(
        &s,
        "GET",
        &format!("/api/v2/torrents/trackers?hash={hash}"),
        "",
    )
    .await;
    let r = row(&b);
    assert_eq!(
        (r["status"].clone(), r["msg"].clone()),
        (
            serde_json::json!(0),
            serde_json::json!("LSD is off for this engine")
        )
    );

    let mut private = format!(
        "d4:infod6:lengthi16384e4:name{}12:piece lengthi16384e6:pieces20:",
        bstr("secret.mkv")
    )
    .into_bytes();
    private.extend_from_slice(&[1u8; 20]);
    private.extend_from_slice(b"7:privatei1eee");
    let (secret, _) =
        add_torrent_bytes(&s.state, &private, "", "/tmp", "fr", true, true, "race").expect("added");

    let port = std::net::UdpSocket::bind("0.0.0.0:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let Ok(lsd) = typhon_engine::lsd::LsdSession::bind(None, port).await else {
        eprintln!("no multicast here: the running half is skipped");
        return;
    };
    let race = s.state.engines.get("race").unwrap();
    race.manager.set_lsd(lsd.clone());
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    // Added stopped by the fixture; a stopped torrent takes no LSD peer.
    find_torrent(&s.state, &hash)
        .unwrap()
        .1
        .set_paused(false, std::sync::atomic::Ordering::Relaxed);
    let from: std::net::SocketAddr = "192.168.0.7:6771".parse().unwrap();
    lsd.offer(
        &race.manager,
        from,
        &typhon_engine::lsd::Announce {
            port: 6881,
            info_hashes: vec![ih],
            cookie: None,
        },
    );

    let (_, b) = call(
        &s,
        "GET",
        &format!("/api/v2/torrents/trackers?hash={hash}"),
        "",
    )
    .await;
    let r = row(&b);
    assert_eq!(
        (r["status"].clone(), r["num_peers"].clone()),
        (serde_json::json!(2), serde_json::json!(1))
    );
    let (_, b) = call(
        &s,
        "GET",
        &format!("/api/v2/torrents/trackers?hash={secret}"),
        "",
    )
    .await;
    let r = row(&b);
    assert_eq!(
        (r["status"].clone(), r["msg"].clone()),
        (
            serde_json::json!(0),
            serde_json::json!("This torrent is private")
        )
    );
}

/// ⭐ The engine keys are live: saving `upload_rate_limit` (bytes/s, the
/// unit the settings screen announces), `choking` and the slots moves the
/// RUNNING engine without a restart, and a file from before 4.4 that has
/// none of these keys accepts them. The native engine route writes the
/// same key, from KiB/s.
#[tokio::test]
async fn saving_the_engine_keys_moves_the_running_engine() {
    let s = state_from(
        "live-engine-keys",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nlisten_port = 1\n"),
    );
    let race = s.state.engines.get("race").unwrap();
    assert!(!race.manager.policy().choking(), "off by default");
    assert_eq!(
        race.manager.policy().max_uploads(),
        None,
        "no cap by default"
    );
    let body = serde_json::json!({"changes": [
        {"section": "race", "key": "upload_rate_limit", "value": 65536},
        {"section": "race", "key": "choking", "value": true},
        {"section": "race", "key": "max_uploads_per_torrent", "value": 2},
        {"section": "race", "key": "max_uploads", "value": 5},
        {"section": "race", "key": "peer_timeout", "value": 600},
    ]});
    let (st, b) = call(&s, "POST", "/api/settings", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(
        json(&b)["restart_required"],
        false,
        "every key of the batch is live"
    );
    assert_eq!(race.manager.rates().engine.up.rate(), 65_536);
    assert!(race.manager.policy().choking());
    assert_eq!(race.manager.policy().unchoke_slots(), Some(2));
    assert_eq!(race.manager.policy().max_uploads(), Some(5));
    assert_eq!(race.manager.policy().idle_timeout().as_secs(), 600);

    let (st, b) = call(
        &s,
        "POST",
        "/api/engines/race/rate-limits",
        r#"{"upload_kib": 128, "download_kib": 0}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(race.manager.rates().engine.up.rate(), 131_072);
    assert_eq!(json(&b)["upload_kib"], 128);
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    assert!(file.contains("upload_rate_limit = 131072"), "{file}");
}

/// `announce_sort` is live too, on every engine: a file without the key
/// accepts it, no restart is asked for, and the running scheduler's order
/// changes for the torrents that have not announced yet.
#[tokio::test]
async fn saving_announce_sort_reorders_the_running_engine() {
    let s = state_from(
        "live-announce-sort",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nlisten_port = 1\n"),
    );
    let race = s.state.engines.get("race").unwrap();
    assert_eq!(
        race.announce_sort.get(),
        crate::config::AnnounceSort::Default
    );
    let body = serde_json::json!({"changes": [
        {"section": "race", "key": "announce_sort", "value": "ratio"},
    ]});
    let (st, b) = call(&s, "POST", "/api/settings", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(json(&b)["restart_required"], false);
    assert_eq!(race.announce_sort.get(), crate::config::AnnounceSort::Ratio);
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    assert!(file.contains("announce_sort = \"ratio\""), "{file}");
}

/// `peer_encryption` is a live engine key: `allow` until saved, then the
/// running engine follows the file without a restart.
#[tokio::test]
async fn saving_peer_encryption_moves_the_running_engine() {
    use typhon_engine::peer::extension::Encryption;
    let s = state_from(
        "live-peer-encryption",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nlisten_port = 1\n"),
    );
    let race = s.state.engines.get("race").unwrap();
    assert_eq!(
        race.manager.policy().encryption(),
        Encryption::Allow,
        "allow by default"
    );
    let body = serde_json::json!({"changes": [{"section": "race", "key": "peer_encryption", "value": "require"}]});
    let (st, b) = call(&s, "POST", "/api/settings", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(json(&b)["restart_required"], false);
    assert_eq!(race.manager.policy().encryption(), Encryption::Require);
}

/// `max_unknown_infohash_per_ip` is a live engine key: off (0) until
/// saved, then the running engine follows the file without a restart.
#[tokio::test]
async fn saving_the_unknown_infohash_limit_moves_the_running_engine() {
    let s = state_from(
        "live-unknown-infohash",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nlisten_port = 1\n"),
    );
    let race = s.state.engines.get("race").unwrap();
    assert_eq!(
        race.manager.policy().unknown_hash_limit(),
        0,
        "off by default"
    );
    let body = serde_json::json!({"changes": [{"section": "race", "key": "max_unknown_infohash_per_ip", "value": 20}]});
    let (st, b) = call(&s, "POST", "/api/settings", &body.to_string()).await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(json(&b)["restart_required"], false);
    assert_eq!(race.manager.policy().unknown_hash_limit(), 20);
}

/// ⭐⭐ The REAL ratio: what the torrent uploaded and downloaded, in
/// `info` and in `properties`, and the two agree. Until 4.4 both said 0
/// for every torrent (see qbitrow.rs).
#[tokio::test]
async fn info_and_properties_report_the_real_ratio() {
    let (s, hash) = populated("shim-real-ratio", &single_file_bytes());
    let (_, t) = find_torrent(&s.state, &hash).unwrap();
    t.restore_lifetime(30_000, 10_000);
    let (_, b) = call(&s, "POST", "/api/v2/torrents/info", "").await;
    let info = json(&b);
    let row = listed(&info, &hash);
    assert_eq!(row["uploaded"], 30_000);
    assert_eq!(row["ratio"], serde_json::json!(3));
    let (_, b) = call(
        &s,
        "POST",
        "/api/v2/torrents/properties",
        &format!("hash={hash}"),
    )
    .await;
    let p = json(&b);
    assert_eq!(p["total_uploaded"], 30_000);
    assert_eq!(p["total_downloaded"], 10_000);
    assert_eq!(
        p["share_ratio"], row["ratio"],
        "one ratio, whichever route asks"
    );
}

/// ⭐⭐ The bug as cross-seed met it: hash in the body, nothing on the URL.
#[tokio::test]
async fn the_detail_routes_read_the_hash_from_a_post_form() {
    let (s, hash) = populated("shim-post-detail", &multi_file_bytes());
    let form = format!("hash={hash}");
    for route in ["files", "properties", "trackers"] {
        let (status, _) = call(&s, "POST", &format!("/api/v2/torrents/{route}"), &form).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "POST torrents/{route} with the hash in the body"
        );
    }
    // The query string still works, as it did.
    let (status, _) = call(
        &s,
        "GET",
        &format!("/api/v2/torrents/files?hash={hash}"),
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_hash_is_a_404_on_every_detail_route() {
    let (s, _) = populated("shim-post-404", &multi_file_bytes());
    let form = format!("hash={}", "ab".repeat(20));
    for route in ["files", "properties", "trackers", "export"] {
        let (status, _) = call(&s, "POST", &format!("/api/v2/torrents/{route}"), &form).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "torrents/{route}: a 200 here reads as an empty torrent"
        );
    }
}

/// qBit names a multi-file torrent's files from the save path, folder
/// first; cross-seed compares those names to its candidate's.
#[tokio::test]
async fn files_carry_the_torrent_folder_and_the_padded_piece_range() {
    let (s, hash) = populated("shim-files-layout", &multi_file_bytes());
    let (_, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/files",
        &format!("hash={hash}"),
    )
    .await;
    let files = json(&body);
    let names: Vec<&str> = files
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Show S02/a.mkv", "Show S02/b.mkv"],
        "the pad is not a file"
    );
    // b.mkv starts at 16384 (10000 + the 6384 pad), i.e. at piece 1, and
    // runs to byte 36383, piece 2. A running sum of the listed lengths
    // put it at 10000, piece 0.
    assert_eq!(files[1]["piece_range"], serde_json::json!([1, 2]));
    assert_eq!(files[0]["piece_range"], serde_json::json!([0, 0]));
}

#[tokio::test]
async fn a_single_file_torrent_is_named_without_a_folder() {
    let (s, hash) = populated("shim-files-single", &single_file_bytes());
    let (_, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/files",
        &format!("hash={hash}"),
    )
    .await;
    assert_eq!(json(&body)[0]["name"], "solo.mkv");
}

#[tokio::test]
async fn export_returns_the_torrent_as_it_was_added() {
    let bytes = multi_file_bytes();
    let (s, hash) = populated("shim-export", &bytes);
    for (method, uri, form) in [
        (
            "POST",
            "/api/v2/torrents/export".to_string(),
            format!("hash={hash}"),
        ),
        (
            "GET",
            format!("/api/v2/torrents/export?hash={}", hash.to_uppercase()),
            String::new(),
        ),
    ] {
        let (status, body) = call(&s, method, &uri, &form).await;
        assert_eq!(status, StatusCode::OK, "{method} export");
        assert_eq!(
            body, bytes,
            "{method}: byte-identical, so the info hash still matches"
        );
    }
}

/// cross-seed reads categories with a POST too: a GET-only route was a 405.
#[tokio::test]
async fn categories_and_tags_answer_a_post() {
    let (s, _) = populated("shim-post-lists", &single_file_bytes());
    for route in ["categories", "tags"] {
        let (status, _) = call(&s, "POST", &format!("/api/v2/torrents/{route}"), "").await;
        assert_eq!(status, StatusCode::OK, "POST torrents/{route}");
    }
}

/// A piece shared by two files counts for each only its own bytes.
#[test]
fn held_bytes_count_the_overlap_not_the_whole_piece() {
    // 16 KiB pieces; a 10 000-byte file, then one at 10 000 of 20 000.
    let only = |held: &'static [u64]| move |i: u64| held.contains(&i);
    assert_eq!(held_bytes(0, 10_000, 16_384, only(&[0])), 10_000);
    // Piece 0 holds the second file's first 6 384 bytes.
    assert_eq!(held_bytes(10_000, 20_000, 16_384, only(&[0])), 6_384);
    assert_eq!(held_bytes(10_000, 20_000, 16_384, only(&[1])), 13_616);
    assert_eq!(held_bytes(10_000, 20_000, 16_384, only(&[0, 1])), 20_000);
    assert_eq!(held_bytes(10_000, 20_000, 16_384, only(&[])), 0);
}

/// The Content tab's route says what each file already has.
#[tokio::test]
async fn the_native_file_list_carries_what_was_downloaded() {
    let (s, hash) = populated("native-files-done", &multi_file_bytes());
    let (status, body) = call(&s, "GET", &format!("/api/torrents/{hash}/files"), "").await;
    assert_eq!(status, StatusCode::OK);
    let files = json(&body)["files"].clone();
    // Added in seed mode: complete, so every file is all there.
    for f in files.as_array().unwrap() {
        assert_eq!(f["done"], f["size"], "{f}");
        assert_eq!(f["progress"], 1.0);
    }
}

/// The Content tab's Recheck on one file: a check of that file only, and
/// the list back with that file under it. A file the torrent does not
/// have, or a body that names none, is refused.
#[tokio::test]
async fn a_file_recheck_checks_that_file_only() {
    let (s, hash) = populated("native-file-recheck", &multi_file_bytes());
    let uri = format!("/api/torrents/{hash}/files/recheck");
    let (status, body) = call(&s, "POST", &uri, r#"{"index": 0}"#).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let v = json(&body);
    assert_eq!(v["scope"], "file", "{v}");
    let files = v["files"].as_array().unwrap();
    let first = files.iter().find(|f| f["index"] == 0).unwrap();
    assert_eq!(first["checking"], true, "{first}");

    let (status, _) = call(&s, "POST", &uri, r#"{"index": 99}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&s, "POST", &uri, "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &s,
        "POST",
        &format!("/api/torrents/{}/files/recheck", "ab".repeat(20)),
        r#"{"index": 0}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn shim_param_prefers_a_non_empty_query_then_the_body() {
    assert_eq!(
        shim_param("hash=aa", "hash=bb", "hash").as_deref(),
        Some("aa")
    );
    assert_eq!(
        shim_param("hash=", "hash=bb", "hash").as_deref(),
        Some("bb")
    );
    assert_eq!(
        shim_param("", "category=a%20b", "category").as_deref(),
        Some("a b")
    );
    assert_eq!(shim_param("", "", "hash"), None);
}

/// Duplicate to a local engine adds in seed mode, which promises every
/// byte is on disk: a source still downloading must be refused, or the
/// second engine serves pieces nobody fetched.
#[tokio::test]
async fn duplicating_an_incomplete_torrent_is_refused() {
    let s = state_from(
        "copy-incomplete",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let dir = s.dir.join("partial").display().to_string();
    let (partial, _) = add_torrent_bytes(
        &s.state,
        &multi_file_bytes(),
        "",
        &dir,
        "",
        true,
        false,
        "race",
    )
    .expect("added");
    let (st, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{partial}/copy"),
        r#"{"engine":"hoard"}"#,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(
        find_copy(&s.state, "hoard", &partial).is_none(),
        "nothing was added to hoard"
    );

    let (whole, _) = add_torrent_bytes(
        &s.state,
        &single_file_bytes(),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let (st, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{whole}/copy"),
        r#"{"engine":"hoard"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(find_copy(&s.state, "hoard", &whole).is_some());
}
