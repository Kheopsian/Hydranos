//! The qBittorrent shim's 4.4 additions, through the real router: the routes
//! 4.3 answered 404 to, `setPreferences` beyond share limits, the add fields
//! that were ignored, `properties` and the import provenance.

use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str, extra: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"))
}

fn single(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xEFu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    piece[2] = name.as_bytes()[name.len() - 1];
    info.extend_from_slice(&piece);
    info.push(b'e');
    let mut out = Vec::new();
    out.extend_from_slice(b"d7:comment9:a comment10:created by4:mk1213:creation datei1600000000e4:info");
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn multi(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(b"d5:filesl");
    info.extend_from_slice(b"d6:lengthi8192e4:pathl5:a.binee");
    info.extend_from_slice(b"d6:lengthi8192e4:pathl5:b.binee");
    info.extend_from_slice(format!("e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0x12u8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    info.extend_from_slice(&piece);
    info.push(b'e');
    let mut out = b"d4:info".to_vec();
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hash_of(bytes: &[u8]) -> String {
    let m = typhon_engine::torrent::metainfo::parse_torrent_bytes(bytes).unwrap();
    typhon_engine::torrent::hex_encode(&m.info_hash)
}

async fn call(s: &TestState, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("X-API-Key", KEY)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_string()))
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// A shim add: `fields` as text parts, `torrent` as the file part.
async fn add(s: &TestState, torrent: &[u8], fields: &[(&str, &str)]) -> (StatusCode, String) {
    let b = "shimboundary";
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    body.extend_from_slice(
        format!("--{b}\r\nContent-Disposition: form-data; name=\"torrents\"; filename=\"t.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(torrent);
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/torrents/add")
        .header("X-API-Key", KEY)
        .header("content-type", format!("multipart/form-data; boundary={b}"))
        .body(axum::body::Body::from(body))
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn held(s: &TestState, hash: &str) -> Option<std::sync::Arc<typhon_engine::torrent::meta::TorrentState>> {
    find_torrent(&s.state, hash).map(|(_, t)| t)
}

fn data(s: &TestState) -> String {
    let d = s.dir.join("data");
    std::fs::create_dir_all(&d).unwrap();
    d.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// #33: the routes 4.3 answered 404 to
// ---------------------------------------------------------------------------

#[tokio::test]
async fn none_of_the_added_routes_answers_404_any_more() {
    let s = st("shim-routes", "");
    for (m, uri) in [
        ("POST", "/api/v2/torrents/setLocation"),
        ("POST", "/api/v2/torrents/setForceStart"),
        ("POST", "/api/v2/torrents/filePrio"),
        ("POST", "/api/v2/torrents/rename"),
        ("POST", "/api/v2/torrents/topPrio"),
        ("POST", "/api/v2/torrents/bottomPrio"),
        ("GET", "/api/v2/sync/maindata"),
        ("GET", "/api/v2/app/defaultSavePath"),
        ("GET", "/api/v2/torrents/count"),
    ] {
        let (code, body) = call(&s, m, uri, "").await;
        // filePrio and rename look the torrent up: a missing one is the
        // qBittorrent 404 "not found", answered by the route, not the router.
        if code == StatusCode::NOT_FOUND {
            assert!(uri.ends_with("filePrio") || uri.ends_with("rename"), "{uri} is not registered: {body}");
        }
    }
}

/// ⭐ `sync/maindata`: the full shape, every torrent keyed by hash, `rid`
/// moving forward.
#[tokio::test]
async fn sync_maindata_answers_the_full_state_with_a_moving_rid() {
    let s = st("shim-maindata", "");
    put_category(&s.state, "films", "race", &data(&s));
    let t = single("maindata-alpha");
    let (code, _) = add(&s, &t, &[("category", "films")]).await;
    assert_eq!(code, StatusCode::OK);

    let (code, body) = call(&s, "GET", "/api/v2/sync/maindata?rid=0", "").await;
    assert_eq!(code, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["full_update"], true);
    let first = v["rid"].as_u64().unwrap();
    let row = &v["torrents"][hash_of(&t)];
    assert_eq!(row["name"], "maindata-alpha", "{v}");
    assert_eq!(row["category"], "films");
    assert_eq!(v["categories"]["films"]["name"], "films");
    assert!(v["server_state"]["dl_info_speed"].is_number());
    assert!(v["tags"].is_array());

    let (_, body) = call(&s, "GET", &format!("/api/v2/sync/maindata?rid={first}"), "").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(v["rid"].as_u64().unwrap() > first);
    let (_, n) = call(&s, "GET", "/api/v2/torrents/count", "").await;
    assert_eq!(n, "1");
}

/// `filePrio`: the engine downloads every file, so "skip" is refused and
/// "normal" is what already happens.
#[tokio::test]
async fn file_priority_zero_is_refused_and_normal_is_accepted() {
    let s = st("shim-fileprio", "");
    let t = multi("prio-multi");
    let (code, _) = add(&s, &t, &[("savepath", &data(&s))]).await;
    assert_eq!(code, StatusCode::OK);
    let h = hash_of(&t);
    let (code, body) = call(&s, "POST", "/api/v2/torrents/filePrio", &format!("hash={h}&id=0&priority=0")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let (code, _) = call(&s, "POST", "/api/v2/torrents/filePrio", &format!("hash={h}&id=0|1&priority=1")).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = call(&s, "POST", "/api/v2/torrents/filePrio", &format!("hash={h}&id=7&priority=1")).await;
    assert_eq!(code, StatusCode::CONFLICT, "no file 7");
    let (code, _) = call(&s, "POST", "/api/v2/torrents/filePrio", &format!("hash={h}&id=0&priority=3")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&s, "POST", "/api/v2/torrents/filePrio", &format!("hash={}&id=0&priority=1", "0".repeat(40))).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn rename_and_queue_positions_are_refused_with_the_reason() {
    let s = st("shim-refusals", "");
    let t = single("rename-alpha");
    add(&s, &t, &[("savepath", &data(&s))]).await;
    let (code, body) = call(&s, "POST", "/api/v2/torrents/rename", &format!("hash={}&name=x", hash_of(&t))).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(body.contains("not supported"), "{body}");
    let (code, body) = call(&s, "POST", "/api/v2/torrents/topPrio", "hashes=all").await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(body, "Torrent queueing is not enabled", "qBittorrent's own answer");
}

/// `setForceStart`: a start where no queue applies; refused where one does.
#[tokio::test]
async fn force_start_starts_a_stopped_torrent_unless_a_queue_would_undo_it() {
    let s = st("shim-force", "");
    let t = single("force-alpha");
    add(&s, &t, &[("savepath", &data(&s)), ("paused", "true")]).await;
    let h = hash_of(&t);
    assert!(held(&s, &h).unwrap().is_paused.load(std::sync::atomic::Ordering::Relaxed));
    let (code, _) = call(&s, "POST", "/api/v2/torrents/setForceStart", &format!("hashes={h}&value=true")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!held(&s, &h).unwrap().is_paused.load(std::sync::atomic::Ordering::Relaxed), "started");

    let q = st("shim-force-queued", "[race]\nactive_downloads = 2\n");
    let t = single("force-beta");
    add(&q, &t, &[("savepath", &data(&q)), ("paused", "true")]).await;
    let h = hash_of(&t);
    let (code, body) = call(&q, "POST", "/api/v2/torrents/setForceStart", &format!("hashes={h}&value=true")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(held(&q, &h).unwrap().is_paused.load(std::sync::atomic::Ordering::Relaxed), "nothing started");
}

/// `setLocation` takes the native move's refusals.
#[tokio::test]
async fn set_location_needs_an_absolute_location() {
    let s = st("shim-location", "");
    let t = single("loc-alpha");
    add(&s, &t, &[("savepath", &data(&s))]).await;
    let h = hash_of(&t);
    let (code, _) = call(&s, "POST", "/api/v2/torrents/setLocation", &format!("hashes={h}")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, body) = call(&s, "POST", "/api/v2/torrents/setLocation", &format!("hashes={h}&location=relative/dir")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let dest = s.dir.join("moved");
    let (code, body) =
        call(&s, "POST", "/api/v2/torrents/setLocation", &format!("hashes={h}&location={}", dest.display())).await;
    assert_eq!(code, StatusCode::OK, "{body}");
}

// ---------------------------------------------------------------------------
// #34: the add fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn content_layout_subfolder_gives_a_single_file_its_folder() {
    let s = st("shim-subfolder", "");
    let base = data(&s);
    let t = single("layout-alpha.mkv");
    let (code, _) = add(&s, &t, &[("savepath", &base), ("contentLayout", "Subfolder")]).await;
    assert_eq!(code, StatusCode::OK);
    let got = held(&s, &hash_of(&t)).unwrap().save_path.read().clone();
    assert_eq!(got, std::path::Path::new(&base).join("layout-alpha"));

    let t = single("layout-beta.mkv");
    add(&s, &t, &[("savepath", &base), ("contentLayout", "Original")]).await;
    assert_eq!(*held(&s, &hash_of(&t)).unwrap().save_path.read(), std::path::PathBuf::from(&base));
}

/// ⭐ NoSubfolder on a multi-file torrent is refused: the engine writes it
/// under its name, and a client linking from the stripped layout would find
/// nothing.
#[tokio::test]
async fn content_layout_no_subfolder_on_a_multi_file_torrent_is_refused() {
    let s = st("shim-nosub", "");
    let t = multi("nosub-multi");
    let (code, body) = add(&s, &t, &[("savepath", &data(&s)), ("contentLayout", "NoSubfolder")]).await;
    assert_eq!((code, body.as_str()), (StatusCode::BAD_REQUEST, "Fails."));
    assert!(held(&s, &hash_of(&t)).is_none());
    // The old boolean says the same.
    let (code, _) = add(&s, &t, &[("savepath", &data(&s)), ("root_folder", "false")]).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    // On a single file it is what happens anyway.
    let one = single("nosub-single");
    let (code, _) = add(&s, &one, &[("savepath", &data(&s)), ("contentLayout", "NoSubfolder")]).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn auto_tmm_lets_the_category_decide_and_stop_condition_adds_stopped() {
    let s = st("shim-tmm", "");
    let cat = s.dir.join("cat");
    std::fs::create_dir_all(&cat).unwrap();
    put_category(&s.state, "films", "race", cat.to_str().unwrap());
    let t = single("tmm-alpha");
    let (code, _) = add(
        &s,
        &t,
        &[("category", "films"), ("savepath", "/somewhere/else"), ("autoTMM", "true"), ("stopCondition", "FilesChecked")],
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let torrent = held(&s, &hash_of(&t)).unwrap();
    assert_eq!(*torrent.save_path.read(), cat, "the category's folder, not the one sent");
    assert!(torrent.is_paused.load(std::sync::atomic::Ordering::Relaxed), "stopped as asked");
}

// ---------------------------------------------------------------------------
// #28: setPreferences
// ---------------------------------------------------------------------------

async fn set_prefs(s: &TestState, json: serde_json::Value) -> (StatusCode, String) {
    let form = format!("json={}", enc(&json.to_string()));
    call(s, "POST", "/api/v2/app/setPreferences", &form).await
}

async fn prefs(s: &TestState) -> serde_json::Value {
    let (_, body) = call(s, "GET", "/api/v2/app/preferences", "").await;
    serde_json::from_str(&body).unwrap()
}

/// ⭐ A client that posts back the page it read changes nothing: not a byte
/// of the config file.
#[tokio::test]
async fn posting_back_the_preferences_page_changes_nothing() {
    let s = st("shim-prefs-noop", "");
    let before = std::fs::read_to_string(&s.state.config_path).unwrap();
    let page = prefs(&s).await;
    let (code, _) = set_prefs(&s, page).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(std::fs::read_to_string(&s.state.config_path).unwrap(), before);
}

#[tokio::test]
async fn set_preferences_applies_rates_dht_pex_active_downloads_and_save_path() {
    let s = st("shim-prefs", "");
    let dest = data(&s);
    let (code, body) = set_prefs(
        &s,
        serde_json::json!({
            "up_limit": 1_048_576, "dl_limit": 2_097_152, "dht": false, "pex": false,
            "max_active_downloads": 3, "save_path": dest, "listen_port": 1234,
        }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let p = prefs(&s).await;
    assert_eq!(p["up_limit"], 1_048_576);
    assert_eq!(p["dl_limit"], 2_097_152);
    assert_eq!(p["dht"], false);
    assert_eq!(p["pex"], false);
    assert_eq!(p["max_active_downloads"], 3);
    assert_eq!(p["save_path"], serde_json::json!(dest));
    let (_, d) = call(&s, "GET", "/api/v2/app/defaultSavePath", "").await;
    assert_eq!(d, dest);
    // `listen_port` is not one of them: the page still shows the engine's.
    assert_ne!(p["listen_port"], 1234);

    // An add with neither a category nor a savepath lands there now.
    let t = single("default-path");
    let (code, _) = add(&s, &t, &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(*held(&s, &hash_of(&t)).unwrap().save_path.read(), std::path::PathBuf::from(&dest));
}

#[tokio::test]
async fn a_relative_save_path_is_refused_and_writes_nothing() {
    let s = st("shim-prefs-relative", "");
    let before = std::fs::read_to_string(&s.state.config_path).unwrap();
    let (code, _) = set_prefs(&s, serde_json::json!({"save_path": "relative", "dht": false})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(std::fs::read_to_string(&s.state.config_path).unwrap(), before, "nothing written");
}

/// Under queueing, the two queue ceilings are written too.
#[tokio::test]
async fn under_queueing_the_queue_ceilings_are_written() {
    let s = st("shim-prefs-queue", "[race]\nqueueing = true\nactive_seeds = 10\n");
    let (code, _) = set_prefs(&s, serde_json::json!({"max_active_uploads": 4, "max_active_torrents": 9})).await;
    assert_eq!(code, StatusCode::OK);
    let p = prefs(&s).await;
    assert_eq!((p["max_active_uploads"].as_i64(), p["max_active_torrents"].as_i64()), (Some(4), Some(9)));
}

// ---------------------------------------------------------------------------
// #30: properties
// ---------------------------------------------------------------------------

#[tokio::test]
async fn properties_read_the_torrent_file_and_the_swarm() {
    let s = st("shim-props", "");
    let t = single("props-alpha");
    add(&s, &t, &[("savepath", &data(&s))]).await;
    let (code, body) = call(&s, "GET", &format!("/api/v2/torrents/properties?hash={}", hash_of(&t)), "").await;
    assert_eq!(code, StatusCode::OK);
    let p: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(p["creation_date"], 1_600_000_000, "from the file, not the addition date");
    assert_eq!(p["created_by"], "mk12");
    assert_eq!(p["comment"], "a comment");
    for k in ["nb_connections", "peers", "peers_total", "seeds_total", "dl_speed_avg", "up_speed_avg", "eta", "last_seen"] {
        assert!(p[k].is_i64(), "{k} is a number: {p}");
    }
    assert_eq!(p["last_seen"], -1, "an incomplete torrent nobody seeds: never seen complete");
}

// ---------------------------------------------------------------------------
// #23: provenance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_import_that_added_torrents_is_recorded_and_served() {
    use std::sync::atomic::Ordering;
    let s = st("shim-provenance", "");
    let before = body_json(super::get_provenance(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert_eq!(before["present"], false);

    let p = crate::importer::Progress::default();
    p.seeded.store(2, Ordering::Relaxed);
    p.downloading.store(1, Ordering::Relaxed);
    p.carried_uploaded.store(4096, Ordering::Relaxed);
    super::record_provenance(&s.state, "qBittorrent", &p);

    let v = body_json(super::get_provenance(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert_eq!(v["present"], true);
    assert_eq!(v["source_client"], "qBittorrent");
    assert_eq!(v["imported_count"], 3);
    assert_eq!(v["carried_uploaded_bytes"], 4096);
    assert!(v["source_date"].as_i64().unwrap() > 0);

    // An import that added nothing claims nothing.
    let empty = crate::importer::Progress::default();
    super::record_provenance(&s.state, "Transmission", &empty);
    let v = body_json(super::get_provenance(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert_eq!(v["source_client"], "qBittorrent");
}
