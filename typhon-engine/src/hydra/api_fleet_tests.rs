use super::*;

/// The host extraction has to survive every shape a URL arrives in, since
/// a wrong answer here either blocks a legitimate node or lets a loopback
/// one through.
#[test]
fn loopback_is_recognised_whatever_the_url_looks_like() {
    let host_of = |url: &str| -> String {
        url.split("//")
            .nth(1)
            .unwrap_or(url)
            .split('/')
            .next()
            .unwrap_or_default()
            .rsplit(':')
            .next_back()
            .unwrap_or_default()
            .trim_matches(|c| c == '[' || c == ']')
            .to_string()
    };
    assert_eq!(host_of("http://127.0.0.1:8499"), "127.0.0.1");
    assert_eq!(host_of("http://localhost:8199/"), "localhost");
    assert_eq!(host_of("http://192.168.99.200:8499"), "192.168.99.200");
    assert_eq!(
        host_of("https://seedbox.example.net:8199"),
        "seedbox.example.net"
    );
    // A LAN address must NOT be mistaken for loopback.
    assert!(!host_of("http://192.168.99.200:8499").starts_with("127."));
}

fn page(rows: &[(&str, f64)]) -> serde_json::Value {
    serde_json::json!({
        "total": rows.len(),
        "filtered": rows.len(),
        "rows": rows.iter().map(|(h, t)| serde_json::json!({
            "info_hash": h, "added_time": t, "name": h, "state": "seeding"
        })).collect::<Vec<_>>(),
    })
}

fn hashes(v: &serde_json::Value) -> Vec<String> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["info_hash"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn two_sorted_pages_interleave_and_the_counts_add_up() {
    let a = page(&[("aa", 30.0), ("bb", 10.0)]);
    let b = page(&[("cc", 20.0), ("dd", 5.0)]);
    let out = merge_pages(vec![a, b], "added_time", false, 0, 10);
    assert_eq!(hashes(&out), vec!["aa", "cc", "bb", "dd"]);
    assert_eq!(out["total"].as_i64(), Some(4));
    assert_eq!(out["filtered"].as_i64(), Some(4));
}

/// The property that matters: paging the fleet must not duplicate a torrent
/// or lose one. Two pages of two, read back to back, must be the whole set
/// in order and each row exactly once.
#[test]
fn consecutive_windows_are_disjoint_and_complete() {
    let mk = || {
        vec![
            page(&[("aa", 30.0), ("bb", 10.0)]),
            page(&[("cc", 20.0), ("dd", 5.0)]),
        ]
    };
    let first = hashes(&merge_pages(mk(), "added_time", false, 0, 2));
    let second = hashes(&merge_pages(mk(), "added_time", false, 2, 2));
    assert_eq!(first, vec!["aa", "cc"]);
    assert_eq!(second, vec!["bb", "dd"]);
    let mut all = first.clone();
    all.extend(second);
    all.sort();
    all.dedup();
    assert_eq!(
        all.len(),
        4,
        "a row was duplicated or dropped between pages"
    );
}

/// Rows that compare equal still need a total order, or the same torrent
/// lands on two pages -- or on none -- as soon as the sort is not unique.
#[test]
fn equal_keys_are_broken_by_hash_so_the_order_is_total() {
    let a = page(&[("bb", 10.0)]);
    let b = page(&[("aa", 10.0)]);
    let out = merge_pages(vec![a, b], "added_time", true, 0, 10);
    assert_eq!(hashes(&out), vec!["aa", "bb"]);
}

fn page_with(rows: Vec<(&str, &str)>, filtered: i64) -> serde_json::Value {
    serde_json::json!({
        "total": filtered,
        "filtered": filtered,
        "rows": rows.iter().map(|(hash, msg)| serde_json::json!({
            "info_hash": hash,
            "tracker_error_msg": msg,
            "added_time": 1,
        })).collect::<Vec<_>>(),
    })
}

/// A node that does not know the filter sends everything. Without the
/// guard the page shows its passkey failures under "dead", which is the
/// bug this exists to stop: remove the retain() and this test fails.
#[test]
fn an_older_node_does_not_smuggle_in_other_classes() {
    let mut pages = vec![page_with(
        vec![
            ("a", "v4: tracker: torrent introuvable"),
            ("b", "v4: tracker: invalid passkey"),
            ("c", "v4: <url>): operation timed out"),
        ],
        3,
    )];
    enforce_error_class(&mut pages, &["dead".to_string()], &[], 10);
    let rows = pages[0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["info_hash"], "a");
    // The whole catalogue fitted in the window, so the count is exact.
    assert_eq!(pages[0]["filtered"], 1);
}

/// A node that filled its window is holding rows nobody has seen, so its
/// count cannot be corrected from the visible ones.
#[test]
fn a_full_window_keeps_its_own_count() {
    let mut pages = vec![page_with(
        vec![
            ("a", "v4: tracker: torrent introuvable"),
            ("b", "v4: tracker: invalid passkey"),
        ],
        900,
    )];
    enforce_error_class(&mut pages, &["dead".to_string()], &[], 2);
    assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(pages[0]["filtered"], 900);
}

/// No filter asked for, nothing touched -- including the torrents with no
/// error at all, which belong to no class and must not be swept away.
#[test]
fn no_filter_leaves_every_row_alone() {
    let mut pages = vec![page_with(
        vec![("a", ""), ("b", "v4: tracker: invalid passkey")],
        2,
    )];
    enforce_error_class(&mut pages, &[], &[], 10);
    assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 2);
}

