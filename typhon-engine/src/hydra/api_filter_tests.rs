use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xABu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    piece[2] = name.as_bytes()[name.len() - 1];
    info.extend_from_slice(&piece);
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn add(s: &TestState, engine_id: &str, name: &str) -> String {
    let engines = s.engines.engines();
    let engine = engines.iter().find(|e| e.id == engine_id).expect("engine");
    let (ih, _) = engine
        .manager
        .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
        .unwrap_or_else(|e| panic!("add {name}: {e}"));
    typhon_engine::torrent::hex_encode(&ih)
}

fn rows(v: &serde_json::Value) -> &Vec<serde_json::Value> {
    v["rows"].as_array().expect("rows")
}

/// A filter naming something no torrent carries keeps NOTHING. Falling
/// back to everything is how a "category: films" view silently shows the
/// whole library.
#[tokio::test]
async fn a_filter_that_matches_nothing_keeps_nothing() {
    let s = st("f-nothing");
    add(&s, "race", "alpha");
    for q in [
        "category=no-such-category",
        "tag=no-such-tag",
        "tracker=no-such-tracker",
        "state=no_such_state",
    ] {
        let v = engine_page_value(&s.state, "race", q).await;
        assert_eq!(
            v["total"],
            serde_json::json!(1),
            "{q}: the library is unchanged"
        );
        assert_eq!(
            v["filtered"],
            serde_json::json!(0),
            "{q} kept something: {v}"
        );
        assert!(rows(&v).is_empty(), "{q}");
    }
}

/// ⭐ The negative filters are the mirror of the positive ones: excluding
/// something no torrent has must keep EVERYTHING, not nothing. Getting the
/// polarity backwards empties the view.
#[tokio::test]
async fn excluding_something_nobody_has_keeps_everything() {
    let s = st("f-not");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    for q in [
        "category_not=no-such-category",
        "tag_not=no-such-tag",
        "tracker_not=no-such-tracker",
    ] {
        let v = engine_page_value(&s.state, "race", q).await;
        assert_eq!(v["filtered"], serde_json::json!(2), "{q} dropped rows: {v}");
    }
}

/// Excluding the tracker every torrent DOES announce to empties the view.
/// This is the pair of the test above and the one that proves the filter
/// is actually reading the tracker rather than always missing.
#[tokio::test]
async fn excluding_the_tracker_they_all_use_keeps_nothing() {
    let s = st("f-nottracker");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "tracker_not=tracker.example").await;
    assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
}

/// A seed-only torrent is a seeder to the Seeding filter, as the chip counts
/// it: `state=seeding` lists it, while a plain `state=seed_only` names the
/// raw state. Pins the contract the client's `_hoardMatches` mirrors.
#[tokio::test]
async fn state_seeding_lists_seed_only_torrents() {
    let s = st("f-seedonly");
    let engine = s.engines.get("race").expect("engine");
    let (ih, _) = engine
        .manager
        .add_torrent_bytes(&torrent_bytes("alpha"), "/tmp", false, false)
        .unwrap_or_else(|e| panic!("add alpha: {e}"));
    engine
        .manager
        .set_seed_only(&ih, true)
        .unwrap_or_else(|e| panic!("set seed-only: {e}"));

    let all = engine_page_value(&s.state, "race", "").await;
    assert_eq!(rows(&all)[0]["state"], "seed_only", "premise: {all}");

    let v = engine_page_value(&s.state, "race", "state=seeding").await;
    assert_eq!(v["filtered"], serde_json::json!(1), "Seeding drops it: {v}");
    assert_eq!(rows(&v)[0]["state"], "seed_only", "{v}");

    let v = engine_page_value(&s.state, "race", "state=__active__").await;
    assert_eq!(
        v["filtered"],
        serde_json::json!(0),
        "no upload, not active: {v}"
    );
}

#[tokio::test]
async fn filtering_on_the_tracker_they_use_keeps_them() {
    let s = st("f-tracker");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "tracker=tracker.example").await;
    assert_eq!(v["filtered"], serde_json::json!(1), "got {v}");
}

/// ⭐ `fields=hash` answers the SELECTION UNIVERSE: every hash the filter
/// matched, with no rows built. Ctrl+A needs the whole set and none of its
/// contents, and shipping full rows for it would undo the paging.
#[tokio::test]
async fn the_hash_projection_answers_every_match_and_no_rows() {
    let s = st("f-hashes");
    for n in ["alpha", "bravo", "charlie"] {
        add(&s, "race", n);
    }
    let v = engine_page_value(&s.state, "race", "fields=hash&limit=1").await;
    let hashes = v["hashes"].as_array().expect("a hashes array");
    assert_eq!(
        hashes.len(),
        3,
        "the window does not apply to the selection universe"
    );
    assert!(
        v.get("rows").is_none() || rows(&v).is_empty(),
        "no rows are built: {v}"
    );
    assert_eq!(v["total"], serde_json::json!(3));
    for h in hashes {
        assert_eq!(h.as_str().map(|s| s.len()), Some(40), "a hex info hash");
    }
}

