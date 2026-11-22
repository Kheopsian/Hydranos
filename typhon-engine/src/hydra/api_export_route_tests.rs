use super::testing::*;
use super::*;
use std::io::Read;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let url = "https://t.example/PASSKEY/announce";
    format!(
        "d8:announce{}:{url}4:infod6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}ee",
        url.len(),
        name.len(),
        "A".repeat(20)
    )
    .into_bytes()
}

fn populated(tag: &str) -> (TestState, String, Vec<u8>) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let bytes = torrent_bytes("alpha");
    let (hash, _) = add_torrent_bytes(&s.state, &bytes, "", "/tmp", "", true, true, "hoard")
        .expect("added");
    (s, hash, bytes)
}

fn form(pairs: &[(&str, &str)]) -> axum::extract::Form<Fields> {
    axum::extract::Form(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

async fn body(resp: Response) -> Vec<u8> {
    axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec()
}

#[tokio::test]
async fn an_export_refuses_a_caller_with_no_key() {
    let (s, hash, _) = populated("export-auth");
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        HeaderMap::new(),
        form(&[("hashes", &hash)]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_zip_export_downloads_the_stored_torrent() {
    let (s, hash, bytes) = populated("export-zip");
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", &hash), ("format", "zip")]),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h.get("content-type").unwrap(), "application/zip");
    assert_eq!(
        h.get("content-encoding").unwrap(),
        "identity",
        "the gzip layer must stay off"
    );
    assert!(h
        .get("content-disposition")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("attachment;"));
    let mut z =
        zip::ZipArchive::new(std::io::Cursor::new(body(resp).await)).expect("a valid zip");
    assert_eq!(z.len(), 1);
    let mut got = Vec::new();
    z.by_index(0).unwrap().read_to_end(&mut got).unwrap();
    assert_eq!(got, bytes, "the .torrent as it was added, byte for byte");
}

#[tokio::test]
async fn stripping_trackers_keeps_the_passkey_out_of_the_zip() {
    let (s, hash, _) = populated("export-strip");
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", &hash), ("strip_trackers", "1")]),
    )
    .await;
    let zip = body(resp).await;
    assert!(!zip.windows(7).any(|w| w == b"PASSKEY"));
}

const NEW_TIERS: [&[&str]; 2] = [
    &[
        "https://new.example/announce",
        "https://new2.example/announce",
    ],
    &["https://new3.example/announce"],
];

/// Edit the trackers of `hash` through the API, as the editor does.
async fn edit_trackers(s: &TestState, hash: &str) {
    let body = serde_json::json!({"op": "set", "tiers": NEW_TIERS}).to_string();
    let resp = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(hash.to_string()),
        RawQuery(None),
        keyed(KEY),
        body,
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "the edit must be accepted");
}

/// The bytes of the `info` value: from `4:info` to the dict's closing `e`.
/// The test torrents carry `info` as their last key.
fn info_bytes(torrent: &[u8]) -> &[u8] {
    let at = torrent
        .windows(6)
        .position(|w| w == b"4:info")
        .expect("an info key");
    &torrent[at + 6..torrent.len() - 1]
}

fn tiers_of(torrent: &[u8]) -> Vec<Vec<String>> {
    typhon_engine::torrent::metainfo::parse_torrent_bytes(torrent)
        .expect("a parseable .torrent")
        .into_trackers()
}

fn new_tiers() -> Vec<Vec<String>> {
    NEW_TIERS
        .iter()
        .map(|t| t.iter().map(|u| u.to_string()).collect())
        .collect()
}

/// A tracker edit changes the live list and the resume record, never the
/// stored blob. Every way of handing the `.torrent` out must still carry
/// the torrent's CURRENT trackers, with the info dict untouched.
#[tokio::test]
async fn every_export_path_carries_the_edited_trackers() {
    use tower::ServiceExt;
    let (s, hash, original) = populated("export-edited");
    edit_trackers(&s, &hash).await;

    // zip
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", &hash)]),
    )
    .await;
    let mut z =
        zip::ZipArchive::new(std::io::Cursor::new(body(resp).await)).expect("a valid zip");
    let mut zipped = Vec::new();
    z.by_index(0).unwrap().read_to_end(&mut zipped).unwrap();

    // native file route
    let resp = super::get_torrent_file(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    let native = body(resp).await;

    // qBittorrent shim, GET and POST
    let mut all = vec![("zip", zipped), ("native", native)];
    for (method, uri, payload) in [
        (
            "GET",
            format!("/api/v2/torrents/export?hash={hash}"),
            String::new(),
        ),
        (
            "POST",
            "/api/v2/torrents/export".to_string(),
            format!("hash={hash}"),
        ),
    ] {
        let req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-API-Key", KEY)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(payload))
            .unwrap();
        let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        all.push((method, body(resp).await));
    }

    for (path, got) in all {
        assert_eq!(tiers_of(&got), new_tiers(), "{path}: the edited trackers");
        assert_eq!(
            info_bytes(&got),
            info_bytes(&original),
            "{path}: the info dict, so the info hash, is untouched"
        );
    }

    // CSV: tracker hosts, never URLs.
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", &hash), ("format", "csv")]),
    )
    .await;
    let csv = String::from_utf8(body(resp).await).unwrap();
    assert!(
        csv.contains("new.example new2.example new3.example"),
        "{csv}"
    );
    assert!(!csv.contains("t.example"), "the old host is gone: {csv}");
}

/// `strip_trackers` still removes everything from an edited torrent.
#[tokio::test]
async fn stripping_still_strips_an_edited_torrent() {
    let (s, hash, _) = populated("export-edited-strip");
    edit_trackers(&s, &hash).await;
    let resp = super::post_torrent_export(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        form(&[("hashes", &hash), ("strip_trackers", "1")]),
    )
    .await;
    let mut z =
        zip::ZipArchive::new(std::io::Cursor::new(body(resp).await)).expect("a valid zip");
    let mut got = Vec::new();
    z.by_index(0).unwrap().read_to_end(&mut got).unwrap();
    assert!(tiers_of(&got).is_empty(), "no announce survives");
    assert!(!got.windows(8).any(|w| w == b"announce"));
    assert!(!got.windows(11).any(|w| w == b"new.example"));
}

#[tokio::test]
async fn a_bad_format_or_no_hash_is_refused_before_anything_is_sent() {
    let (s, hash, _) = populated("export-bad");
    for (pairs, why) in [
        (
            vec![("hashes", hash.as_str()), ("format", "tar")],
            "unknown format",
        ),
        (vec![("hashes", "not-a-hash")], "no usable hash"),
        (vec![], "no hashes at all"),
    ] {
        let resp = super::post_torrent_export(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            form(&pairs),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{why}");
    }
}

/// ⭐ A Ctrl+A travels as its FILTER, through the real router: the daemon
/// resolves it, the browser never holds the hashes. An unknown filter key
/// and a filter that grew past the confirmed count are refused before a
/// byte of the download is sent.
#[tokio::test]
async fn an_export_by_filter_is_resolved_by_the_daemon() {
    use tower::ServiceExt;
    let (s, hash, _) = populated("export-filter");
    let post = |sel: serde_json::Value| {
        let payload = format!("format=txt&selection={}", enc_form(&sel.to_string()));
        axum::http::Request::builder()
            .method("POST")
            .uri("/api/torrents/export")
            .header("X-API-Key", KEY)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(payload))
            .unwrap()
    };
    let resp = super::router(s.state.clone())
        .oneshot(post(serde_json::json!({"filter": "", "expect": 1})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        String::from_utf8(body(resp).await).unwrap(),
        format!("{hash}\n")
    );

    let resp = super::router(s.state.clone())
        .oneshot(post(serde_json::json!({"filter": "limit=1", "expect": 1})))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let resp = super::router(s.state.clone())
        .oneshot(post(serde_json::json!({"filter": "", "expect": 0})))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CONFLICT,
        "one torrent, none confirmed"
    );
}

fn enc_form(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
