use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        "N".repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

async fn add(s: &TestState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let r = post_torrent_add(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let code = r.status();
    (code, body_json(r).await)
}

/// A request the daemon refuses is not a crash: a v2-only magnet is the
/// caller's to fix (400), never a 500.
#[tokio::test]
async fn a_refused_add_is_not_a_server_error() {
    let s = state_from(
        "native-add-refused",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let (code, v) = add(
        &s,
        serde_json::json!({
            "magnet_uri": format!("magnet:?xt=urn:btmh:1220{}", "ab".repeat(32)),
            "save_path": s.dir.display().to_string(),
        }),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
}

/// The Add tab's options do what they say: the mode picks the engine,
/// "add without starting" holds the torrent, the subfolder is made for a
/// single file. 4.3 read none of them on this route.
#[tokio::test]
async fn the_add_options_are_applied() {
    let s = state_from("native-add", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let file = s.dir.join("book.epub.torrent");
    std::fs::write(&file, torrent_bytes("book.epub")).unwrap();
    let base = s.dir.join("lib").display().to_string();
    let (code, v) = add(
        &s,
        serde_json::json!({
            "torrent_path": file.display().to_string(), "save_path": base,
            "mode": "hoard", "stopped": true, "create_subfolder": true, "seed_mode": true,
        }),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["mode"], "hoard");
    let hash = v["info_hash"].as_str().unwrap().to_string();
    let t = find_copy(&s.state, "hoard", &hash).expect("in hoard");
    assert!(
        t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
        "added without starting"
    );
    assert_eq!(
        *t.save_path.read(),
        std::path::Path::new(&base).join("book")
    );

    // The same again: already added is a conflict, not a server error.
    let (code, _) = add(&s, serde_json::json!({"torrent_path": file.display().to_string(), "save_path": base, "mode": "hoard"})).await;
    assert_eq!(code, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_refusal_says_what_was_wrong() {
    let s = state_from(
        "native-add-refuse",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let (code, v) = add(
        &s,
        serde_json::json!({"torrent_path": "/nonexistent/x.torrent", "save_path": "/tmp"}),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert!(!v["error"].as_str().unwrap().starts_with("race:"), "{v}");
    let (code, _) = add(
        &s,
        serde_json::json!({"torrent_path": "/x", "mode": "turbo"}),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = add(&s, serde_json::json!({})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}
