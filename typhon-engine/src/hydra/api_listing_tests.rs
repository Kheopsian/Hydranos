//! The torrent listing the *arr stack, cross-seed and autobrr poll: what each
//! `filter` word keeps, what `category`, `hashes` and `tag` narrow to, and the
//! order and the page the answer comes in.

use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const SIZE: u64 = 16_384;

/// A single-file torrent, unique per name.
fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{SIZE}e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xCDu8; 20];
    for (i, b) in name.bytes().enumerate().take(20) {
        piece[i] = b;
    }
    info.extend_from_slice(&piece);
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

/// Every state a qBittorrent client can be told, in one order.
const STATES: [&str; 15] = [
    "downloading",
    "stalledDL",
    "checkingDL",
    "queuedDL",
    "allocating",
    "uploading",
    "stalledUP",
    "queuedUP",
    "checkingUP",
    "pausedDL",
    "pausedUP",
    "stoppedDL",
    "stoppedUP",
    "error",
    "missingFiles",
];

fn kept(filter: &str) -> Vec<&'static str> {
    STATES
        .iter()
        .copied()
        .filter(|s| qbit_filter_matches(s, filter))
        .collect()
}

/// ⭐ Each filter word keeps exactly its states: a state one filter takes in
/// too many is a torrent a client sees in a list it did not ask for, and one
/// too few is a torrent it never sees.
#[test]
fn each_filter_word_keeps_exactly_its_states() {
    assert_eq!(kept("all"), STATES.to_vec());
    assert_eq!(
        kept("downloading"),
        [
            "downloading",
            "stalledDL",
            "checkingDL",
            "queuedDL",
            "allocating"
        ]
    );
    assert_eq!(
        kept("seeding"),
        ["uploading", "stalledUP", "queuedUP", "checkingUP"]
    );
    assert_eq!(
        kept("completed"),
        [
            "uploading",
            "stalledUP",
            "queuedUP",
            "checkingUP",
            "pausedUP",
            "stoppedUP"
        ]
    );
    let paused = ["pausedDL", "pausedUP", "stoppedDL", "stoppedUP"];
    assert_eq!(kept("paused"), paused);
    assert_eq!(kept("stopped"), paused, "qBittorrent 5's word for it");
    assert_eq!(kept("active"), ["downloading", "uploading"]);
    assert_eq!(
        kept("inactive"),
        [
            "stalledDL",
            "stalledUP",
            "pausedDL",
            "pausedUP",
            "stoppedDL",
            "stoppedUP"
        ]
    );
    assert_eq!(kept("stalled"), ["stalledDL", "stalledUP"]);
    assert_eq!(kept("stalled_uploading"), ["stalledUP"]);
    assert_eq!(kept("stalled_downloading"), ["stalledDL"]);
    assert_eq!(kept("errored"), ["error"]);
    let running: Vec<&str> = STATES
        .iter()
        .copied()
        .filter(|s| !paused.contains(s))
        .collect();
    assert_eq!(kept("resumed"), running);
    assert_eq!(kept("running"), running);
    assert_eq!(
        kept("no-such-filter"),
        STATES.to_vec(),
        "an unknown word lists everything, never an empty queue"
    );
}

/// Same-typed values are ordered on their own terms (2 before 10, not after),
/// anything else on its rendered form.
#[test]
fn listing_values_are_ordered_on_their_own_terms() {
    use serde_json::json;
    use std::cmp::Ordering::*;
    assert_eq!(qbit_value_cmp(&json!("a"), &json!("b")), Less);
    assert_eq!(qbit_value_cmp(&json!(2), &json!(10)), Less);
    assert_eq!(qbit_value_cmp(&json!(2.5), &json!(2)), Greater);
    assert_eq!(qbit_value_cmp(&json!(7), &json!(7.0)), Equal);
    assert_eq!(qbit_value_cmp(&json!(false), &json!(true)), Less);
    assert_eq!(qbit_value_cmp(&json!(null), &json!(null)), Equal);
    // A number against a string: "5" against "\"5\"", the quote sorts first.
    assert_eq!(qbit_value_cmp(&json!(5), &json!("5")), Greater);
    assert_eq!(qbit_value_cmp(&json!("5"), &json!(5)), Less);
}

async fn listing(s: &TestState, query: &str) -> Vec<serde_json::Value> {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/v2/torrents/info?{query}"))
        .header("X-API-Key", KEY)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = router(s.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{query}");
    match body_json(resp).await {
        serde_json::Value::Array(rows) => rows,
        other => panic!("{query}: not a list: {other}"),
    }
}

async fn names(s: &TestState, query: &str) -> Vec<String> {
    listing(s, query)
        .await
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect()
}

struct Library {
    s: TestState,
    hashes: Vec<String>,
}

/// la (race, tv, tags x,y), lb (race, films, tag y, stopped), lc (hoard, tv),
/// ld (hoard, no category).
fn library(tag: &str, with_data: bool) -> Library {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let mut hashes = Vec::new();
    for (name, engine, category, tags, paused) in [
        ("la", "race", "tv", "x,y", false),
        ("lb", "race", "films", "y", true),
        ("lc", "hoard", "tv", "", false),
        ("ld", "hoard", "", "", false),
    ] {
        if with_data {
            std::fs::write(dir.join(name), vec![0u8; SIZE as usize]).unwrap();
        }
        let (hash, _) = add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            category,
            dir.to_str().unwrap(),
            tags,
            paused,
            true,
            engine,
        )
        .expect("added");
        hashes.push(hash);
    }
    Library { s, hashes }
}

