use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

/// A minimal single-file torrent. Bencode lengths are COMPUTED, never
/// counted by hand: a wrong one yields a file the parser refuses for a
/// reason unrelated to the test.
fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    // The info hash must differ per torrent, or the second add is refused
    // as a duplicate. The piece hash is what varies.
    let mut piece = [0xABu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    info.extend_from_slice(&piece);
    info.push(b'e');

    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn add(s: &TestState, engine_id: &str, name: &str) {
    let engines = s.engines.engines();
    let engine = engines
        .iter()
        .find(|e| e.id == engine_id)
        .unwrap_or_else(|| panic!("no engine {engine_id}"));
    engine
        .manager
        .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
        .unwrap_or_else(|e| panic!("add {name}: {e}"));
}

fn rows(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["rows"].as_array().expect("a rows array")
}

/// A fresh install has both engines and neither holds anything. The page
/// must be an empty list with honest counters, not a null the UI has to
/// special-case.
#[tokio::test]
async fn an_empty_engine_answers_an_empty_page_not_null() {
    let s = st("page-empty");
    let v = engine_page_value(&s.state, "race", "").await;
    assert_eq!(v["total"], serde_json::json!(0));
    assert_eq!(v["filtered"], serde_json::json!(0));
    assert!(rows(&v).is_empty());
}

/// An engine this node does not host is an empty page, never a panic: the
/// id comes off the wire.
#[tokio::test]
async fn an_unknown_engine_is_an_empty_page() {
    let s = st("page-unknown");
    let v = engine_page_value(&s.state, "no-such-engine", "").await;
    assert_eq!(v["total"], serde_json::json!(0));
    assert!(rows(&v).is_empty());
}

#[tokio::test]
async fn every_torrent_of_the_engine_is_listed_and_counted() {
    let s = st("page-list");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    let v = engine_page_value(&s.state, "race", "").await;
    assert_eq!(v["total"], serde_json::json!(2));
    assert_eq!(rows(&v).len(), 2);
}

/// ⭐ The engines do not share a catalogue: a torrent added to race must
/// not appear on the hoard's page. Collapsing the two is how a torrent
/// ends up running in one engine and listed under another.
#[tokio::test]
async fn one_engines_torrents_do_not_appear_on_the_others_page() {
    let s = st("page-sep");
    add(&s, "race", "only-in-race");
    let race = engine_page_value(&s.state, "race", "").await;
    let hoard = engine_page_value(&s.state, "hoard", "").await;
    assert_eq!(race["total"], serde_json::json!(1));
    assert_eq!(hoard["total"], serde_json::json!(0));
}

/// `total` is the library; `filtered` is what the query kept. Reporting
/// the filtered count as the total is how a search makes the library look
/// like it shrank.
#[tokio::test]
async fn a_search_narrows_filtered_but_never_total() {
    let s = st("page-search");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    let v = engine_page_value(&s.state, "race", "search=alpha").await;
    assert_eq!(
        v["total"],
        serde_json::json!(2),
        "the library did not shrink"
    );
    assert_eq!(v["filtered"], serde_json::json!(1));
    assert_eq!(rows(&v).len(), 1);
    assert_eq!(rows(&v)[0]["name"], serde_json::json!("alpha"));
}

#[tokio::test]
async fn a_search_matching_nothing_returns_no_rows_rather_than_all_of_them() {
    let s = st("page-nomatch");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "search=nosuchthing").await;
    assert_eq!(v["filtered"], serde_json::json!(0));
    assert!(
        rows(&v).is_empty(),
        "an unmatched search must not fall back to everything"
    );
}

#[tokio::test]
async fn a_search_ignores_case() {
    let s = st("page-case");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "search=ALPHA").await;
    assert_eq!(v["filtered"], serde_json::json!(1));
}

/// The window is honoured: a page of one returns one row, and the counters
/// still describe the whole library.
#[tokio::test]
async fn the_window_limits_the_rows_without_lying_about_the_totals() {
    let s = st("page-window");
    for n in ["alpha", "bravo", "charlie"] {
        add(&s, "race", n);
    }
    let v = engine_page_value(&s.state, "race", "limit=1&offset=0").await;
    assert_eq!(rows(&v).len(), 1);
    assert_eq!(v["total"], serde_json::json!(3));
    assert_eq!(v["filtered"], serde_json::json!(3));
}

/// An offset past the end is an empty page, not a wrapped one and not a
/// panic on a slice out of range.
#[tokio::test]
async fn an_offset_past_the_end_is_empty_not_a_panic() {
    let s = st("page-past");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "offset=9999&limit=10").await;
    assert!(rows(&v).is_empty());
    assert_eq!(v["total"], serde_json::json!(1));
}

