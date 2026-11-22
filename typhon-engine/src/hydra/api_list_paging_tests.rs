use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const N: usize = 3000;

/// A single-file torrent whose info hash is its own: the piece hash
/// carries the number.
fn torrent_bytes(i: usize) -> Vec<u8> {
    let name = format!("t{i}");
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xABu8; 20];
    piece[..8].copy_from_slice(&(i as u64).to_be_bytes());
    info.extend_from_slice(&piece);
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn filled(tag: &str, engine_id: &str) -> TestState {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let engines = s.engines.engines();
    let engine = engines.iter().find(|e| e.id == engine_id).expect("engine");
    for i in 0..N {
        engine
            .manager
            .add_torrent_bytes(&torrent_bytes(i), "/tmp", true, true)
            .unwrap_or_else(|e| panic!("add {i}: {e}"));
    }
    s
}

async fn get(s: &TestState, uri: &str) -> serde_json::Value {
    let req = axum::http::Request::builder()
        .uri(uri)
        .header("X-API-Key", KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = super::router(s.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200, "{uri}");
    body_json(resp).await
}

fn built() -> usize {
    ENGINE_ROWS_BUILT.with(|c| c.get())
}

fn hashes(v: &serde_json::Value) -> Vec<String> {
    v.as_array()
        .expect("an array")
        .iter()
        .map(|r| r["info_hash"].as_str().unwrap().to_string())
        .collect()
}

/// `limit=1` is one row, and the answer is not made by building the whole
/// catalogue and cutting it: on a million torrents that was a lock held for
/// the length of a full-library walk and a 30 s timeout.
#[tokio::test]
async fn limit_one_renders_one_row_without_building_the_catalogue() {
    let s = filled("list-limit-1", "hoard");
    let before = built();
    let v = get(&s, "/api/hoard/torrents?limit=1").await;
    let worked = built() - before;
    assert_eq!(v.as_array().unwrap().len(), 1, "one row asked, one row due");
    assert!(worked <= 1, "{worked} rows built to answer limit=1");
}

/// `offset` and `limit` take the same window the page route takes, in its
/// order (newest first, ties by hash), with the same fields; on the engine
/// route and on the race alias too. Without a `limit` nothing changes.
#[tokio::test]
async fn limit_and_offset_window_the_list_in_the_page_order() {
    let s = filled("list-window", "hoard");
    let page = get(&s, "/api/hoard/page?offset=5&limit=7").await;
    let want: Vec<String> = hashes(&page["rows"]);
    assert_eq!(want.len(), 7);

    let before = built();
    let v = get(&s, "/api/hoard/torrents?offset=5&limit=7").await;
    assert_eq!(hashes(&v), want);
    assert_eq!(v, page["rows"], "same fields as the page rows");
    let v = get(&s, "/api/engines/hoard/torrents?offset=5&limit=7").await;
    assert_eq!(hashes(&v), want);
    assert!(built() - before <= 1, "a window must not build the list");

    let tail = get(
        &s,
        &format!("/api/hoard/torrents?offset={}&limit=50", N - 3),
    )
    .await;
    assert_eq!(tail.as_array().unwrap().len(), 3, "past the end is short");

    let before = built();
    let all = get(&s, "/api/hoard/torrents").await;
    assert_eq!(all.as_array().unwrap().len(), N, "no limit, whole list");
    assert_eq!(built() - before, N);
}

#[tokio::test]
async fn race_list_honours_limit_too() {
    let s = filled("list-race", "race");
    let before = built();
    let v = get(&s, "/api/race/torrents?limit=2").await;
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert!(built() - before <= 2);
    let all = get(&s, "/api/race/torrents").await;
    assert_eq!(all.as_array().unwrap().len(), N);
}

/// `/api/race/torrents` spans every race engine: the window is cut from
/// their union, in the page order, not per engine.
#[tokio::test]
async fn race_window_spans_every_race_engine() {
    let s = state_from(
        "list-race2",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[[engine]]\nname = \"vpn7\"\nrole = \"race\"\n\
             [engine.session]\nlisten_port = 26991\n"
        ),
    );
    let engines = s.engines.engines();
    for (k, id) in ["race", "vpn7"].iter().enumerate() {
        let e = engines.iter().find(|e| e.id == *id).expect("engine");
        for i in 0..20 {
            e.manager
                .add_torrent_bytes(&torrent_bytes(k * 100 + i), "/tmp", true, true)
                .unwrap();
        }
    }
    let all = get(&s, "/api/race/torrents").await;
    assert_eq!(all.as_array().unwrap().len(), 40);
    let v = get(&s, "/api/race/torrents?offset=3&limit=30").await;
    let page = get(&s, "/api/race/page?offset=3&limit=30").await;
    assert_eq!(v.as_array().unwrap().len(), 30);
    assert_eq!(hashes(&v), hashes(&page["rows"]), "the window of the union");
}

/// With a filter, the window is taken from the filtered list.
#[tokio::test]
async fn limit_applies_to_the_filtered_list() {
    let s = filled("list-filter", "hoard");
    let v = get(&s, "/api/hoard/torrents?search=t12&limit=4").await;
    assert_eq!(v.as_array().unwrap().len(), 4);
    let all = get(&s, "/api/hoard/torrents?search=t12").await;
    assert!(all.as_array().unwrap().len() > 4);
}