/// ⭐ A client configured with one category gets that category, from every
/// engine, and a torrent with none is listed under the engine that holds it.
#[tokio::test]
async fn a_category_scopes_the_listing_across_engines() {
    let l = library("list-category", true);
    let s = &l.s;
    assert_eq!(names(s, "category=tv&sort=name").await, ["la", "lc"]);
    assert_eq!(
        names(s, "category=tv&sort=name&reverse=true").await,
        ["lc", "la"]
    );
    assert_eq!(names(s, "category=films").await, ["lb"]);
    assert_eq!(
        names(s, "category=hoard").await,
        ["ld"],
        "no category: the engine's own name"
    );
    assert!(names(s, "category=race").await.is_empty());
    assert!(names(s, "category=nothing").await.is_empty());
    assert_eq!(names(s, "sort=name").await, ["la", "lb", "lc", "ld"]);
    let rows = listing(s, "category=tv&sort=name").await;
    assert_eq!(rows[0]["category"], "tv");
    assert_eq!(rows[0]["tags"], "x,y");
    assert_eq!(rows[0]["hash"], l.hashes[0]);
}

/// Hashes name torrents, `all` names none in particular, and a category and
/// hashes together narrow to what both say.
#[tokio::test]
async fn hashes_narrow_the_listing_and_all_is_no_filter() {
    let l = library("list-hashes", true);
    let s = &l.s;
    let (a, b, c) = (&l.hashes[0], &l.hashes[1], &l.hashes[2]);
    assert_eq!(
        names(s, &format!("hashes={a}|{b}&sort=name")).await,
        ["la", "lb"]
    );
    assert_eq!(
        names(s, &format!("hashes={}%2C{}&sort=name", a.to_uppercase(), c)).await,
        ["la", "lc"],
        "comma as well, any case"
    );
    assert_eq!(
        names(s, "hashes=all&sort=name").await,
        ["la", "lb", "lc", "ld"]
    );
    assert_eq!(names(s, &format!("hashes={a}&category=tv")).await, ["la"]);
    assert!(names(s, &format!("hashes={b}&category=tv"))
        .await
        .is_empty());
    assert!(names(s, &format!("hashes={}", "9".repeat(40)))
        .await
        .is_empty());
}

/// A tag is matched whole, and a filter word keeps the torrents whose state
/// it names: the stopped one is the paused one and the rest are running.
#[tokio::test]
async fn tags_and_state_filters_narrow_the_listing() {
    let l = library("list-filters", true);
    let s = &l.s;
    assert_eq!(names(s, "tag=y&sort=name").await, ["la", "lb"]);
    assert_eq!(names(s, "tag=x").await, ["la"]);
    assert!(names(s, "tag=xy").await.is_empty(), "whole tags only");
    assert_eq!(names(s, "filter=paused").await, ["lb"]);
    assert_eq!(names(s, "filter=stopped").await, ["lb"]);
    assert_eq!(
        names(s, "filter=running&sort=name").await,
        ["la", "lc", "ld"]
    );
    assert_eq!(
        names(s, "filter=seeding&sort=name").await,
        ["la", "lc", "ld"],
        "the one that is stopped is not seeding"
    );
    assert_eq!(
        names(s, "filter=completed&sort=name").await,
        ["la", "lb", "lc", "ld"],
        "but it is completed"
    );
    assert!(names(s, "filter=downloading").await.is_empty());
    assert!(names(s, "filter=errored").await.is_empty());
    assert_eq!(names(s, "filter=paused&tag=y").await, ["lb"]);
    assert!(names(s, "filter=paused&tag=x").await.is_empty());
}

/// The page: `offset` skips, `limit` cuts, past the end is an empty page and
/// not the whole list again, and an empty list is `[]`.
#[tokio::test]
async fn the_listing_is_paged_the_way_qbittorrent_pages_it() {
    let l = library("list-page", true);
    let s = &l.s;
    assert_eq!(names(s, "sort=name&limit=2").await, ["la", "lb"]);
    assert_eq!(names(s, "sort=name&limit=2&offset=1").await, ["lb", "lc"]);
    assert_eq!(names(s, "sort=name&offset=3").await, ["ld"]);
    assert!(names(s, "sort=name&offset=4").await.is_empty());
    assert!(names(s, "sort=name&offset=40").await.is_empty());
    assert_eq!(
        names(s, "sort=name&limit=40").await,
        ["la", "lb", "lc", "ld"]
    );
    assert_eq!(names(s, "sort=name&reverse=true&limit=1").await, ["ld"]);
}

/// A running torrent whose data is not there is the errored one, and a
/// client that asks for errors gets it and nothing else.
#[tokio::test]
async fn a_running_torrent_with_no_data_is_the_errored_one() {
    let l = library("list-errored", false);
    let s = &l.s;
    assert_eq!(
        names(s, "filter=errored&sort=name").await,
        ["la", "lc", "ld"]
    );
    assert!(names(s, "filter=seeding").await.is_empty());
    assert_eq!(
        names(s, "filter=paused").await,
        ["lb"],
        "stopped, not errored"
    );
}
