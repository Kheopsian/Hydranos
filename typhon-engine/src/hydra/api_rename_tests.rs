//! Renaming the files of a stopped torrent, through the real router: the
//! native route and the qBittorrent `renameFile`. Nothing moves on disk; the
//! paths the torrent reads and writes change, and that survives a restart.

use super::testing::*;
use super::*;
use sha1::{Digest, Sha1};
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

const A: usize = 20_000;
const B: usize = 12_000;

fn a_bytes() -> Vec<u8> {
    (0..A as u32).map(|i| (i % 251) as u8).collect()
}

fn b_bytes() -> Vec<u8> {
    (0..B as u32).map(|i| (i % 239) as u8).collect()
}

/// A multi-file torrent `name/{a.bin,b.bin}` with real SHA-1 pieces.
fn pack(name: &str) -> Vec<u8> {
    let stream: Vec<u8> = a_bytes().into_iter().chain(b_bytes()).collect();
    let mut pieces = Vec::new();
    for chunk in stream.chunks(16384) {
        pieces.extend_from_slice(&Sha1::digest(chunk));
    }
    let mut v = b"d4:infod5:filesl".to_vec();
    v.extend_from_slice(format!("d6:lengthi{A}e4:pathl5:a.binee").as_bytes());
    v.extend_from_slice(format!("d6:lengthi{B}e4:pathl5:b.binee").as_bytes());
    v.extend_from_slice(
        format!(
            "e4:name{}:{name}12:piece lengthi16384e6:pieces{}:",
            name.len(),
            pieces.len()
        )
        .as_bytes(),
    );
    v.extend_from_slice(&pieces);
    v.extend_from_slice(b"ee");
    v
}

/// A single-file torrent over `a_bytes`.
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
) -> (StatusCode, String, String) {
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
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ct, String::from_utf8_lossy(&bytes).into_owned())
}

async fn call(s: &TestState, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let (c, _, b) = send(s, method, uri, body, true).await;
    (c, b)
}