/// Excluding a class keeps the torrents that are in NO class.
#[test]
fn excluding_a_class_keeps_the_healthy_ones() {
    let mut pages = vec![page_with(
        vec![("a", ""), ("b", "v4: tracker: invalid passkey")],
        2,
    )];
    enforce_error_class(&mut pages, &[], &["auth".to_string()], 10);
    let rows = pages[0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["info_hash"], "a");
}

/// Facets from several nodes add up instead of vanishing. Enrolling one
/// node used to blank every number on the page.
#[test]
fn facets_of_two_nodes_are_summed() {
    let a = serde_json::json!({
        "total": 1, "filtered": 1, "rows": [],
        "facets": {"all": 10, "category": {"movies": 4}, "error_class": {"dead": 2}},
    });
    let b = serde_json::json!({
        "total": 1, "filtered": 1, "rows": [],
        "facets": {"all": 5, "category": {"movies": 1, "series": 3}, "error_class": {"auth": 1}},
    });
    let out = merge_pages(vec![a, b], "added_time", false, 0, 10);
    assert_eq!(out["facets"]["all"], 15);
    assert_eq!(out["facets"]["category"]["movies"], 5);
    // A category only one node knows is still the answer for that node.
    assert_eq!(out["facets"]["category"]["series"], 3);
    assert_eq!(out["facets"]["error_class"]["dead"], 2);
    assert_eq!(out["facets"]["error_class"]["auth"], 1);
}

/// Null when nobody counted: that is a different claim from zero, and the
/// UI reads the difference to decide whether to draw chips at all.
#[test]
fn facets_stay_null_when_no_node_counted() {
    let a = serde_json::json!({"total": 1, "filtered": 1, "rows": [], "facets": null});
    let out = merge_pages(vec![a], "added_time", false, 0, 10);
    assert!(out["facets"].is_null());
}

#[test]
fn a_seeding_row_sorts_as_complete_whatever_its_progress() {
    let seeding = serde_json::json!({"state": "seeding", "progress": 0.2, "info_hash": "aa"});
    let leeching =
        serde_json::json!({"state": "downloading", "progress": 0.9, "info_hash": "bb"});
    assert_eq!(row_sort_key(&seeding, "progress").1, 1.0);
    assert_eq!(row_sort_key(&leeching, "progress").1, 0.9);
}

#[test]
fn the_window_is_replaced_and_every_other_filter_survives() {
    let q = with_window("search=demo&offset=500&limit=500&sort=name", 0, 1000);
    assert!(q.contains("search=demo"), "{q}");
    assert!(q.contains("sort=name"), "{q}");
    assert!(q.contains("offset=0"), "{q}");
    assert!(q.contains("limit=1000"), "{q}");
    assert!(!q.contains("offset=500"), "the old window must go: {q}");
}
