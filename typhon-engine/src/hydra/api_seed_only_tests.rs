//! The per-torrent seed-only flag through the real router: the native route,
//! the row, and what the qBittorrent shim reports for a torrent that serves
//! what it holds and never downloads.

use super::testing::*;
use super::*;
use sha1::{Digest, Sha1};
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const A: usize = 20_000;

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn a_bytes() -> Vec<u8> {
    (0..A as u32).map(|i| (i % 251) as u8).collect()
}

/// A single-file torrent over `a_bytes`, real SHA-1 pieces.
fn solo(name: &str) -> Vec<u8> {
    let mut pieces = Vec::new();
    for chunk in a_bytes().chunks(16384) {
        pieces.extend_from_slice(&Sha1::digest(chunk));
    }
    let mut v = format!(
        "d4:infod6:lengthi{A}e4:name{}:{name}12:piece lengthi16384e6:pieces{}:",
        name.len(),
        pieces.len()
    )
    .into_bytes();
    v.extend_from_slice(&pieces);
    v.extend_from_slice(b"ee");
    v
}

fn hash_of(bytes: &[u8]) -> String {
    let m = typhon_engine::torrent::metainfo::parse_torrent_bytes(bytes).unwrap();
    typhon_engine::torrent::hex_encode(&m.info_hash)
}

