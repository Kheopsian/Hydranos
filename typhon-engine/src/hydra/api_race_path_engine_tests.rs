use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(b"e");
    let mut out = Vec::new();
    out.extend_from_slice(b"d4:infod");
    out.extend_from_slice(&info[1..]);
    out.extend_from_slice(b"e");
    out
}

/// A node holding one torrent, in RACE only.
fn race_only(tag: &str) -> (TestState, String) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (hash, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("racer"),
        "",
        "/tmp",
        "fr",
        true,
        true,
        "race",
    )
    .expect("added to race");
    (s, hash)
}

/// ⚠ The fixture first: the torrent must really be in race and NOT in hoard,
/// or the assertion below would pass for the wrong reason.
#[tokio::test]
async fn the_fixture_holds_the_torrent_in_race_only() {
    let (s, hash) = race_only("race-only-fixture");
    let store = s.state.store.lock().unwrap();
    assert!(
        store.resolve_hash_in("race", &hash).is_some(),
        "the torrent must be in race"
    );
    assert!(
        store.resolve_hash_in("hoard", &hash).is_none(),
        "the torrent must NOT be in hoard, or this fixture proves nothing"
    );
}

/// ⭐⭐ Setting a category through the RACE route must act on the RACE copy.
///
/// `torrent_write!` defaults its engine to the literal `"hoard"` when the
/// request carries no `?agent=`, and the front end's category call carries
/// none -- `fetch(`/api/${mode}/torrents/${hash}/category`)`, no query at
/// all. So the engine named in the PATH was ignored: the write was aimed at
/// hoard whatever the URL said.
///
/// On a torrent that only race holds, that is a 404 for a torrent which
/// plainly exists. On one held by both, it is worse and silent: the label
/// lands on the other copy and the answer is still `{"status":"ok"}`.
#[tokio::test]
async fn the_race_category_route_acts_on_the_race_copy() {
    let (s, hash) = race_only("race-category");
    let resp = super::set_race_torrent_category(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"category": "Anime"}).to_string(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "the race route must find the race copy, got {:?}",
        resp.status()
    );
}
