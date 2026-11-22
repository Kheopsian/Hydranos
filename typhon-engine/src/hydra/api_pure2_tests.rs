use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn row(fields: serde_json::Value) -> serde_json::Value {
    fields
}

/// ⭐ A seeding torrent sorts as complete whatever its stored progress.
/// A library restored from resume can carry a progress of 0 on a torrent
/// that is fully seeding, and sorting on the stored number alone puts the
/// complete ones at the bottom.
#[test]
fn a_seeding_torrent_sorts_as_complete_whatever_its_stored_progress() {
    let seeding = row(serde_json::json!({"state": "seeding", "progress": 0.0}));
    let half = row(serde_json::json!({"state": "downloading", "progress": 0.5}));
    assert_eq!(row_sort_key(&seeding, "progress").1, 1.0);
    assert_eq!(row_sort_key(&half, "progress").1, 0.5);
}

#[test]
fn text_sorts_fold_case_so_the_order_is_not_ascii_order() {
    let a = row(serde_json::json!({"name": "Zebra"}));
    let b = row(serde_json::json!({"name": "apple"}));
    assert_eq!(row_sort_key(&a, "name").0, "zebra");
    assert_eq!(row_sort_key(&b, "name").0, "apple");
    assert!(row_sort_key(&b, "name").0 < row_sort_key(&a, "name").0);
}

#[test]
fn a_numeric_column_sorts_on_its_own_number() {
    let r = row(serde_json::json!({"ratio": 2.5, "added_time": 10.0}));
    assert_eq!(row_sort_key(&r, "ratio").1, 2.5);
}

/// An unknown sort is not an error and not an arbitrary order: it falls
/// back to when the torrent was added.
#[test]
fn an_unknown_sort_falls_back_to_the_added_time() {
    let r = row(serde_json::json!({"added_time": 42.0, "ratio": 9.0}));
    assert_eq!(row_sort_key(&r, "no_such_column").1, 42.0);
}

#[test]
fn a_missing_field_sorts_as_empty_or_zero_rather_than_panicking() {
    let r = row(serde_json::json!({}));
    assert_eq!(row_sort_key(&r, "name").0, "");
    assert_eq!(row_sort_key(&r, "ratio").1, 0.0);
    assert_eq!(row_sort_key(&r, "progress").1, 0.0);
}

fn job(kind: &str, params: &str, total: i64, done: i64, error: &str) -> crate::store::Job {
    crate::store::Job {
        id: "j1".into(),
        kind: kind.into(),
        state: "running".into(),
        info_hash: String::new(),
        params: params.into(),
        progress_bytes: done,
        total_bytes: total,
        error: error.into(),
        created_at: 1,
        updated_at: 2,
    }
}

/// ⭐ A corrupt `params` row must not make the WHOLE listing unparseable
/// for the caller: it is dropped, exactly as 3.x checks json.Valid first.
#[test]
fn a_job_with_unparseable_params_still_lists_without_them() {
    let v = job_view(&job("move", "{not json", 0, 0, ""));
    assert_eq!(v["id"], serde_json::json!("j1"));
    assert!(
        v.get("params").is_none(),
        "the corrupt field is dropped, not embedded"
    );
}

