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
    info.extend_from_slice(&piece);
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

/// Which engine holds the torrent of this name, if any.
fn engine_holding(s: &TestState, name: &str) -> Option<String> {
    s.engines
        .engines()
        .iter()
        .find(|e| e.manager.all().iter().any(|t| t.meta.name == name))
        .map(|e| e.id.clone())
}

/// A file that is not a torrent is refused at the door, with a reason.
#[tokio::test]
async fn adding_something_that_is_not_a_torrent_is_refused() {
    let s = st("add-junk");
    for bytes in [b"not bencode".to_vec(), Vec::new(), b"d".to_vec()] {
        let out = add_torrent_bytes(&s.state, &bytes, "", "", "", true, true, "");
        assert!(out.is_err(), "junk must be refused: {out:?}");
    }
}

/// ⭐ An added torrent lands in the engine the CATEGORY names, and comes
/// back with the hash it was stored under -- the caller polls on it.
#[tokio::test]
async fn an_added_torrent_reports_the_hash_it_was_stored_under() {
    let s = st("add-ok");
    // ⚠️ The tuple is (info hash, torrent NAME) -- not the engine. Reading
    // the second field as an engine id silently compares a name to "race".
    let (hash, name) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "",
    )
    .expect("a valid torrent is accepted");
    assert_eq!(hash.len(), 40, "a hex info hash: {hash}");
    assert_eq!(name, "alpha", "the torrent name comes back");

    // Where it landed is read from the catalogues, not from the reply.
    assert_eq!(engine_holding(&s, "alpha").as_deref(), Some("race"));
}

/// ⚠️⚠️ An UNKNOWN category routes to RACE. That is the documented
/// behaviour and the trap that sent a whole bench import to the wrong
/// engine -- pinned so it cannot change by accident.
#[tokio::test]
async fn an_unknown_category_routes_to_race() {
    let s = st("add-unknowncat");
    add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "no-such-category",
        "/tmp",
        "",
        true,
        true,
        "",
    )
    .expect("accepted");
    assert_eq!(
        engine_holding(&s, "alpha").as_deref(),
        Some("race"),
        "an unknown category is a race torrent"
    );
}

/// ⭐ An explicit engine wins over the category's mode: it is the only way
/// to reach an engine that is neither race nor hoard.
#[tokio::test]
async fn an_explicit_engine_overrides_the_category() {
    let s = st("add-override");
    add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "hoard",
    )
    .expect("accepted");
    assert_eq!(engine_holding(&s, "alpha").as_deref(), Some("hoard"));
}

/// An override naming no engine of this node must not place the torrent
/// nowhere: it falls back rather than being obeyed.
#[tokio::test]
async fn an_override_naming_no_engine_falls_back() {
    let s = st("add-badoverride");
    add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "no-such-engine",
    )
    .expect("accepted");
    let landed = engine_holding(&s, "alpha");
    assert!(
        landed.as_deref() == Some("race") || landed.as_deref() == Some("hoard"),
        "a bogus override falls back to a real engine, got {landed:?}"
    );
}

/// The same torrent added twice to the same engine is refused rather than
/// duplicated.
#[tokio::test]
async fn adding_the_same_torrent_twice_is_refused() {
    let s = st("add-dup");
    add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("first add");
    let second = add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    );
    assert!(second.is_err(), "the duplicate is refused: {second:?}");
}

/// Tags given at add time are carried, not dropped on the floor.
#[tokio::test]
async fn tags_given_at_add_time_are_kept() {
    let s = st("add-tags");
    let (hash, _engine) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "fr,anime",
        true,
        true,
        "race",
    )
    .expect("accepted");
    let store = s.store.lock().unwrap();
    let mut tags = store.tags_of(&hash);
    tags.sort();
    assert_eq!(
        tags,
        vec!["anime".to_string(), "fr".to_string()],
        "got {tags:?}"
    );
}

/// ⭐ Seed mode is taken at its word: the torrent is not rechecked, which
/// is exactly the work the operator asked to skip.
#[test]
fn the_recheck_decision_follows_seed_mode_and_what_is_on_disk() {
    assert!(!add_recheck_wanted(true, true));
    assert!(add_recheck_wanted(false, true));
    assert!(!add_recheck_wanted(false, false));
}

/// ⚠ Admission is against PROJECTED free space, not free space. Ten races
/// arriving in thirty seconds each fit in what is free at the moment they
/// are looked at, and together they fill the disk.
///
/// Off unless `add_block_enabled`: on a default config nothing is refused.
#[tokio::test]
async fn race_admission_is_off_unless_the_operator_turned_it_on() {
    let s = st("admission-off");
    let engines = s.engines.engines();
    let race = engines.iter().find(|e| e.id == "race").expect("race");
    let out = race_admission(&s.state, race, 1 << 40, "/tmp");
    assert!(out.is_ok(), "a default config admits everything: {out:?}");
}

/// Linking an existing copy is an optimisation, not a requirement: when
/// there is nothing to link to it must answer None rather than fail the
/// add.
#[tokio::test]
async fn linking_finds_nothing_on_an_empty_library() {
    let s = st("link-empty");
    let cfg = s.cfg();
    let out = try_link_existing(
        &s.state,
        &torrent_bytes("alpha"),
        &"0".repeat(40),
        "/tmp",
        &cfg,
    );
    assert!(out.is_none(), "nothing to link to: {out:?}");
}

/// The qBittorrent shim is the *arr stack's door. Its version endpoints
/// must answer something a client will accept, not an empty body.
#[tokio::test]
async fn the_qbit_shim_reports_a_version_an_arr_client_accepts() {
    let s = st("qbit-version");
    let resp = super::qbit_version(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert!(resp.status().is_success());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(
        !bytes.is_empty(),
        "a client parses this to decide what it can call"
    );

    let api =
        super::qbit_webapi_version(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert!(api.status().is_success());
    let bytes = axum::body::to_bytes(api.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(!bytes.is_empty());
}

/// The shim's preferences are what an *arr reads to learn the save paths.
/// It must be an object, not a list or a bare string.
#[tokio::test]
async fn the_qbit_preferences_are_an_object() {
    let s = st("qbit-prefs");
    let body = body_json(
        super::qbit_preferences(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    assert!(body.is_object(), "got {body}");
}

/// `torrents/info` is the listing an *arr polls. On an empty node it is an
/// empty ARRAY -- a null there makes the *arr log a parse error every tick.
#[tokio::test]
async fn the_qbit_listing_is_an_array_even_when_empty() {
    let s = st("qbit-info");
    let body = body_json(
        super::qbit_torrents_info(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            String::new(),
        )
        .await,
    )
    .await;
    assert!(body.is_array(), "got {body}");
    assert_eq!(body.as_array().map(|a| a.len()), Some(0));
}

/// A torrent added through the native path shows up in the shim listing:
/// the two views must not disagree about what the node holds.
#[tokio::test]
async fn a_torrent_added_natively_appears_in_the_qbit_listing() {
    let s = st("qbit-sees");
    add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let body = body_json(
        super::qbit_torrents_info(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            String::new(),
        )
        .await,
    )
    .await;
    let rows = body.as_array().expect("an array");
    assert_eq!(rows.len(), 1, "got {body}");
    assert_eq!(rows[0]["name"], serde_json::json!("alpha"));
}
