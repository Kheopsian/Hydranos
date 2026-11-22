use super::*;

/// 03/10/2026: eleven torrents whose folder ended in a space were "set
/// location"-ed to it, re-added at the trimmed name, and stayed in error.
#[test]
fn a_location_keeps_the_trailing_space_of_its_folder() {
    // `/data` is only absolute on Unix; Windows needs a drive.
    let root = if cfg!(windows) { "C:" } else { "" };
    let p = location_path(&format!("{root}/data/sw_1/Purity  2016 ")).unwrap();
    assert_eq!(
        p,
        std::path::PathBuf::from(format!("{root}/data/sw_1/Purity  2016 "))
    );
    assert!(p.to_string_lossy().ends_with("Purity  2016 "));
    assert_eq!(
        location_path(&format!("{root}/data/tv/")).unwrap(),
        std::path::PathBuf::from(format!("{root}/data/tv"))
    );
    assert!(location_path("   ").is_err());
    assert!(location_path(&format!(" {root}/data")).is_err());
}

/// ⭐ THE REGRESSION TEST FOR 2026-09-16.
///
/// This is the exact body the browser sent for any selection over 500 rows.
/// It parsed without complaint, `filter` was dropped on the floor, `hashes`
/// defaulted to empty -- and empty meant the whole engine. A start aimed at
/// 70k torrents started 293k.
///
/// It must now be refused. If someone re-adds a `filter` field, they have to
/// implement it: this test fails the moment it parses again.
#[test]
fn the_body_that_started_the_whole_library_is_refused() {
    let body = r#"{"action":"start","filter":{"category":"Books","search":"","tracker":"","tag":"","state":""},"exclude":[]}"#;
    let parsed = serde_json::from_str::<BulkBody>(body);
    let err = parsed
        .err()
        .expect("a filter this API never implemented must not parse");
    assert!(
        err.to_string().contains("filter"),
        "the error must NAME the field, or the caller learns nothing: {err}"
    );
}

/// The same shape without the unknown key still parses -- and still means
/// nothing, because `all` was not set. The refusal of an empty selection
/// lives in `bulk_action`; this pins the value it reads.
#[test]
fn an_empty_selection_does_not_opt_into_everything() {
    let req: BulkBody =
        serde_json::from_str(r#"{"action":"start","hashes":[],"exclude":[]}"#).unwrap();
    assert!(req.hashes.is_empty());
    assert!(!req.all, "empty must never imply the whole engine");
}

#[test]
fn everything_is_reachable_but_only_on_purpose() {
    let req: BulkBody =
        serde_json::from_str(r#"{"action":"stop","hashes":[],"all":true}"#).unwrap();
    assert!(req.all);
}

#[test]
fn a_normal_selection_still_parses() {
    let req: BulkBody =
        serde_json::from_str(r#"{"action":"stop","hashes":["aa","BB"],"exclude":["cc"]}"#).unwrap();
    assert_eq!(req.hashes, vec!["aa", "BB"]);
    assert_eq!(req.exclude, vec!["cc"]);
    assert!(!req.all);
}