#[test]
fn a_jobs_params_are_embedded_as_json_not_as_a_string() {
    let v = job_view(&job("move", r#"{"to":"/data"}"#, 0, 0, ""));
    assert_eq!(v["params"]["to"], serde_json::json!("/data"));
}

/// An absent info_hash is ABSENT, not the empty string: the Go side marks
/// it omitempty and clients test for presence.
#[test]
fn empty_optional_fields_are_omitted_rather_than_sent_empty() {
    let v = job_view(&job("scan", "", 0, 0, ""));
    assert!(v.get("info_hash").is_none());
    assert!(v.get("error").is_none());
    assert!(v.get("params").is_none());
}

#[test]
fn an_error_is_reported_when_there_is_one() {
    let v = job_view(&job("move", "", 0, 0, "disk full"));
    assert_eq!(v["error"], serde_json::json!("disk full"));
}

/// A job of unknown size is 0 %, not a division by zero.
#[test]
fn a_job_with_no_total_reports_zero_percent_not_nan() {
    let v = job_view(&job("scan", "", 0, 500, ""));
    assert_eq!(v["percent"], serde_json::json!(0.0));
    assert!(!v["percent"].as_f64().unwrap().is_nan());
}

#[test]
fn job_progress_is_a_percentage_of_the_total() {
    let v = job_view(&job("move", "", 200, 50, ""));
    assert_eq!(v["percent"].as_f64().unwrap(), 25.0);
}

fn page(rows: Vec<serde_json::Value>, filtered: i64) -> serde_json::Value {
    serde_json::json!({"rows": rows, "filtered": filtered})
}

fn err_row(msg: &str) -> serde_json::Value {
    serde_json::json!({"tracker_error_msg": msg})
}

/// With neither an include nor an exclude list there is nothing to
/// enforce, and the pages must come back untouched rather than emptied.
#[test]
fn no_error_filter_leaves_every_row_in_place() {
    let mut pages = vec![page(vec![err_row("whatever")], 1)];
    enforce_error_class(&mut pages, &[], &[], 100);
    assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
}

/// Excluding a class drops exactly the rows of that class.
#[test]
fn excluding_a_class_drops_only_that_class() {
    let clean = err_row("");
    let unregistered = err_row("unregistered torrent");
    let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();

    let mut pages = vec![page(vec![clean.clone(), unregistered.clone()], 2)];
    enforce_error_class(&mut pages, &[], std::slice::from_ref(&class_of_unreg), 100);
    let rows = pages[0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the excluded class is gone");
    assert_eq!(rows[0]["tracker_error_msg"], serde_json::json!(""));
}

/// Including a class keeps only it.
#[test]
fn including_a_class_keeps_only_that_class() {
    let clean = err_row("");
    let unregistered = err_row("unregistered torrent");
    let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();

    let mut pages = vec![page(vec![clean, unregistered], 2)];
    enforce_error_class(&mut pages, &[class_of_unreg], &[], 100);
    assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
    assert_eq!(
        pages[0]["rows"][0]["tracker_error_msg"],
        serde_json::json!("unregistered torrent")
    );
}

/// The filtered count must follow the rows it dropped, or the UI shows a
/// total that does not match what it is displaying.
#[test]
fn the_filtered_count_follows_the_rows_that_were_dropped() {
    let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();
    let mut pages = vec![page(vec![err_row(""), err_row("unregistered torrent")], 2)];
    enforce_error_class(&mut pages, &[], &[class_of_unreg], 100);
    assert_eq!(pages[0]["filtered"], serde_json::json!(1));
}

/// The count is never taken below zero, whatever the page claimed.
#[test]
fn the_filtered_count_never_goes_negative() {
    let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();
    let mut pages = vec![page(vec![err_row("unregistered torrent")], 0)];
    enforce_error_class(&mut pages, &[], &[class_of_unreg], 100);
    assert!(pages[0]["filtered"].as_i64().unwrap() >= 0);
}

/// A page that is not an object, or carries no rows, is skipped rather
/// than panicking: these come from another node over the wire.
#[test]
fn a_malformed_page_from_another_node_is_skipped_not_fatal() {
    let mut pages = vec![
        serde_json::json!("not an object"),
        serde_json::json!({"no_rows_here": true}),
        serde_json::json!({"rows": "not an array"}),
    ];
    enforce_error_class(&mut pages, &[], &["whatever".to_string()], 100);
}

/// ⭐ A category carries a MODE ("hoard" or "race"), which names a
/// behaviour, not one of the engines a node hosts. An explicit engine is
/// the only way to reach an engine that is neither -- without it a torrent
/// could never be placed in `vpn1`, whatever the config said.
#[tokio::test]
async fn an_unknown_category_lands_in_race_by_default() {
    let s = state_from("placement", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (engine, path) = placement(&s.state, "no-such-category", "");
    assert_eq!(engine, "race");
    assert!(path.is_empty());
}

/// An engine override that names no engine of this node must NOT be
/// honoured: it would place the torrent nowhere.
#[tokio::test]
async fn an_override_naming_no_engine_of_this_node_is_ignored() {
    let s = state_from("placement-bad", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (engine, _) = placement(&s.state, "no-such-category", "no-such-engine");
    assert_eq!(
        engine, "race",
        "the bogus override falls back rather than being obeyed"
    );
}