async fn add(s: &TestState, torrent: &[u8], paused: bool) {
    let b = "renameboundary";
    let save = s.dir.join("data");
    std::fs::create_dir_all(&save).unwrap();
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in [
        ("savepath", save.to_string_lossy().into_owned()),
        ("paused", paused.to_string()),
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

fn engine_dir(s: &TestState, hash: &str) -> std::path::PathBuf {
    held(s, hash).save_path.read().clone()
}

async fn native_paths(s: &TestState, h: &str) -> Vec<String> {
    let (code, body) = call(s, "GET", &format!("/api/torrents/{h}/files"), "").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["path"].as_str().unwrap().to_string())
        .collect()
}

async fn shim_names(s: &TestState, h: &str) -> Vec<String> {
    let (_, body) = call(s, "POST", "/api/v2/torrents/files", &format!("hash={h}")).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect()
}

fn rename_body(from: &str, to: &str) -> String {
    serde_json::json!({"from": from, "to": to}).to_string()
}

#[tokio::test]
async fn the_native_route_renames_a_file_at_its_index_and_says_so() {
    let s = st("rn-native");
    let t = pack("Pack");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/files/rename"),
        &rename_body("a.bin", "sub/x.mkv"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["files"][0]["path"], "sub/x.mkv");
    assert_eq!(v["files"][0]["index"], 0);
    assert_eq!(v["files"][1]["path"], "b.bin");
    assert_eq!(native_paths(&s, &h).await, ["sub/x.mkv", "b.bin"]);
    assert_eq!(shim_names(&s, &h).await, ["Pack/sub/x.mkv", "Pack/b.bin"]);
}

#[tokio::test]
async fn the_native_route_refuses_what_it_cannot_do_and_says_why() {
    let s = st("rn-native-refuse");
    let t = pack("Pack");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let url = format!("/api/torrents/{h}/files/rename");
    for bad in [
        "", "/abs.bin", "../x", "a/../x", "./x", "a//x", "a\\x", "a/",
    ] {
        let (code, body) = call(&s, "POST", &url, &rename_body("a.bin", bad)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "to={bad:?}: {body}");
    }
    let (code, _) = call(&s, "POST", &url, &rename_body("nope.bin", "x.bin")).await;
    assert_eq!(
        code,
        StatusCode::BAD_REQUEST,
        "from is not a file of the torrent"
    );
    let (code, body) = call(&s, "POST", &url, &rename_body("a.bin", "b.bin")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("error"), "{body}");
    let (code, _) = call(
        &s,
        "POST",
        &format!("/api/torrents/{}/files/rename", "0".repeat(40)),
        &rename_body("a.bin", "x.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = call(
        &s,
        "POST",
        &format!("{url}?engine=nowhere"),
        &rename_body("a.bin", "x.bin"),
    )
    .await;
    assert_eq!(
        code,
        StatusCode::NOT_FOUND,
        "an engine that does not hold it"
    );
    assert_eq!(
        native_paths(&s, &h).await,
        ["a.bin", "b.bin"],
        "nothing changed"
    );

    // Data on disk at `from`: nothing is ever moved, so it is refused.
    let dir = engine_dir(&s, &h).join("Pack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.bin"), a_bytes()).unwrap();
    let (code, body) = call(&s, "POST", &url, &rename_body("a.bin", "x.bin")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("move"),
        "the message says nothing is moved: {body}"
    );
    assert_eq!(native_paths(&s, &h).await, ["a.bin", "b.bin"]);
}

#[tokio::test]
async fn a_running_torrent_is_not_renamed() {
    let s = st("rn-running");
    let t = pack("Pack");
    add(&s, &t, false).await;
    let h = hash_of(&t);
    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/files/rename"),
        &rename_body("a.bin", "x.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    let (code, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack/a.bin&newPath=Pack/x.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(native_paths(&s, &h).await, ["a.bin", "b.bin"]);
}

#[tokio::test]
async fn the_shim_renames_with_the_root_in_the_path() {
    let s = st("rn-shim");
    let t = pack("Pack");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let (code, ct, body) = send(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fb.bin&newPath=Pack%2Fz%2Fy.bin"),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body, "");
    let _ = ct;
    assert_eq!(shim_names(&s, &h).await, ["Pack/a.bin", "Pack/z/y.bin"]);
    assert_eq!(native_paths(&s, &h).await, ["a.bin", "z/y.bin"]);

    let (code, ct, body) = send(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin&newPath=Pack%2Fz%2Fy.bin"),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert!(ct.starts_with("text/plain"), "{ct}");
    assert!(!body.is_empty() && !body.starts_with('{'), "{body}");
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fnope&newPath=Pack%2Fq"),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "newPath is required");
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin&newPath=Pack%2F..%2Fa"),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={}&oldPath=a&newPath=b", "0".repeat(40)),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // A multi-file torrent's file keeps a folder; one outside the save path is no path at all.
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin&newPath=loose.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin&newPath=%2Fetc%2Fa.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(shim_names(&s, &h).await, ["Pack/a.bin", "Pack/z/y.bin"]);
}

async fn info_row(s: &TestState, h: &str) -> serde_json::Value {
    let (_, body) = call(s, "GET", "/api/v2/torrents/info", "").await;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    rows.into_iter().find(|r| r["hash"] == h).expect("listed")
}

/// The case a cross-seed tool makes: a local copy whose FOLDER has another
/// name than the .torrent's, renamed file by file with the folder in the path.
#[tokio::test]
async fn a_torrent_moves_to_another_folder_file_by_file() {
    let s = st("rn-folder");
    let t = pack("Pack");
    // The test engines have no store to read piece hashes from.
    for e in s.engines.engines() {
        let blob = t.clone();
        e.manager
            .set_blob_source(std::sync::Arc::new(move |_: &str| Some(blob.clone())));
    }
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let save = engine_dir(&s, &h);
    std::fs::create_dir_all(save.join("Copy.B")).unwrap();
    std::fs::write(save.join("Copy.B/one.mkv"), a_bytes()).unwrap();
    std::fs::write(save.join("Copy.B/two.nfo"), b_bytes()).unwrap();

    // First file, through the shim: folder and name both change.
    let (code, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fa.bin&newPath=Copy.B%2Fone.mkv"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    // One of two moved: coherent, split between two folders.
    assert_eq!(shim_names(&s, &h).await, ["Copy.B/one.mkv", "Pack/b.bin"]);
    let row = info_row(&s, &h).await;
    assert!(
        row["content_path"].as_str().unwrap().ends_with("/Pack"),
        "{row}"
    );
    let (_, body) = call(&s, "GET", &format!("/api/torrents/{h}/files"), "").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (
            v["files"][0]["folder"].as_str(),
            v["files"][0]["path"].as_str()
        ),
        (Some("Copy.B"), Some("one.mkv"))
    );
    assert_eq!(
        (
            v["files"][1]["folder"].as_str(),
            v["files"][1]["path"].as_str()
        ),
        (Some("Pack"), Some("b.bin"))
    );

    // Second file, natively: `folder` names where it goes.
    let url = format!("/api/torrents/{h}/files/rename");
    let (code, body) = call(
        &s,
        "POST",
        &url,
        r#"{"from":"b.bin","to":"two.nfo","folder":"Copy.B"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (
            v["files"][1]["folder"].as_str(),
            v["files"][1]["path"].as_str()
        ),
        (Some("Copy.B"), Some("two.nfo"))
    );
    assert_eq!(
        shim_names(&s, &h).await,
        ["Copy.B/one.mkv", "Copy.B/two.nfo"]
    );
    // Every file shares the new folder: that is the content path and the name.
    let row = info_row(&s, &h).await;
    assert!(
        row["content_path"].as_str().unwrap().ends_with("/Copy.B"),
        "{row}"
    );
    assert_eq!(row["name"], "Copy.B");
    // Not outside the save path, nor out of a folder.
    let (code, _) = call(
        &s,
        "POST",
        &url,
        r#"{"from":"one.mkv","to":"x","folder":".."}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(
        &s,
        "POST",
        &url,
        r#"{"from":"one.mkv","to":"x","from_folder":"Pack"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "no such file in that folder");

    // The data is there under the new folder: a recheck reaches 100 %.
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/recheck",
        &format!("hashes={h}"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let mut whole = false;
    for _ in 0..300 {
        let (_, body) = call(&s, "GET", &format!("/api/torrents/{h}/files"), "").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        if v["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["progress"] == 1.0)
        {
            whole = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(whole, "the recheck found the data under the new folder");
}

#[tokio::test]
async fn several_engines_and_a_check_in_progress_are_409() {
    let s = st("rn-409");
    let t = pack("Pack");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let url = format!("/api/torrents/{h}/files/rename");
    // The same torrent in a second engine, as an engine move leaves it.
    let engines = s.engines.engines();
    let from = engines
        .iter()
        .find(|e| {
            e.manager
                .get(&typhon_engine::torrent::hex_decode(&h).unwrap())
                .is_some()
        })
        .unwrap();
    let other = engines
        .iter()
        .find(|e| e.id != from.id)
        .expect("two engines");
    let blob = t.clone();
    other
        .manager
        .set_blob_source(std::sync::Arc::new(move |_: &str| Some(blob.clone())));
    let rd = from
        .manager
        .export_state(&typhon_engine::torrent::hex_decode(&h).unwrap())
        .expect("exported");
    other.manager.import_state(&rd).expect("imported");

    let (code, body) = call(&s, "POST", &url, &rename_body("a.bin", "x.bin")).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains("several engines") && body.contains(&from.id) && body.contains(&other.id),
        "{body}"
    );
    let (code, body) = call(
        &s,
        "POST",
        &format!("{url}?engine={}", other.id),
        &rename_body("a.bin", "x.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let ih = typhon_engine::torrent::hex_decode(&h).unwrap();
    assert_eq!(
        other.manager.get(&ih).unwrap().meta.disk_rel(0),
        std::path::PathBuf::from("Pack/x.bin")
    );
    assert_eq!(
        from.manager.get(&ih).unwrap().meta.disk_rel(0),
        std::path::PathBuf::from("Pack/a.bin"),
        "the other copy is untouched"
    );

    // A check under way: nothing is renamed under it.
    other
        .manager
        .get(&ih)
        .unwrap()
        .rechecking
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, body) = call(
        &s,
        "POST",
        &format!("{url}?engine={}", other.id),
        &rename_body("x.bin", "y.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("checked"), "{body}");
    other
        .manager
        .get(&ih)
        .unwrap()
        .rechecking
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // The shim renames every copy: if the LAST one would refuse, the first
    // must not have been renamed already.
    let holders: Vec<_> = engines
        .iter()
        .filter(|e| e.manager.get(&ih).is_some())
        .collect();
    let (first, last) = (holders[0], holders[holders.len() - 1]);
    last.manager
        .get(&ih)
        .unwrap()
        .rechecking
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fb.bin&newPath=Pack%2Fz.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        first.manager.get(&ih).unwrap().meta.disk_rel(1),
        std::path::PathBuf::from("Pack/b.bin"),
        "the copy that would have accepted was left alone"
    );
    last.manager
        .get(&ih)
        .unwrap()
        .rechecking
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let (code, body) = call(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack%2Fb.bin&newPath=Pack%2Fz.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    for e in &holders {
        assert_eq!(
            e.manager.get(&ih).unwrap().meta.disk_rel(1),
            std::path::PathBuf::from("Pack/z.bin")
        );
    }
}

/// A copy made from a torrent whose files were renamed reads them at the same
/// paths: not the .torrent's, where it would be a seed with nothing to serve.
#[tokio::test]
async fn a_copy_to_another_engine_keeps_the_renamed_files() {
    let s = st("rn-copy");
    let t = pack("Pack");
    for e in s.engines.engines() {
        let blob = t.clone();
        e.manager
            .set_blob_source(std::sync::Arc::new(move |_: &str| Some(blob.clone())));
    }
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let ih = typhon_engine::torrent::hex_decode(&h).unwrap();
    let dir = engine_dir(&s, &h).join("Pack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.mkv"), a_bytes()).unwrap();
    std::fs::write(dir.join("two.nfo"), b_bytes()).unwrap();
    let url = format!("/api/torrents/{h}/files/rename");
    assert_eq!(
        call(&s, "POST", &url, &rename_body("a.bin", "one.mkv"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s, "POST", &url, &rename_body("b.bin", "two.nfo"))
            .await
            .0,
        StatusCode::OK
    );
    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/recheck",
        &format!("hashes={h}"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    for _ in 0..300 {
        let (_, body) = call(&s, "GET", &format!("/api/torrents/{h}/files"), "").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        if v["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["progress"] == 1.0)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let engines = s.engines.engines();
    let from = engines
        .iter()
        .find(|e| e.manager.get(&ih).is_some())
        .unwrap();
    let other = engines
        .iter()
        .find(|e| e.id != from.id)
        .expect("two engines");
    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/copy"),
        &format!(r#"{{"engine":"{}"}}"#, other.id),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let copy = other.manager.get(&ih).expect("the copy");
    assert_eq!(
        copy.meta.disk_rel(0),
        std::path::PathBuf::from("Pack/one.mkv")
    );
    assert_eq!(
        copy.meta.disk_rel(1),
        std::path::PathBuf::from("Pack/two.nfo")
    );
    assert_ne!(
        copy.status.load(std::sync::atomic::Ordering::Relaxed),
        typhon_engine::torrent::meta::TorrentStatus::Error as u8,
        "a seed whose files are all there"
    );
}

/// A torrent named `a/b` has the folder `a/b`, renamed or not.
#[tokio::test]
async fn a_torrent_named_with_a_slash_lists_its_whole_folder() {
    let s = st("rn-slash");
    let t = pack("a/b");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let (_, body) = call(&s, "GET", &format!("/api/torrents/{h}/files"), "").await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (
            v["files"][0]["folder"].as_str(),
            v["files"][0]["path"].as_str()
        ),
        (Some("a/b"), Some("a.bin"))
    );
    let row = info_row(&s, &h).await;
    assert!(
        row["content_path"].as_str().unwrap().ends_with("/a/b"),
        "{row}"
    );
    assert_eq!(row["name"], "a/b");
    assert_eq!(shim_names(&s, &h).await, ["a/b/a.bin", "a/b/b.bin"]);
    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/files/rename"),
        &rename_body("a.bin", "x.bin"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(shim_names(&s, &h).await, ["a/b/x.bin", "a/b/b.bin"]);
}

#[tokio::test]
async fn a_recheck_reads_the_data_under_the_new_names() {
    let s = st("rn-recheck");
    let t = pack("Pack");
    // The test engines have no store to read piece hashes from.
    for e in s.engines.engines() {
        let blob = t.clone();
        e.manager
            .set_blob_source(std::sync::Arc::new(move |_: &str| Some(blob.clone())));
    }
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let dir = engine_dir(&s, &h).join("Pack");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("one.mkv"), a_bytes()).unwrap();
    std::fs::write(dir.join("two.nfo"), b_bytes()).unwrap();
    let url = format!("/api/torrents/{h}/files/rename");
    assert_eq!(
        call(&s, "POST", &url, &rename_body("a.bin", "one.mkv"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&s, "POST", &url, &rename_body("b.bin", "two.nfo"))
            .await
            .0,
        StatusCode::OK
    );

    let (code, _) = call(
        &s,
        "POST",
        "/api/v2/torrents/recheck",
        &format!("hashes={h}"),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    let mut whole = false;
    for _ in 0..300 {
        let (_, body) = call(&s, "GET", &format!("/api/torrents/{h}/files"), "").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        if v["files"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["progress"] == 1.0)
        {
            whole = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let t = held(&s, &h);
    assert!(
        whole,
        "the recheck found the data under the new names: status {} files {}",
        t.status.load(std::sync::atomic::Ordering::Relaxed),
        call(&s, "GET", &format!("/api/torrents/{h}/files"), "")
            .await
            .1
    );
}

#[tokio::test]
async fn a_single_file_torrent_renamed_has_the_new_content_path_and_the_same_export() {
    let s = st("rn-solo");
    let t = solo("old.mkv");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/torrents/{h}/files/rename"),
        &rename_body("old.mkv", "new.mkv"),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let (_, body) = call(&s, "GET", "/api/v2/torrents/info", "").await;
    let rows: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let row = rows
        .iter()
        .find(|r| r["hash"] == h.as_str())
        .expect("listed");
    assert!(
        row["content_path"].as_str().unwrap().ends_with("/new.mkv"),
        "{row}"
    );
    assert_eq!(shim_names(&s, &h).await, ["new.mkv"]);

    // The .torrent that goes out is the one that came in.
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v2/torrents/export?hash={h}"))
        .header("X-API-Key", KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let exported = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(hash_of(&exported), h, "same info-hash");
    assert!(exported.as_ref() == t.as_slice(), "the original bytes");
}

#[tokio::test]
async fn both_routes_refuse_a_caller_with_no_key() {
    let s = st("rn-auth");
    let t = pack("Pack");
    add(&s, &t, true).await;
    let h = hash_of(&t);
    let (code, _, _) = send(
        &s,
        "POST",
        &format!("/api/torrents/{h}/files/rename"),
        &rename_body("a.bin", "x.bin"),
        false,
    )
    .await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    let (code, _, _) = send(
        &s,
        "POST",
        "/api/v2/torrents/renameFile",
        &format!("hash={h}&oldPath=Pack/a.bin&newPath=Pack/x.bin"),
        false,
    )
    .await;
    assert!(
        code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN,
        "{code}"
    );
    assert_eq!(
        native_paths(&s, &h).await,
        ["a.bin", "b.bin"],
        "and nothing was renamed"
    );
}