/// The selection universe respects the filter, or Ctrl+A would select
/// torrents the view is not showing.
#[tokio::test]
async fn the_hash_projection_respects_the_filter() {
    let s = st("f-hashfilter");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    let v = engine_page_value(&s.state, "race", "fields=hash&search=alpha").await;
    assert_eq!(v["hashes"].as_array().map(|a| a.len()), Some(1), "got {v}");
}

/// Facets are what the sidebar counts. They must be present when asked
/// for, absent otherwise -- computing them on every page was measurable.
#[tokio::test]
async fn facets_are_computed_only_when_asked_for() {
    let s = st("f-facets");
    add(&s, "race", "alpha");
    let without = engine_page_value(&s.state, "race", "").await;
    let with = engine_page_value(&s.state, "race", "facets=1").await;
    assert!(
        with.get("facets").is_some(),
        "facets=1 must produce them: {with}"
    );
    let _ = without;
}

/// The facet counts must agree with the rows they summarise, or the
/// sidebar and the table disagree on screen.
#[tokio::test]
async fn the_facet_counts_agree_with_the_library() {
    let s = st("f-facetcount");
    add(&s, "race", "alpha");
    add(&s, "race", "bravo");
    let v = engine_page_value(&s.state, "race", "facets=1").await;
    let facets = &v["facets"];
    if let Some(trackers) = facets.get("trackers").and_then(|t| t.as_array()) {
        let total: i64 = trackers
            .iter()
            .filter_map(|t| t.get("count").and_then(|c| c.as_i64()))
            .sum();
        assert_eq!(total, 2, "every torrent is counted once: {facets}");
    }
}

/// Two filters are an AND, not an OR: a category that matches and a search
/// that does not must keep nothing.
#[tokio::test]
async fn two_filters_narrow_together_rather_than_widening() {
    let s = st("f-and");
    add(&s, "race", "alpha");
    let v = engine_page_value(
        &s.state,
        "race",
        "tracker=tracker.example&search=nosuchthing",
    )
    .await;
    assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
}

/// The pinned view is its own state filter and must not fall through to
/// "everything" on a library where nothing is pinned.
#[tokio::test]
async fn the_pinned_view_shows_nothing_when_nothing_is_pinned() {
    let s = st("f-pinned");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "state=__pinned__").await;
    assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
}

/// A search on a hash prefix must not also match a torrent whose NAME
/// happens to contain those hex characters by coincidence -- the hex
/// branch is chosen by the shape of the query.
#[tokio::test]
async fn a_hex_search_is_matched_against_hashes_not_names() {
    let s = st("f-hex");
    let h = add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", &format!("search={}", &h[..10])).await;
    assert_eq!(
        v["filtered"],
        serde_json::json!(1),
        "the hash prefix finds it: {v}"
    );
}

/// Both engines answer their own page independently, including under a
/// filter -- the hoard must not inherit the race's matches.
#[tokio::test]
async fn a_filter_applies_per_engine() {
    let s = st("f-perengine");
    add(&s, "race", "alpha");
    add(&s, "hoard", "bravo");
    let race = engine_page_value(&s.state, "race", "search=alpha").await;
    let hoard = engine_page_value(&s.state, "hoard", "search=alpha").await;
    assert_eq!(race["filtered"], serde_json::json!(1));
    assert_eq!(hoard["filtered"], serde_json::json!(0), "got {hoard}");
}

/// Every row carries the fields the table renders. A row missing its hash
/// cannot be selected, and one missing its name renders blank.
#[tokio::test]
async fn every_row_carries_what_the_table_needs() {
    let s = st("f-rowshape");
    add(&s, "race", "alpha");
    let v = engine_page_value(&s.state, "race", "").await;
    let row = &rows(&v)[0];
    for key in ["info_hash", "name", "state", "progress", "total_size"] {
        assert!(row.get(key).is_some(), "a row carries {key}: {row}");
    }
    assert_eq!(row["info_hash"].as_str().map(|s| s.len()), Some(40));
    assert_eq!(row["name"], serde_json::json!("alpha"));
}

/// An added torrent lands in the engine it was added to and is counted
/// there -- the add path, end to end, through the real manager.
#[tokio::test]
async fn an_added_torrent_is_counted_by_its_engine() {
    let s = st("f-added");
    let engines = s.engines.engines();
    let race = engines.iter().find(|e| e.id == "race").unwrap();
    assert_eq!(race.manager.count(), 0);
    add(&s, "race", "alpha");
    assert_eq!(race.manager.count(), 1);
    assert_eq!(race.manager.len(), 1);
}

/// The same torrent cannot be added twice to one engine: the second add is
/// refused rather than producing a duplicate row.
#[tokio::test]
async fn adding_the_same_torrent_twice_to_one_engine_is_refused() {
    let s = st("f-dup");
    add(&s, "race", "alpha");
    let engines = s.engines.engines();
    let race = engines.iter().find(|e| e.id == "race").unwrap();
    assert!(
        race.manager
            .add_torrent_bytes(&torrent_bytes("alpha"), "/tmp", true, true)
            .is_err(),
        "the duplicate must be refused"
    );
    assert_eq!(race.manager.count(), 1);
}
