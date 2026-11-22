use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn row(name: &str, hash: &str, ratio: f64) -> serde_json::Value {
    serde_json::json!({"name": name, "info_hash": hash, "ratio": ratio, "added_time": 1.0})
}

fn page(total: i64, filtered: i64, rows: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({"total": total, "filtered": filtered, "rows": rows})
}

/// The fleet totals are the SUM of the nodes', not one node's.
#[test]
fn the_totals_add_up_across_the_nodes() {
    let merged = merge_pages(
        vec![
            page(100, 10, vec![row("a", "aa", 1.0)]),
            page(50, 5, vec![row("b", "bb", 2.0)]),
        ],
        "name",
        true,
        0,
        100,
    );
    assert_eq!(merged["total"], serde_json::json!(150));
    assert_eq!(merged["filtered"], serde_json::json!(15));
    assert_eq!(merged["rows"].as_array().unwrap().len(), 2);
}

/// With no node at all the answer is still a page, with honest zeroes.
#[test]
fn merging_nothing_is_an_empty_page_not_a_null() {
    let merged = merge_pages(vec![], "name", true, 0, 100);
    assert_eq!(merged["total"], serde_json::json!(0));
    assert_eq!(merged["filtered"], serde_json::json!(0));
    assert!(merged["rows"].as_array().unwrap().is_empty());
}