async fn send(
    s: &TestState,
    method: &str,
    uri: &str,
    body: &str,
    key: bool,
) -> (StatusCode, String) {
    let mut req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/x-www-form-urlencoded");
    if key {
        req = req.header("X-API-Key", KEY);
    }
    let resp = super::router(s.state.clone())
        .oneshot(req.body(axum::body::Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn call(s: &TestState, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    send(s, method, uri, body, true).await
}

/// Add through the shim. `whole` puts the file in place and vouches for it.
async fn add(s: &TestState, torrent: &[u8], name: &str, paused: bool, whole: bool) {
    let b = "seedonlyboundary";
    let save = s.dir.join("data");
    std::fs::create_dir_all(&save).unwrap();
    if whole {
        std::fs::write(save.join(name), a_bytes()).unwrap();
    }
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in [
        ("savepath", save.to_string_lossy().into_owned()),
        ("paused", paused.to_string()),
        ("skip_checking", whole.to_string()),
    ] {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
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
    assert_eq!(resp.status(), StatusCode::OK);
}

fn held(s: &TestState, hash: &str) -> std::sync::Arc<typhon_engine::torrent::meta::TorrentState> {
    find_torrent(&s.state, hash).map(|(_, t)| t).expect("held")
}

fn j(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

/// The row the shim lists for `hash`, once no check is running on it.
async fn shim_row(s: &TestState, hash: &str) -> serde_json::Value {
    for _ in 0..300 {
        let (_, b) = call(s, "POST", "/api/v2/torrents/info", "").await;
        let row = j(&b)
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["hash"] == hash)
            .cloned()
            .expect("listed");
        if !row["state"].as_str().unwrap().starts_with("checking") {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the check does not end");
}

#[tokio::test]
async fn the_route_sets_clears_and_persists_the_flag() {
    let s = st("so-route");
    let t = solo("Solo");
    add(&s, &t, "Solo", true, false).await;
    let h = hash_of(&t);
    let uri = format!("/api/torrents/{h}/seed-only");

    let (code, body) = call(&s, "GET", &uri, "").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(j(&body)["copies"][0]["seed_only"], false);

    let (code, body) = call(&s, "POST", &uri, r#"{"seed_only": true}"#).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(j(&body)["copies"][0]["seed_only"], true);
    assert_eq!(j(&body)["copies"][0]["complete"], false);
    assert!(held(&s, &h).is_seed_only(), "live in the engine");
    let (_, body) = call(&s, "GET", &uri, "").await;
    assert_eq!(j(&body)["copies"][0]["seed_only"], true);

    // Persisted at once: the record an engine move or a restart reads.
    let (engine, t_state) = find_torrent(&s.state, &h).unwrap();
    let rd = s
        .state
        .engines
        .get(&engine)
        .unwrap()
        .manager
        .export_state(&t_state.info_hash)
        .unwrap();
    assert!(rd.seed_only);

    // The detail row and the listing say it.
    let (_, body) = call(&s, "GET", &format!("/api/race/torrents/{h}"), "").await;
    assert_eq!(j(&body)["seed_only"], true, "{body}");

    let (code, _) = call(&s, "POST", &uri, r#"{"seed_only": false}"#).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!held(&s, &h).is_seed_only());
}

#[tokio::test]
async fn the_route_refuses_what_it_cannot_do() {
    let s = st("so-refuse");
    let t = solo("Solo");
    add(&s, &t, "Solo", true, false).await;
    let h = hash_of(&t);
    let uri = format!("/api/torrents/{h}/seed-only");

    let (code, _) = call(
        &s,
        "POST",
        &format!("/api/torrents/{}/seed-only", "ab".repeat(20)),
        r#"{"seed_only": true}"#,
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND, "no such torrent");
    let (code, _) = call(
        &s,
        "GET",
        &format!("/api/torrents/{}/seed-only", "ab".repeat(20)),
        "",
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    for bad in [
        "{}",
        r#"{"seed_only": "yes"}"#,
        r#"{"seed_only": true, "x": 1}"#,
        "nope",
    ] {
        let (code, _) = call(&s, "POST", &uri, bad).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (code, _) = send(&s, "POST", &uri, r#"{"seed_only": true}"#, false).await;
    assert_ne!(code, StatusCode::OK, "no key, no change");
    assert!(
        !held(&s, &h).is_seed_only(),
        "nothing was set by the refusals"
    );
}

/// ⭐ An incomplete seed-only torrent must not read as a download (the *arr
/// clients would think a transfer is under way) nor as completed (they would
/// import half a file): `stoppedDL`, running or stopped. Progress and what is
/// left stay true.
#[tokio::test]
async fn the_shim_shows_an_incomplete_seed_only_torrent_as_stopped_download() {
    let s = st("so-shim");
    let t = solo("Solo");
    add(&s, &t, "Solo", true, false).await;
    let h = hash_of(&t);
    call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/seed-only"),
        r#"{"seed_only": true}"#,
    )
    .await;

    // Stopped.
    let row = shim_row(&s, &h).await;
    assert_eq!(row["state"], "stoppedDL", "{row}");
    // Running: it is started, and still no download state.
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/resume",
        &format!("hashes={h}"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let row = shim_row(&s, &h).await;
    assert_eq!(row["state"], "stoppedDL", "running: {row}");
    assert!(row["progress"].as_f64().unwrap() < 1.0);
    assert_eq!(row["amount_left"], A as u64, "left stays true");
    assert_eq!(held(&s, &h).bytes_left(), A as u64);
    let (code, _) = call(&s, "POST", "/api/v2/torrents/stop", &format!("hashes={h}")).await;
    assert_eq!(code, StatusCode::OK);
    let row = shim_row(&s, &h).await;
    assert_eq!(row["state"], "stoppedDL", "stopped again: {row}");

    // The native state is not "downloading" either.
    let (_, body) = call(&s, "GET", &format!("/api/race/torrents/{h}"), "").await;
    assert_ne!(j(&body)["state"], "downloading", "{body}");
}

/// A complete seed-only torrent just seeds: the normal states, no `*DL`.
#[tokio::test]
async fn a_complete_seed_only_torrent_seeds_as_usual() {
    let s = st("so-whole");
    let t = solo("Whole");
    add(&s, &t, "Whole", false, true).await;
    let h = hash_of(&t);
    let (code, _) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/seed-only"),
        r#"{"seed_only": true}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let row = shim_row(&s, &h).await;
    let state = row["state"].as_str().unwrap();
    assert!(matches!(state, "uploading" | "stalledUP"), "{row}");
    assert_eq!(row["amount_left"], 0);
}
