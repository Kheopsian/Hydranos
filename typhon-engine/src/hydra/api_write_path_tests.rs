use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

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

fn with_torrent(tag: &str) -> (TestState, String) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (hash, _name) = add_torrent_bytes(
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
    (s, hash)
}

/// Make every write to these tables fail the way a dead mount does.
fn refuse_writes(s: &TestState, tables: &[&str]) {
    let mut sql = String::new();
    for table in tables {
        for ev in ["INSERT", "UPDATE", "DELETE"] {
            sql += &format!(
                "CREATE TRIGGER refuse_{table}_{ev} BEFORE {ev} ON {table} \
                 BEGIN SELECT RAISE(ABORT, 'database disk image is malformed'); END;"
            );
        }
    }
    s.state.store.lock().unwrap().execute_batch_for_tests(&sql);
}

/// A passkey override the announce path could not place in any URL shows
/// on the host's row, so the operator is not left believing it is live.
#[tokio::test]
async fn the_tracker_row_flags_a_passkey_that_could_not_be_placed() {
    let s = state_from(
        "row-unplaced",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"row-unplaced.example.invalid\" = \"0123abcd\"\n\
             \"row-placed.example.invalid\" = \"0123abcd\"\n"
        ),
    );
    let mut p = crate::announce::policy::Policy::default();
    p.passkeys
        .insert("row-unplaced.example.invalid".into(), "0123abcd".into());
    crate::announce::policy::keyed_url(
        &p,
        "https://row-unplaced.example.invalid/tracker/announce",
    );
    let body = body_json(
        super::get_trackers(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    let row = |h: &str| {
        body.as_array()
            .and_then(|a| a.iter().find(|r| r["host"] == h))
            .unwrap_or_else(|| panic!("no row {h}: {body}"))
            .clone()
    };
    assert_eq!(
        row("row-unplaced.example.invalid")["passkey_unplaced"],
        true,
        "{body}"
    );
    assert_eq!(
        row("row-placed.example.invalid")["passkey_unplaced"],
        false,
        "{body}"
    );
}

/// The writes that come AFTER the engine has acted -- a copy, a move, a
/// removal -- and the ones on the side (counters, a pause, the references
/// a rename follows): when the store refuses them the answer is not 2xx,
/// and the engine is not left holding what the store does not know.
#[tokio::test]
async fn a_store_that_refuses_after_the_engine_acted_is_not_acknowledged() {
    let mut seen: Vec<(&str, StatusCode)> = Vec::new();
    let mut left_behind: Vec<&str> = Vec::new();

    // Copy to a second engine: the engine adds it, the store insert fails.
    let (s, hash) = with_torrent("refuse-copy");
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    refuse_writes(&s, &["torrents"]);
    let r = super::post_torrent_copy(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"engine":"hoard"}"#.to_string(),
    )
    .await;
    seen.push(("POST /api/torrents/:hash/copy", r.status()));
    let hoard = s.state.engines.get("hoard").unwrap();
    if hoard.manager.get(&ih).is_some() {
        left_behind.push("copy: the engine kept a copy the store does not know");
    }

    // Move to another engine: both engines moved, the store update fails.
    let (s, hash) = with_torrent("refuse-move");
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    refuse_writes(&s, &["torrents"]);
    let r = super::post_torrent_engine(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"engine":"hoard"}"#.to_string(),
    )
    .await;
    seen.push(("POST /api/torrents/:hash/engine", r.status()));
    if s.state
        .engines
        .get("race")
        .unwrap()
        .manager
        .get(&ih)
        .is_none()
        || s.state
            .engines
            .get("hoard")
            .unwrap()
            .manager
            .get(&ih)
            .is_some()
    {
        left_behind.push("move: the engines moved, the store did not");
    }

    // Removal: the engine let go, the store delete fails.
    let (s, hash) = with_torrent("refuse-delete");
    refuse_writes(&s, &["torrents"]);
    let r = super::delete_torrent(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    seen.push(("DELETE /api/torrents/:hash", r.status()));
    let (s, hash) = with_torrent("refuse-qbit-delete");
    refuse_writes(&s, &["torrents"]);
    let mut form = super::Fields::new();
    form.insert("hashes".into(), hash.clone());
    let r = super::qbit_delete(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        axum::extract::Form(form),
    )
    .await;
    seen.push(("qbit delete", r.status()));

    // Pause of one race torrent.
    let (s, hash) = with_torrent("refuse-pause");
    refuse_writes(&s, &["torrents"]);
    let r = super::race_pause_one(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    seen.push(("POST /api/race/torrents/:hash/pause", r.status()));

    // The baseline counters.
    let (s, _) = with_torrent("refuse-baseline");
    refuse_writes(&s, &["counters"]);
    let r = super::post_baseline(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"total_uploaded":5,"total_downloaded":6}"#.to_string(),
    )
    .await;
    seen.push(("POST /api/stats/baseline", r.status()));

    // A rename that cannot follow a watched folder.
    let s = state_from("refuse-rename", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    super::category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;
    s.state
        .store
        .lock()
        .unwrap()
        .put_setting("watch_folders", r#"[{"path":"/watch","category":"films"}]"#)
        .unwrap();
    refuse_writes(&s, &["app_settings"]);
    let r = super::category_update(
        State(s.state.clone()),
        axum::extract::Path("films".to_string()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"movies","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;
    seen.push(("PUT /api/categories/:name (rename)", r.status()));

    let acked: Vec<String> = seen
        .iter()
        .filter(|(_, c)| !c.is_server_error())
        .map(|(n, c)| format!("{n} -> {c}"))
        .collect();
    assert!(
        acked.is_empty(),
        "not refused although the store refused the write: {acked:?}"
    );
    assert!(
        left_behind.is_empty(),
        "engines and store disagree: {left_behind:?}"
    );
}

/// ⭐⭐ The settings screen edits keys this binary does not model, so the
/// route WRITES THE FILE rather than serialising the typed Config. A round
/// trip through the struct would silently drop every key it does not know.
#[tokio::test]
async fn saving_settings_keeps_a_key_the_binary_does_not_model() {
    let s = state_from(
        "set-unmodelled",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let doc =
        format!("[daemon]\napi_key = \"{KEY}\"\n\n[a_section_nobody_models]\nkept = \"yes\"\n");
    let resp = super::post_settings(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"content": doc}).to_string(),
    )
    .await;
    // Whether it takes {"content": ...} or the raw document, it must not
    // answer a server error.
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// `[daemon] preallocate` is commented out in the template, so the switch
/// offered in the settings tab must be able to create it; only a boolean.
#[tokio::test]
async fn settings_create_daemon_preallocate_when_absent() {
    let s = state_from("set-prealloc", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let post = |value: serde_json::Value| {
        super::post_settings(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"changes":[{"section":"daemon","key":"preallocate","value":value}]})
                .to_string(),
        )
    };
    let bad = post(serde_json::json!("yes")).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST, "non-boolean refused");
    let resp = post(serde_json::json!(true)).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    let cfg: toml::Value = toml::from_str(&file).unwrap();
    assert_eq!(
        cfg["daemon"]["preallocate"].as_bool(),
        Some(true),
        "not written under [daemon]: {file}"
    );
    assert!(s.state.cfg().daemon.preallocate);
}

/// The recheck limits are commented out in the template: the settings tab
/// creates them, refuses one out of range (the daemon would not start
/// with it) and says no restart is needed. The value written is the
/// default, 0 (by the kind of disk): the limits are the whole process's,
/// and a test of the proofs next door counts one reader per disk.
#[tokio::test]
async fn settings_create_the_recheck_limits_and_refuse_one_out_of_range() {
    let s = state_from("set-recheck", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let post = |value: serde_json::Value| {
        super::post_settings(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"changes":[{"section":"daemon","key":"recheck_per_disk","value":value}]})
                .to_string(),
        )
    };
    for bad in [
        serde_json::json!(-1),
        serde_json::json!(65),
        serde_json::json!("two"),
    ] {
        let resp = post(bad.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad} accepted");
    }
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    assert!(
        !file.contains("recheck_per_disk"),
        "a refused value was written: {file}"
    );

    let resp = post(serde_json::json!(0)).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["restart_required"], false, "applied live: {v}");
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    let cfg: toml::Value = toml::from_str(&file).unwrap();
    assert_eq!(
        cfg["daemon"]["recheck_per_disk"].as_integer(),
        Some(0),
        "{file}"
    );
    assert_eq!(s.state.cfg().daemon.recheck_per_disk, Some(0));
}