/// Rows from different nodes interleave by the sort key, rather than
/// staying grouped per node.
#[test]
fn rows_from_two_nodes_interleave_by_the_sort_key() {
    let merged = merge_pages(
        vec![
            page(
                2,
                2,
                vec![row("alpha", "a1", 1.0), row("charlie", "c1", 3.0)],
            ),
            page(1, 1, vec![row("bravo", "b1", 2.0)]),
        ],
        "name",
        true,
        0,
        100,
    );
    let names: Vec<&str> = merged["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
}

#[test]
fn the_direction_is_honoured_when_merging() {
    let pages = || {
        vec![
            page(1, 1, vec![row("alpha", "a1", 1.0)]),
            page(1, 1, vec![row("bravo", "b1", 2.0)]),
        ]
    };
    let up = merge_pages(pages(), "name", true, 0, 100);
    let down = merge_pages(pages(), "name", false, 0, 100);
    assert_eq!(up["rows"][0]["name"], serde_json::json!("alpha"));
    assert_eq!(down["rows"][0]["name"], serde_json::json!("bravo"));
}

/// ⭐ The tie-break on the hex hash is what keeps paging stable: without
/// it, two rows that compare equal can swap between requests and the same
/// torrent shows up on two pages -- or on none.
#[test]
fn rows_that_compare_equal_are_still_ordered_the_same_way_every_time() {
    let mk = || {
        vec![
            page(1, 1, vec![row("same", "ffff", 1.0)]),
            page(1, 1, vec![row("same", "0000", 1.0)]),
            page(1, 1, vec![row("same", "8888", 1.0)]),
        ]
    };
    let first = merge_pages(mk(), "name", true, 0, 100);
    let second = merge_pages(mk(), "name", true, 0, 100);
    assert_eq!(
        first["rows"], second["rows"],
        "the order must be deterministic"
    );

    let hashes: Vec<&str> = first["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["info_hash"].as_str())
        .collect();
    assert_eq!(
        hashes,
        vec!["0000", "8888", "ffff"],
        "ordered by hash on a tie"
    );
}

/// The window is applied AFTER the merge, or each node would contribute
/// its own first page and the fleet would show the same offset twice.
#[test]
fn the_window_applies_to_the_merged_list_not_to_each_node() {
    let merged = merge_pages(
        vec![
            page(
                2,
                2,
                vec![row("alpha", "a1", 1.0), row("charlie", "c1", 3.0)],
            ),
            page(1, 1, vec![row("bravo", "b1", 2.0)]),
        ],
        "name",
        true,
        1,
        1,
    );
    let rows = merged["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["name"],
        serde_json::json!("bravo"),
        "the second row overall"
    );
    assert_eq!(merged["offset"], serde_json::json!(1));
    assert_eq!(merged["limit"], serde_json::json!(1));
}

/// An offset past the merged end is empty, and the counters still describe
/// the fleet.
#[test]
fn an_offset_past_the_merged_end_is_empty_but_still_counts() {
    let merged = merge_pages(
        vec![page(9, 9, vec![row("a", "a1", 1.0)])],
        "name",
        true,
        50,
        10,
    );
    assert!(merged["rows"].as_array().unwrap().is_empty());
    assert_eq!(merged["total"], serde_json::json!(9));
}

/// A node that answered with no `rows` key at all, or a malformed page,
/// must not take the fleet listing down with it.
#[test]
fn a_node_that_answered_badly_does_not_break_the_merge() {
    let merged = merge_pages(
        vec![
            serde_json::json!({"total": 5}),
            serde_json::json!("not an object"),
            page(1, 1, vec![row("alpha", "a1", 1.0)]),
        ],
        "name",
        true,
        0,
        100,
    );
    assert_eq!(
        merged["total"],
        serde_json::json!(6),
        "the counts it did give still count"
    );
    assert_eq!(merged["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn a_numeric_sort_merges_on_the_number() {
    let merged = merge_pages(
        vec![
            page(1, 1, vec![row("a", "a1", 10.0)]),
            page(1, 1, vec![row("b", "b1", 2.0)]),
        ],
        "ratio",
        true,
        0,
        100,
    );
    let ratios: Vec<f64> = merged["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["ratio"].as_f64())
        .collect();
    assert_eq!(
        ratios,
        vec![2.0, 10.0],
        "2 before 10, not \"10\" before \"2\""
    );
}

/// Session and day counters are derived from marks, never stored twice.
/// On a fresh state everything is zero rather than absent.
#[tokio::test]
async fn a_fresh_state_reports_zeroed_counters_rather_than_nothing() {
    let s = state_from("counters", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (session, day, life) = session_and_day(&s.state);
    for (name, (ul, dl)) in [("session", session), ("day", day), ("life", life)] {
        assert!(ul >= 0, "{name} upload is not negative");
        assert!(dl >= 0, "{name} download is not negative");
    }
}

/// An engine this node does not host has no session counters -- zero, not
/// a panic on an index that is not there.
#[tokio::test]
async fn an_unknown_engine_has_zeroed_session_counters() {
    let s = state_from(
        "counters-unknown",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    assert_eq!(engine_session(&s.state, "no-such-engine"), (0, 0));
}

/// Both engines of a fresh node answer, and answer zero.
#[tokio::test]
async fn both_local_engines_report_their_own_counters() {
    let s = state_from("counters-both", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    assert_eq!(engine_session(&s.state, "race"), (0, 0));
    assert_eq!(engine_session(&s.state, "hoard"), (0, 0));
}

/// `engine_rows` is what the qBittorrent-shaped listing serves; on an
/// empty engine it is an empty list, and on an unknown one too.
#[tokio::test]
async fn engine_rows_are_empty_for_an_empty_or_unknown_engine() {
    let s = state_from("rows", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    assert!(engine_rows(&s.state, "race").is_empty());
    assert!(engine_rows(&s.state, "no-such-engine").is_empty());
}

#[tokio::test]
async fn a_fresh_node_has_announced_nothing_and_sees_no_leechers() {
    let s = state_from("announced", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    assert_eq!(announced_count(&s.state, "race"), 0);
    assert_eq!(swarm_leechers_total(&s.state), 0);
}

/// The disk figures come back for a real path and degrade to zeroes for
/// one that is not there, rather than refusing the whole panel.
///
/// ⚠️ The tuple is `(total, used, pct)` -- total FIRST. Reading it as
/// `(used, total)` silently swaps the two on a panel where both are
/// plausible numbers.
///
/// ⚠️⚠️ This is the THIRD implementation of "how full is this disk", and
/// it is the only one that computes `used = total - available`, which
/// counts the filesystem's reserved blocks as used. `volumes::usage` and
/// `workers::disk_usage` both take `f_blocks - f_bfree` instead, and both
/// carry a comment saying why the subtraction here is wrong. The panel and
/// the drain therefore do not report the same fullness for the same disk.
#[test]
fn disk_usage_answers_for_a_real_path_and_zeroes_for_a_missing_one() {
    let tmp = std::env::temp_dir();
    let (total, used, pct) = disk_usage(tmp.to_str().unwrap());
    assert!(total > 0, "a mounted filesystem has a size");
    assert!(used <= total);
    assert!((0.0..=100.0).contains(&pct), "got {pct}");

    let missing = tmp.join("typhon-no-such-dir-3f8a");
    let (t2, u2, p2) = disk_usage(missing.to_str().unwrap());
    assert_eq!(
        (t2, u2, p2),
        (0, 0, 0.0),
        "a path we cannot stat is zeroes, not a refusal"
    );
}

/// The reserved-block gap, stated as a fact rather than as a preference:
/// this function's `used` is at least what the filesystem counts as taken,
/// and on a filesystem with reserved blocks it is strictly more.
#[test]
fn the_panel_counts_reserved_blocks_as_used_where_the_drain_does_not() {
    // One reading for both figures: two stats would differ by whatever
    // other tests write between them.
    let (drain_used, total, free) =
        crate::volumes::usage(&std::env::temp_dir()).expect("the temp dir is on a filesystem");
    let (_, panel_used, _) = panel_figures(total, free);
    assert!(
        panel_used >= drain_used as i64,
        "panel {panel_used} vs drain {drain_used}: the panel adds the reserved blocks"
    );
}