/// A limit of zero is not "no rows": it is outside the allowed window and
/// gets clamped, because a page of zero rows is never what a client wants.
#[tokio::test]
async fn a_nonsense_window_is_clamped_rather_than_obeyed() {
    let s = st("page-clamp");
    add(&s, "race", "alpha");
    for q in ["limit=0", "limit=-5", "limit=abc", "offset=abc"] {
        let v = engine_page_value(&s.state, "race", q).await;
        assert_eq!(v["total"], serde_json::json!(1), "{q} must still answer");
        assert_eq!(rows(&v).len(), 1, "{q} must not produce an empty page");
    }
}

/// Sorting by name is case-insensitive, so "Zebra" does not lead "apple"
/// the way ASCII order would.
///
/// ⚠️ The default direction is DESCENDING (`order=asc` opts in): the
/// default sort is the added time, where newest-first is what anyone
/// wants. A test that assumes ascending is testing its own assumption.
#[tokio::test]
async fn sorting_by_name_folds_case() {
    let s = st("page-sort");
    add(&s, "race", "Zebra");
    add(&s, "race", "apple");

    let up = engine_page_value(&s.state, "race", "sort=name&order=asc").await;
    let names: Vec<&str> = rows(&up)
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(names, vec!["apple", "Zebra"], "ascending folds case");

    let down = engine_page_value(&s.state, "race", "sort=name").await;
    let names: Vec<&str> = rows(&down)
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(names, vec!["Zebra", "apple"], "descending is the default");
}

/// ⭐ Walking the list page by page gives the whole list, once each and in
/// order, for every sort and direction. Only the page itself is sorted,
/// after a second partition at the offset; a slip there shows as a row
/// missing or repeated between two pages, which reads as data loss. The
/// sorts on state, category and size are all ties here, so the order rests
/// on the hash tie-break carried in the key.
#[tokio::test]
async fn paging_through_the_list_gives_the_whole_list_in_order() {
    let s = st("page-walk");
    for i in 0..37 {
        add(&s, "race", &format!("Name{:02}", (i * 7) % 37));
    }
    let hash = |r: &serde_json::Value| {
        r["hash"]
            .as_str()
            .or(r["info_hash"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    for sort in ["name", "added_time", "total_size", "state", "category"] {
        for order in ["asc", "desc"] {
            let all = engine_page_value(
                &s.state,
                "race",
                &format!("sort={sort}&order={order}&limit=5000"),
            )
            .await;
            let want: Vec<String> = rows(&all).iter().map(hash).collect();
            assert_eq!(want.len(), 37);
            let mut got = Vec::new();
            let mut offset = 0;
            while offset < want.len() {
                let v = engine_page_value(
                    &s.state,
                    "race",
                    &format!("sort={sort}&order={order}&offset={offset}&limit=6"),
                )
                .await;
                got.extend(rows(&v).iter().map(hash));
                offset += 6;
            }
            assert_eq!(got, want, "sort={sort} order={order}");
        }
    }
}

/// An unknown sort must still answer, in some stable order, rather than
/// refusing the page.
#[tokio::test]
async fn an_unknown_sort_still_answers_a_page() {
    let s = st("page-badsort");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    let v = engine_page_value(&s.state, "race", "sort=no_such_column").await;
    assert_eq!(rows(&v).len(), 2);
}

/// A search on an info hash is the operator pasting one in. It is matched
/// as hex against the hash, not as text against the name.
#[tokio::test]
async fn a_hex_search_matches_the_info_hash() {
    let s = st("page-hex");
    add(&s, "race", "alpha");
    let all = engine_page_value(&s.state, "race", "").await;
    let hash = rows(&all)[0]["info_hash"]
        .as_str()
        .expect("a row carries its info hash")
        .to_string();
    let v = engine_page_value(&s.state, "race", &format!("search={}", &hash[..12])).await;
    assert_eq!(
        v["filtered"],
        serde_json::json!(1),
        "a hash prefix finds its torrent"
    );
}

/// The routes on top of the page carry the same gate as everything else.
#[tokio::test]
async fn the_page_routes_refuse_a_caller_with_no_key() {
    let s = st("page-auth");
    for resp in [
        get_race_torrents(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        get_hoard_torrents(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
    ] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn the_page_routes_answer_a_caller_with_the_key() {
    let s = st("page-ok");
    add(&s, "race", "alpha");
    let resp = get_race_torrents(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    // This route answers the ROWS themselves, not a page object: it is the
    // qBittorrent-shaped listing, which is a bare array.
    let rows = body.as_array().expect("a bare array of rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], serde_json::json!("alpha"));
}