/// `read_cache_mb` from the settings tab: absent from the file, created;
/// out of range, refused and not written; in range, live.
#[tokio::test]
async fn settings_create_read_cache_mb_and_refuse_one_out_of_range() {
    let s = state_from("set-readcache", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let post = |value: serde_json::Value| {
        super::post_settings(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"changes":[{"section":"daemon","key":"read_cache_mb","value":value}]})
                .to_string(),
        )
    };
    for bad in [serde_json::json!(-1), serde_json::json!(65537)] {
        let resp = post(bad.clone()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad} accepted");
    }
    let file = std::fs::read_to_string(&s.state.config_path).unwrap();
    assert!(
        !file.contains("read_cache_mb"),
        "a refused value was written: {file}"
    );

    let resp = post(serde_json::json!(512)).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["restart_required"], false, "applied live: {v}");
    assert_eq!(s.state.cfg().daemon.read_cache_bytes(), 512 << 20);
}

/// A settings document that is not valid TOML is refused rather than
/// written -- writing it would make the daemon unstartable.
#[tokio::test]
async fn settings_that_are_not_valid_toml_are_refused() {
    let s = state_from("set-badtoml", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let resp = super::post_settings(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"content": "[unclosed\nnot = toml ="}).to_string(),
    )
    .await;
    assert!(
        !resp.status().is_success(),
        "invalid TOML must not be written: {:?}",
        resp.status()
    );
}

/// A category can be updated after it was created, and updating one that
/// does not exist is reported rather than silently creating it.
#[tokio::test]
async fn a_category_is_updated_and_an_unknown_one_is_reported() {
    let s = state_from("cat-update", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    super::category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;

    let updated = super::category_update(
        State(s.state.clone()),
        axum::extract::Path("films".to_string()),
        RawQuery(None),
        keyed(KEY),
        r#"{"save_path":"/data/movies","mode":"hoard"}"#.to_string(),
    )
    .await;
    assert!(updated.status().is_success(), "got {:?}", updated.status());

    let (_engine, path) = placement(&s.state, "films", "");
    assert_eq!(
        path, "/data/movies",
        "the new save path is what placement uses"
    );
}

/// Deleting a category that torrents still point at, then placing into it,
/// must fall back rather than place a torrent nowhere.
#[tokio::test]
async fn placing_into_a_deleted_category_falls_back_to_race() {
    let s = state_from("cat-deleted", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    super::category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;
    super::category_delete(
        State(s.state.clone()),
        axum::extract::Path("films".to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    let (engine, _path) = placement(&s.state, "films", "");
    assert_eq!(
        engine, "race",
        "a category that is gone is an unknown category"
    );
}

/// ⭐ Moving a torrent to another engine is a JOB, never inline: a bulk
/// move would otherwise hold the request open for terabytes of copying.
#[tokio::test]
async fn moving_a_torrent_between_engines_is_answered() {
    let (s, hash) = with_torrent("move-engine");
    let resp = super::post_torrent_engine(
        State(s.state.clone()),
        axum::extract::Path(hash),
        RawQuery(None),
        keyed(KEY),
        r#"{"engine":"hoard"}"#.to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// A move carries the torrent's own state across: an edited tracker list
/// and the stopped state survive, and the source lets go only once the
/// target holds it. 4.3 re-added the .torrent in seed mode and lost both.
#[tokio::test]
async fn a_move_between_engines_keeps_the_torrent_as_it_was() {
    let (s, hash) = with_torrent("move-keeps");
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    let race = s.state.engines.get("race").unwrap();
    race.manager
        .set_trackers(&ih, vec![vec!["https://edited.example/announce".into()]])
        .unwrap();
    let resp = super::post_torrent_engine(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"engine":"hoard"}"#.to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(race.manager.get(&ih).is_none(), "the source let go");
    let hoard = s.state.engines.get("hoard").unwrap();
    let t = hoard.manager.get(&ih).expect("the target holds it");
    assert_eq!(
        *t.live_trackers.read(),
        vec![vec!["https://edited.example/announce".to_string()]]
    );
    assert!(
        t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
        "added paused, still paused"
    );
}

/// Copying to an engine that does not exist is refused: the copy would
/// have nowhere to land.
#[tokio::test]
async fn copying_to_an_engine_that_does_not_exist_is_refused() {
    let (s, hash) = with_torrent("copy-bad");
    let resp = super::post_torrent_copy(
        State(s.state.clone()),
        axum::extract::Path(hash),
        RawQuery(None),
        keyed(KEY),
        r#"{"to":"no-such-engine"}"#.to_string(),
    )
    .await;
    assert!(!resp.status().is_success(), "got {:?}", resp.status());
}

/// A tracker can be added to a torrent we hold, and the tracker list
/// reflects it -- the two views read the same state.
#[tokio::test]
async fn a_tracker_added_to_a_torrent_shows_up_in_its_tracker_list() {
    let (s, hash) = with_torrent("add-tracker");
    let resp = super::post_add_tracker(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"url":"https://second.example/announce"}"#.to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());

    let listed = body_json(
        super::get_torrent_trackers(
            State(s.state.clone()),
            axum::extract::Path(hash),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert!(
        listed.to_string().contains("tracker.example"),
        "got {listed}"
    );
}

/// Replacing the tracker list REPLACES it; the old host must be gone.
#[tokio::test]
async fn replacing_the_tracker_list_drops_the_old_host() {
    let (s, hash) = with_torrent("set-trackers");
    let resp = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"trackers":["https://other.example/announce"]}"#.to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

async fn rehost_call(s: &TestState, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = crate::rehostrun::post_rehost(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

async fn rehost_status(s: &TestState) -> serde_json::Value {
    body_json(
        crate::rehostrun::get_rehost(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await
}

/// Run the queued job the way the runner does, to its end.
fn run_rehost_job(s: &TestState) -> Result<(), String> {
    let job = s
        .store
        .lock()
        .unwrap()
        .claim_next_job()
        .expect("a queued job");
    let out = crate::jobsrun::run_job(&s.state, &job);
    let err = out.clone().err().unwrap_or_default();
    s.store.lock().unwrap().job_finish(&job.id, &err).unwrap();
    out
}

fn stored_trackers(s: &TestState, hash: &str) -> String {
    let conn = rusqlite::Connection::open(s.dir.join("race").join("state.db")).unwrap();
    conn.query_row(
        "SELECT trackers FROM torrent_state WHERE info_hash = ?1",
        [hash],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

/// The passkey and IP family an announce uses come from the most specific
/// key covering the host, a parent domain included. Moving a subdomain
/// whose settings sit on its parent must carry those values to the new
/// host, and leave the parent's keys alone: other subdomains still use them.
#[tokio::test]
async fn moving_a_subdomain_carries_the_settings_its_parent_gave_it() {
    let s = state_from(
        "rehost-parent",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n\
             [announce_ip_modes]\n\"tracker.example\" = \"v6\"\n"
        ),
    );
    let (h, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("parent"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let r = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(h.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"op":"set","tiers":[["https://tk.tracker.example/announce"]]}"#.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    let (code, v) =
        rehost_call(&s, r#"{"from":"tk.tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    // Carried BEFORE any torrent is rewritten: the job is only queued.
    let cfg = s.state.cfg();
    assert_eq!(
        cfg.announce_passkeys.get("new.example").map(String::as_str),
        Some("SECRET")
    );
    assert_eq!(
        cfg.announce_ip_modes.get("new.example").map(String::as_str),
        Some("v6")
    );

    run_rehost_job(&s).expect("the job ends well");
    assert_eq!(rehost_status(&s).await["changed"], 1);
    let cfg = s.state.cfg();
    assert_eq!(cfg.announce_passkeys["tracker.example"], "SECRET");
    assert_eq!(cfg.announce_ip_modes["tracker.example"], "v6");
    assert_eq!(cfg.announce_passkeys["new.example"], "SECRET");
}

/// The new host may already be covered by a parent domain's key with
/// ANOTHER passkey. Leaving it at that would announce the moved torrents
/// with the parent's passkey once the old host's key is removed: the old
/// host's own value is written for the new host instead.
#[tokio::test]
async fn moving_onto_a_parent_with_another_passkey_keeps_the_torrents_own() {
    let s = state_from(
        "rehost-other-parent",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"OWN\"\n\"parent.example\" = \"PARENT\"\n"
        ),
    );
    let (h, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("own"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let r = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(h.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"op":"set","tiers":[["https://tracker.example/announce"]]}"#.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    let (code, v) = rehost_call(
        &s,
        r#"{"from":"tracker.example","to":"new.parent.example"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    run_rehost_job(&s).expect("the job ends well");
    let cfg = s.state.cfg();
    assert_eq!(
        cfg.announce_passkeys
            .get("new.parent.example")
            .map(String::as_str),
        Some("OWN")
    );
    assert_eq!(cfg.announce_passkeys["parent.example"], "PARENT");
}

/// Moving a tracker host: the count first, then a job that rewrites every
/// torrent on it, keeps the edit across a restart, carries the host's
/// settings to the new one, and refuses a second move while one runs.
#[tokio::test]
async fn moving_a_tracker_host_rewrites_the_torrents_and_carries_the_settings() {
    let s = state_from(
        "rehost",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n\
             [announce_muted]\n\"tracker.example\" = \"1\"\n"
        ),
    );
    let mut hashes = Vec::new();
    for name in ["alpha", "beta", "gamma"] {
        let (h, _) = add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            "/tmp",
            "",
            true,
            true,
            "race",
        )
        .expect("added");
        hashes.push(h);
    }
    // One torrent already elsewhere: it must not count, nor move.
    let r = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(hashes[2].clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"op":"set","tiers":[["https://other.example/announce"]]}"#.to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    // Refusals first.
    let (code, _) = rehost_call(
        &s,
        r#"{"from":"tracker.example","to":"https://new.example"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) =
        rehost_call(&s, r#"{"from":"tracker.example","to":"tracker.example"}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    // A dry run counts and writes nothing.
    let (code, v) = rehost_call(
        &s,
        r#"{"from":"tracker.example","to":"new.example","dry_run":true}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["torrents"], 2);
    assert!(!s
        .store
        .lock()
        .unwrap()
        .job_in_flight_of_kind("tracker_rehost"));
    assert!(!s.state.cfg().announce_passkeys.contains_key("new.example"));
    assert_eq!(rehost_status(&s).await["state"], "none");

    let (code, v) = rehost_call(&s, r#"{"from":"tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["torrents"], 2);
    assert!(v["job_id"].as_str().is_some_and(|j| !j.is_empty()));
    // Settings are copied as the job starts, the old ones still there.
    let cfg = s.state.cfg();
    assert_eq!(cfg.announce_passkeys["new.example"], "SECRET");
    assert!(cfg.announce_muted.contains_key("new.example"));
    assert!(cfg.announce_passkeys.contains_key("tracker.example"));

    // One at a time.
    let (code, _) = rehost_call(&s, r#"{"from":"tracker.example","to":"third.example"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(rehost_status(&s).await["state"], "queued");

    run_rehost_job(&s).expect("the job ends well");

    let st = rehost_status(&s).await;
    assert_eq!(st["state"], "done", "{st}");
    assert_eq!(st["from"], "tracker.example");
    assert_eq!(st["to"], "new.example");
    assert_eq!(
        (st["total"].as_i64(), st["done"].as_i64()),
        (Some(2), Some(2))
    );
    assert_eq!(st["changed"], 2);
    assert_eq!(st["skipped"], 0);
    assert_eq!(st["failed"], 0);
    assert_eq!(st["settings_kept"], false);

    let race = s.state.engines.get("race").unwrap();
    for h in &hashes[..2] {
        let ih = typhon_engine::torrent::hex_decode(h).unwrap();
        assert_eq!(
            *race.manager.get(&ih).unwrap().live_trackers.read(),
            vec![vec!["https://new.example/announce".to_string()]]
        );
        let kept = stored_trackers(&s, h);
        assert!(
            kept.contains("new.example") && !kept.contains("tracker.example"),
            "{kept}"
        );
    }
    let ih = typhon_engine::torrent::hex_decode(&hashes[2]).unwrap();
    assert_eq!(
        *race.manager.get(&ih).unwrap().live_trackers.read(),
        vec![vec!["https://other.example/announce".to_string()]]
    );
    // Nothing left on the old host: its settings went, the new ones stay.
    let cfg = s.state.cfg();
    assert!(!cfg.announce_passkeys.contains_key("tracker.example"));
    assert!(!cfg.announce_muted.contains_key("tracker.example"));
    assert_eq!(cfg.announce_passkeys["new.example"], "SECRET");

    // The slot is free again, and the job a second time finds nothing.
    let (code, _) = rehost_call(&s, r#"{"from":"tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "nothing left on that host");
}

/// A format move: the passkey goes from the query to a path segment. The
/// dry run counts and shows masked samples, the job rewrites what matches,
/// persists it, carries the settings, and leaves a URL that does not match
/// the template exactly as it was.
#[tokio::test]
async fn moving_a_tracker_format_rewrites_the_matching_urls_only() {
    const K1: &str = "0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd";
    const K2: &str = "fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876fedc9876";
    const FROM: &str = "https://old.example.invalid/tracker/announce?passkey={passkey}";
    const TO: &str = "https://new.example.invalid/{passkey}/announce";
    let s = state_from(
        "reformat",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"old.example.invalid\" = \"SECRET\"\n\
             [announce_muted]\n\"old.example.invalid\" = \"1\"\n"
        ),
    );
    let urls = [
        format!("https://old.example.invalid/tracker/announce?passkey={K1}"),
        format!("https://old.example.invalid/tracker/announce?passkey={K2}"),
        // On the old host, but not in that format: left alone.
        "https://old.example.invalid/other/announce".to_string(),
        format!("https://old.example.invalid/tracker/announce?passkey={K1}&extra=1"),
        // Elsewhere: not this move's business.
        format!("https://elsewhere.example.invalid/tracker/announce?passkey={K1}"),
    ];
    let mut hashes = Vec::new();
    for (name, url) in ["alpha", "beta", "gamma", "delta", "eps"].iter().zip(&urls) {
        let (h, _) = add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            "/tmp",
            "",
            true,
            true,
            "race",
        )
        .expect("added");
        let r = super::post_torrent_trackers(
            State(s.state.clone()),
            axum::extract::Path(h.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"op": "set", "tiers": [[url]]}).to_string(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        hashes.push(h);
    }
    let body =
        |extra: &str| format!(r#"{{"from_template":"{FROM}","to_template":"{TO}"{extra}}}"#);

    // Refusals: a hole the old format lacks, a mix with nothing to match.
    let (code, _) = rehost_call(
        &s,
        &format!(
            r#"{{"from_template":"{FROM}","to_template":"https://new.example.invalid/{{uid}}"}}"#
        ),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = rehost_call(
        &s,
        &format!(r#"{{"from_template":"{FROM}","to_template":"{FROM}"}}"#),
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);

    // A dry run counts, masks, and writes nothing.
    let (code, v) = rehost_call(&s, &body(r#","dry_run":true"#)).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["torrents"], 2);
    assert_eq!(v["unmatched"], 2);
    let samples = v["samples"].as_array().unwrap();
    assert_eq!(samples.len(), 2);
    let shown = v.to_string();
    assert!(!shown.contains(K1) && !shown.contains(K2), "{shown}");
    assert!(
        !shown.contains(&K1[..8]) && !shown.contains(&K2[..8]),
        "{shown}"
    );
    assert!(
        samples.iter().all(|x| {
            x["before"]
                .as_str()
                .unwrap()
                .starts_with("https://old.example.invalid/tracker/announce?passkey=")
                && x["after"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://new.example.invalid/")
        }),
        "{shown}"
    );
    assert!(!s
        .store
        .lock()
        .unwrap()
        .job_in_flight_of_kind("tracker_rehost"));
    assert!(!s
        .state
        .cfg()
        .announce_passkeys
        .contains_key("new.example.invalid"));
    assert_eq!(rehost_status(&s).await["state"], "none");

    // The job.
    let (code, v) = rehost_call(&s, &body("")).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        (v["torrents"].as_i64(), v["unmatched"].as_i64()),
        (Some(2), Some(2))
    );
    let cfg = s.state.cfg();
    assert_eq!(cfg.announce_passkeys["new.example.invalid"], "SECRET");
    assert!(cfg.announce_muted.contains_key("new.example.invalid"));
    let (code, _) = rehost_call(&s, &body("")).await;
    assert_eq!(code, StatusCode::CONFLICT);

    run_rehost_job(&s).expect("the job ends well");
    let st = rehost_status(&s).await;
    assert_eq!(st["state"], "done", "{st}");
    assert_eq!(st["mode"], "format");
    assert_eq!(
        (
            st["changed"].as_i64(),
            st["skipped"].as_i64(),
            st["failed"].as_i64()
        ),
        (Some(2), Some(0), Some(0))
    );
    assert_eq!(st["unmatched"], 2);
    assert_eq!(st["from"], "old.example.invalid");
    assert_eq!(st["to"], "new.example.invalid");
    assert!(!st.to_string().contains(K1), "{st}");

    let race = s.state.engines.get("race").unwrap();
    let live = |i: usize| {
        let ih = typhon_engine::torrent::hex_decode(&hashes[i]).unwrap();
        let t = race.manager.get(&ih).unwrap().live_trackers.read().clone();
        t
    };
    assert_eq!(
        live(0),
        vec![vec![format!("https://new.example.invalid/{K1}/announce")]]
    );
    assert_eq!(
        live(1),
        vec![vec![format!("https://new.example.invalid/{K2}/announce")]]
    );
    for (i, url) in urls.iter().enumerate().skip(2) {
        assert_eq!(
            live(i),
            vec![vec![url.clone()]],
            "torrent {i} must not change"
        );
    }
    // Persisted: what state.db holds is the new URL.
    let kept = stored_trackers(&s, &hashes[0]);
    assert!(
        kept.contains(&format!("new.example.invalid/{K1}/announce"))
            && !kept.contains("old.example.invalid"),
        "{kept}"
    );
    assert!(stored_trackers(&s, &hashes[2]).contains("old.example.invalid/other/announce"));
    // Torrents still on the old host: its settings stay.
    let cfg = s.state.cfg();
    assert!(cfg.announce_passkeys.contains_key("old.example.invalid"));
    assert_eq!(st["settings_kept"], true);

    // Nothing matches any more.
    let (code, _) = rehost_call(&s, &body("")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, v) = rehost_call(&s, &body(r#","dry_run":true"#)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        (v["torrents"].as_i64(), v["unmatched"].as_i64()),
        (Some(0), Some(2))
    );
}

/// The form prefills the old format from a real URL of the host, with the
/// passkey as a hole and never in the answer.
#[tokio::test]
async fn the_format_of_a_host_is_guessed_without_its_passkey() {
    const K1: &str = "0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd0123abcd";
    let s = state_from(
        "reformat-guess",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let (h, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("g"),
        "",
        "/tmp",
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let r = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(h),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"op": "set", "tiers": [[format!("https://old.example.invalid/tracker/announce?passkey={K1}")]]}).to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(
        crate::rehostrun::get_format_guess(
            State(s.state.clone()),
            RawQuery(Some("host=old.example.invalid".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert_eq!(
        v["format"],
        "https://old.example.invalid/tracker/announce?passkey={passkey}"
    );
    let v = body_json(
        crate::rehostrun::get_format_guess(
            State(s.state.clone()),
            RawQuery(Some("host=nobody.example.invalid".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert!(v["format"].is_null());
}

/// A torrent whose .torrent is not stored yet cannot keep the edit: it is
/// counted as skipped, stays on the old host, and the old settings stay.
#[tokio::test]
async fn a_torrent_without_a_stored_blob_is_skipped_and_the_settings_kept() {
    let s = state_from(
        "rehost-noblob",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n"
        ),
    );
    let mut hashes = Vec::new();
    for name in ["alpha", "beta"] {
        let (h, _) = add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            "/tmp",
            "",
            true,
            true,
            "race",
        )
        .expect("added");
        hashes.push(h);
    }
    s.store.lock().unwrap().delete_torrent(&hashes[1]).unwrap();
    let (code, v) = rehost_call(&s, r#"{"from":"tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    run_rehost_job(&s).expect("the job ends");
    let st = rehost_status(&s).await;
    assert_eq!(st["state"], "done", "{st}");
    assert_eq!(
        (st["changed"].as_i64(), st["skipped"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(st["settings_kept"], true);
    assert!(s
        .state
        .cfg()
        .announce_passkeys
        .contains_key("tracker.example"));
}

/// Add `names` as torrents announcing to `tracker.example`.
fn add_on_tracker_example(s: &TestState, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|name| {
            add_torrent_bytes(
                &s.state,
                &torrent_bytes(name),
                "",
                "/tmp",
                "",
                true,
                true,
                "race",
            )
            .expect("added")
            .0
        })
        .collect()
}

/// A move that is refused is refused before anything is counted, queued or
/// copied: no key, an unreadable body, a missing host and a host with a
/// scheme all leave the queue empty and the settings as they were.
#[tokio::test]
async fn a_refused_move_queues_nothing_and_copies_nothing() {
    let s = state_from(
        "rehost-refused",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n"
        ),
    );
    add_on_tracker_example(&s, &["alpha"]);
    let body = r#"{"from":"tracker.example","to":"new.example"}"#;

    let resp = crate::rehostrun::post_rehost(
        State(s.state.clone()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
        body.to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = crate::rehostrun::get_rehost(
        State(s.state.clone()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    for bad in [
        "not json",
        r#"{"from":"","to":"new.example"}"#,
        r#"{"from":"tracker.example","to":""}"#,
    ] {
        let (code, v) = rehost_call(&s, bad).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}: {v}");
    }
    assert!(!s
        .store
        .lock()
        .unwrap()
        .job_in_flight_of_kind("tracker_rehost"));
    assert!(!s.state.cfg().announce_passkeys.contains_key("new.example"));
    assert_eq!(rehost_status(&s).await["state"], "none");
}

/// ⭐ Cancelling stops the run after the batch in hand, reports how many
/// were already moved, and does NOT delete the old host's settings:
/// the torrents not reached still announce with them.
#[tokio::test]
async fn a_cancelled_move_keeps_the_old_hosts_settings() {
    let s = state_from(
        "rehost-cancel",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n"
        ),
    );
    add_on_tracker_example(&s, &["alpha", "beta"]);
    let (code, v) = rehost_call(&s, r#"{"from":"tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");

    let job = s.store.lock().unwrap().claim_next_job().expect("queued");
    assert_eq!(
        s.store.lock().unwrap().cancel_job(&job.id).as_deref(),
        Some("running")
    );
    let err = crate::rehostrun::run(&s.state, &job).unwrap_err();
    assert_eq!(err, "cancelled; 2 torrents were already moved");
    s.store.lock().unwrap().job_finish(&job.id, &err).unwrap();

    let cfg = s.state.cfg();
    assert_eq!(cfg.announce_passkeys["tracker.example"], "SECRET");
    assert_eq!(cfg.announce_passkeys["new.example"], "SECRET");
    let st = rehost_status(&s).await;
    assert_eq!(st["changed"], 2, "{st}");
    assert_eq!(st["done"], 2, "{st}");
}

/// A job found running at boot starts again: what the first run moved
/// (`changed` in its params) no longer matches the host, so the new total
/// is that plus what is left, and the counter carries on from it.
#[tokio::test]
async fn a_resumed_move_counts_what_the_first_run_already_moved() {
    let s = state_from(
        "rehost-resume",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\
             [announce_passkeys]\n\"tracker.example\" = \"SECRET\"\n"
        ),
    );
    add_on_tracker_example(&s, &["alpha"]);
    let (code, v) = rehost_call(&s, r#"{"from":"tracker.example","to":"new.example"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    let job = s.store.lock().unwrap().claim_next_job().expect("queued");
    let mut p: serde_json::Value = serde_json::from_str(&job.params).unwrap();
    p["changed"] = serde_json::json!(4);
    s.store
        .lock()
        .unwrap()
        .set_job_params(&job.id, &p.to_string())
        .unwrap();
    let job = s.store.lock().unwrap().job(&job.id).unwrap();

    crate::rehostrun::run(&s.state, &job).expect("the run ends well");
    let st = rehost_status(&s).await;
    assert_eq!(st["changed"], 5, "{st}");
    assert_eq!(
        (st["total"].as_i64(), st["done"].as_i64()),
        (Some(5), Some(5)),
        "{st}"
    );
    assert_eq!(st["skipped"], 0);
    assert_eq!(st["failed"], 0);
}

/// A tracker list or a speed cap the engine's state store could not keep
/// is answered with an error, not 200, and the engine keeps what it had.
#[tokio::test]
async fn an_edit_the_engine_store_did_not_keep_is_not_acknowledged() {
    let (s, hash) = with_torrent("edit-store-down");
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    let race = s.state.engines.get("race").unwrap();
    let trackers_before = race.manager.get(&ih).unwrap().live_trackers.read().clone();
    let conn = rusqlite::Connection::open(s.dir.join("race").join("state.db")).unwrap();
    conn.execute("DROP TABLE torrent_state", []).unwrap();
    drop(conn);

    let r = super::post_torrent_limits(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"up_kib":50}"#.to_string(),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a cap the store refused"
    );
    assert_eq!(race.manager.get(&ih).unwrap().rate_limits(), (0, 0));

    let r = super::post_torrent_trackers(
        State(s.state.clone()),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"op":"set","tiers":[["https://edited.example/announce"]]}"#.to_string(),
    )
    .await;
    assert_eq!(
        r.status(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "a tracker list the store refused"
    );
    assert_eq!(
        *race.manager.get(&ih).unwrap().live_trackers.read(),
        trackers_before
    );
}

/// An add the engine's state store refused is answered with an error, and
/// leaves no torrent in the engine and no row in the hub's store. Every
/// add route (native, qBittorrent shim, MCP, magnets) goes through this.
#[tokio::test]
async fn an_add_the_engine_store_did_not_keep_is_refused() {
    let s = state_from(
        "add-store-down",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let conn = rusqlite::Connection::open(s.dir.join("race").join("state.db")).unwrap();
    conn.execute("DROP TABLE torrent_state", []).unwrap();
    drop(conn);

    let bytes = torrent_bytes("alpha");
    let ih = typhon_engine::torrent::metainfo::parse_torrent_bytes(&bytes)
        .unwrap()
        .info_hash;
    let r = add_torrent_bytes(&s.state, &bytes, "", "/tmp", "", true, true, "race");
    assert!(
        r.is_err(),
        "the engine's store refused the record, so the answer is no: {r:?}"
    );
    assert!(
        s.state
            .engines
            .get("race")
            .unwrap()
            .manager
            .get(&ih)
            .is_none(),
        "no torrent is left in the engine"
    );
    let hash = typhon_engine::torrent::hex_encode(&ih);
    assert!(
        !s.state.store.lock().unwrap().has_torrent_blob(&hash),
        "nor a row in the hub's store"
    );
}

/// A job created by the daemon is listed and can be read back by id -- the
/// UI polls on exactly this.
#[tokio::test]
async fn a_job_is_listed_and_readable_by_its_id() {
    let s = state_from("jobs", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let id = {
        let store = s.store.lock().unwrap();
        store
            .create_job("move", &"a".repeat(40), "{}", 1000)
            .expect("created")
    };

    let listed =
        body_json(super::get_jobs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(
        listed.to_string().contains(&id),
        "the job is listed: {listed}"
    );

    let one = super::get_job(
        State(s.state.clone()),
        axum::extract::Path(id.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(one.status().is_success(), "got {:?}", one.status());
    let body = body_json(one).await;
    assert_eq!(body["id"], serde_json::json!(id));
    assert_eq!(
        body["type"],
        serde_json::json!("move"),
        "`type` in JSON, `kind` in Rust"
    );
}

/// Deleting a job that exists works; deleting it twice is reported.
#[tokio::test]
async fn a_job_can_be_deleted_once() {
    let s = state_from("jobs-delete", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let id = {
        let store = s.store.lock().unwrap();
        store
            .create_job("move", &"a".repeat(40), "{}", 10)
            .expect("created")
    };
    let first = super::delete_job(
        State(s.state.clone()),
        axum::extract::Path(id.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(
        !first.status().is_server_error(),
        "got {:?}",
        first.status()
    );
}

/// Checking import paths that do not exist reports them as missing rather
/// than starting an import that would fail torrent by torrent.
#[tokio::test]
async fn import_paths_that_do_not_exist_are_reported_before_the_import() {
    let s = state_from("import-check", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let resp = super::import_check_paths(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"paths": ["/tmp/typhon-no-such-dir-8c2a"]}).to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    let body = body_json(resp).await;
    assert!(body.is_object() || body.is_array(), "got {body}");
}

/// An existing path checks out -- the other half of the same route.
#[tokio::test]
async fn an_import_path_that_exists_checks_out() {
    let s = state_from("import-ok", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let resp = super::import_check_paths(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"paths": ["/tmp"]}).to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// A volume policy typed in the panel is stored and read back by the
/// drain -- it must apply on the NEXT tick, not after a restart.
#[tokio::test]
async fn a_volume_policy_is_stored_and_read_back_by_the_drain() {
    let s = state_from(
        "volpolicy-store",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let body: VolumePolicyBody = serde_json::from_value(serde_json::json!({
        "volume": "/mnt/race",
        "enabled": true,
        "high_watermark": 91,
        "low_watermark": 77
    }))
    .expect("a valid policy body");
    let resp = super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(body),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());

    let cfg = s.cfg();
    let p = crate::volumes::policy_for(&s.state, "/mnt/race", &cfg.race_drain);
    assert_eq!(p.high, 91, "the drain reads what the panel wrote");
    assert_eq!(p.low, 77);
    assert!(!p.inherited, "it is this volume's own policy now");
}

/// TRIM after drain is a fact about the disk: turning it on alone must not
/// freeze today's watermarks onto the volume, and it is off by default.
#[tokio::test]
async fn trim_after_drain_is_off_by_default_and_does_not_freeze_the_watermarks() {
    let s = state_from("voltrim", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    assert!(
        !crate::volumes::trim_enabled(&s.state, "/mnt/race"),
        "off unless asked"
    );
    let body: VolumePolicyBody = serde_json::from_value(serde_json::json!({
        "volume": "/mnt/race", "trim_after_drain": true
    }))
    .unwrap();
    let resp = super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(body),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    assert!(crate::volumes::trim_enabled(&s.state, "/mnt/race"));
    let cfg = s.cfg();
    let p = crate::volumes::policy_for(&s.state, "/mnt/race", &cfg.race_drain);
    assert!(
        p.inherited,
        "the watermarks still follow the global default"
    );

    let off: VolumePolicyBody = serde_json::from_value(serde_json::json!({
        "volume": "/mnt/race", "trim_after_drain": false
    }))
    .unwrap();
    super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(off),
    )
    .await;
    assert!(!crate::volumes::trim_enabled(&s.state, "/mnt/race"));
}

/// A trim that runs leaves its outcome where the panel reads it, whether
/// the filesystem accepted it or not -- a test container usually refuses
/// (no CAP_SYS_ADMIN, overlayfs), which is the case worth seeing.
#[tokio::test]
async fn a_trim_after_drain_writes_down_its_outcome() {
    let s = state_from("voltrim-run", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let dir = std::env::temp_dir().join(format!("hydranos-trim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mount = dir.to_string_lossy().to_string();
    crate::volumes::trim_after_drain(&s.state, &mount);
    assert!(
        crate::volumes::last_trim(&s.state, &mount).is_none(),
        "off: nothing runs"
    );

    crate::volumes::save_trim(&s.state, &mount, true).unwrap();
    crate::volumes::trim_after_drain(&s.state, &mount);
    let mut last = None;
    // A real discard on a real SSD takes seconds (19 s for 1.8 TB); a
    // refusal comes back at once.
    for _ in 0..1200 {
        last = crate::volumes::last_trim(&s.state, &mount);
        if last.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let last = last.expect("the outcome is written down");
    eprintln!("trim outcome on {mount}: {last}");
    assert!(last.get("secs").is_some(), "got {last}");
    assert!(
        last.get("bytes").is_some() || last.get("error").is_some(),
        "got {last}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Clearing a volume's policy puts it back on the global default.
#[tokio::test]
async fn clearing_a_volume_policy_returns_it_to_the_global_default() {
    let s = state_from(
        "volpolicy-clear",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let set: VolumePolicyBody = serde_json::from_value(serde_json::json!({
        "volume": "/mnt/race", "enabled": true, "high_watermark": 91, "low_watermark": 77
    }))
    .unwrap();
    super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(set),
    )
    .await;

    let inherit: VolumePolicyBody = serde_json::from_value(serde_json::json!({
        "volume": "/mnt/race", "inherit": true
    }))
    .unwrap();
    super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Json(inherit),
    )
    .await;

    let cfg = s.cfg();
    let p = crate::volumes::policy_for(&s.state, "/mnt/race", &cfg.race_drain);
    assert!(p.inherited, "back on the global default");
}
