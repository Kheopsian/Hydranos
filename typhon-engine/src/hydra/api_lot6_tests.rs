//! Three routes and one helper that act on data and had no test that reads
//! the result back: the race drain's button (`drain_now`), the uploaded
//! `.torrent` add (`post_torrent_upload`) and what a category rename carries
//! with it (`rename_category_references`).

use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const SIZE: u64 = 16_384;
const TRACKER: &str = "https://tracker.example/announce";

/// A single-file torrent, unique per name (the piece hash carries it).
fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi{SIZE}e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xABu8; 20];
    for (i, b) in name.bytes().enumerate().take(20) {
        piece[i] = b;
    }
    info.extend_from_slice(&piece);
    info.push(b'e');
    let mut out = Vec::new();
    out.extend_from_slice(format!("d8:announce{}:{TRACKER}4:info", TRACKER.len()).as_bytes());
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn st(tag: &str, extra: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"))
}

fn manager(s: &TestState, engine: &str) -> std::sync::Arc<typhon_engine::torrent::TorrentManager> {
    s.engines.get(engine).expect("engine").manager.clone()
}

// ---------------------------------------------------------------------------
// The drain button
// ---------------------------------------------------------------------------

/// A seed on `engine` with its file on disk and a row in the store, announcing
/// to `tracker`, filed under `category`, having served `seeded_secs`.
fn seed(
    s: &TestState,
    engine: &str,
    name: &str,
    category: &str,
    seeded_secs: i64,
) -> (String, std::path::PathBuf) {
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(name);
    std::fs::write(&file, vec![0u8; SIZE as usize]).unwrap();
    let (hash, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes(name),
        category,
        dir.to_str().unwrap(),
        "",
        false,
        true,
        engine,
    )
    .expect("added");
    let t = manager(s, engine)
        .get(&typhon_engine::torrent::hex_decode(&hash).unwrap())
        .expect("held");
    {
        let mut live = t.live_trackers.write();
        live.clear();
        live.push(vec![TRACKER.to_string()]);
    }
    t.status.store(
        typhon_engine::torrent::meta::TorrentStatus::Seeding as u8,
        std::sync::atomic::Ordering::Relaxed,
    );
    t.is_paused
        .store(false, std::sync::atomic::Ordering::Relaxed);
    t.seed_secs
        .store(seeded_secs, std::sync::atomic::Ordering::Relaxed);
    t.seed_since.store(0, std::sync::atomic::Ordering::Relaxed);
    (hash, file)
}

fn in_store(s: &TestState, hash: &str) -> bool {
    ["race", "hoard"].iter().any(|e| {
        s.store
            .read()
            .unwrap()
            .all_hashes(e)
            .unwrap()
            .iter()
            .any(|h| h == hash)
    })
}

/// Where the test directory's volume is, as the drain names it.
fn mount(s: &TestState) -> String {
    crate::volumes::mount_point_of(&s.dir)
        .to_string_lossy()
        .to_string()
}

/// The volume's own policy: drain from `high` percent down to `low`.
fn policy(s: &TestState, high: i64, low: i64) {
    let p = crate::volumes::Policy {
        enabled: true,
        high,
        low,
        inherited: false,
    };
    crate::volumes::save_policy(&s.state, &mount(s), &p).unwrap();
}

fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn press(s: &TestState, query: Option<&str>) -> serde_json::Value {
    let resp = drain_now(
        State(s.state.clone()),
        RawQuery(query.map(str::to_string)),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await
}

const FREE_TRACKER: &str = "\n[announce_min_seed_hours]\n\"tracker.example\" = \"0\"\n";

/// ⭐ Pressed on one volume, the button empties it down to ITS low mark: the
/// race torrent goes with its data and its row, a hoard torrent on the same
/// disk is not a race and stays, and the answer counts what was done.
#[tokio::test]
async fn the_button_drains_the_named_volume_and_only_race_torrents() {
    let s = st("dnow-named", FREE_TRACKER);
    policy(&s, 0, 0);
    let (race, race_file) = seed(&s, "race", "ra", "", 100);
    let (hoard, hoard_file) = seed(&s, "hoard", "ha", "", 100);

    let v = press(&s, Some(&format!("volume={}", enc(&mount(&s))))).await;

    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["volumes"], serde_json::json!([mount(&s)]));
    assert_eq!(v["deleted"], 1);
    assert_eq!(v["graduated"], 0);
    assert_eq!(v["stuck"], 0);
    assert!(v["freed_bytes"].is_u64(), "{v}");
    assert!(!race_file.exists() && !in_store(&s, &race));
    assert!(manager(&s, "race").all().is_empty());
    assert!(
        hoard_file.exists() && in_store(&s, &hoard),
        "a hoard torrent is not the race drain's"
    );
}

/// ⭐ Scoped to another volume, the button touches nothing, however full this
/// one is: that is what pressing it on the full SSD promises about the other.
#[tokio::test]
async fn a_button_scoped_to_another_volume_touches_nothing() {
    let s = st("dnow-other", FREE_TRACKER);
    policy(&s, 0, 0);
    let (hash, file) = seed(&s, "race", "rb", "", 100);

    let v = press(&s, Some("volume=%2Fnot%2Fthis%2Fvolume")).await;

    assert_eq!(v["status"], "no_volume", "{v}");
    assert_eq!(v["volumes"], serde_json::json!([]));
    assert!(file.exists() && in_store(&s, &hash));

    // The same press, but naming the volume the torrent is on, does drain it.
    let v = press(&s, Some(&format!("volume={}", enc(&mount(&s))))).await;
    assert_eq!(v["deleted"], 1, "{v}");
    assert!(!file.exists());
}

/// Without a volume it passes over every volume that is over its mark and
/// only those: under its high mark, the disk is left alone.
#[tokio::test]
async fn without_a_volume_the_button_skips_a_volume_under_its_mark() {
    let s = st("dnow-all", FREE_TRACKER);
    let (hash, file) = seed(&s, "race", "rc", "", 100);

    policy(&s, 101, 0);
    let v = press(&s, None).await;
    assert_eq!(v["status"], "no_volume", "{v}");
    assert!(file.exists() && in_store(&s, &hash), "under its mark");

    policy(&s, 0, 0);
    let v = press(&s, None).await;
    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(v["volumes"], serde_json::json!([mount(&s)]));
    assert_eq!(v["deleted"], 1);
    assert!(!file.exists() && !in_store(&s, &hash), "over its mark");
}

/// ⭐ A torrent that still owes seeding time and whose category has nowhere to
/// graduate to can be neither deleted nor moved: the button counts it as stuck
/// and says nothing was drained, and the files stay.
#[tokio::test]
async fn a_torrent_owing_seeding_time_with_nowhere_to_go_is_stuck_not_deleted() {
    let s = st(
        "dnow-stuck",
        "\n[announce_min_seed_hours]\n\"tracker.example\" = \"10\"\n",
    );
    policy(&s, 0, 0);
    let (hash, file) = seed(&s, "race", "rd", "", 100);

    let v = press(&s, Some(&format!("volume={}", enc(&mount(&s))))).await;

    assert_eq!(v["status"], "no_drain_needed", "{v}");
    assert_eq!(
        (v["deleted"].clone(), v["graduated"].clone()),
        (0.into(), 0.into())
    );
    assert_eq!(v["stuck"], 1);
    assert!(file.exists() && in_store(&s, &hash));
}

/// ⭐ One that owes time but has a category to graduate into is not deleted
/// either: the button queues the move to the other engine and counts it.
#[tokio::test]
async fn a_torrent_owing_seeding_time_is_graduated_not_deleted() {
    let s = st(
        "dnow-grad",
        "\n[announce_min_seed_hours]\n\"tracker.example\" = \"10\"\n",
    );
    let data = s.dir.join("pool");
    s.store
        .lock()
        .unwrap()
        .put_meta(
            "categories",
            &serde_json::json!({
                "ssd": {"save_path": s.dir.join("data").to_str().unwrap(), "mode": "race", "graduate_to": "pool"},
                "pool": {"save_path": data.to_str().unwrap(), "mode": "hoard"},
            })
            .to_string(),
        )
        .unwrap();
    policy(&s, 0, 0);
    let (hash, file) = seed(&s, "race", "re", "ssd", 100);

    let v = press(&s, Some(&format!("volume={}", enc(&mount(&s))))).await;

    assert_eq!(v["status"], "ok", "{v}");
    assert_eq!(
        (v["deleted"].clone(), v["graduated"].clone()),
        (0.into(), 1.into())
    );
    assert!(file.exists() && in_store(&s, &hash), "queued, not removed");
    let jobs = s.store.lock().unwrap().list_jobs(10).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, "graduate");
    assert_eq!(jobs[0].info_hash, hash);
    let p: serde_json::Value = serde_json::from_str(&jobs[0].params).unwrap();
    assert_eq!(p["to_engine"], "hoard");
    assert_eq!(p["to_category"], "pool");
    assert_eq!(p["save_path"], data.to_str().unwrap());
}

/// The button asks for the API key like every other route.
#[tokio::test]
async fn the_button_needs_the_api_key() {
    let s = st("dnow-key", FREE_TRACKER);
    policy(&s, 0, 0);
    let (hash, file) = seed(&s, "race", "rf", "", 100);
    let resp = drain_now(
        State(s.state.clone()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(file.exists() && in_store(&s, &hash), "nothing was drained");
}

// ---------------------------------------------------------------------------
// The uploaded .torrent
// ---------------------------------------------------------------------------

/// A multipart body: text fields, then the file part under `file_field`.
fn multipart(fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let b = "uploadboundary";
    let mut body: Vec<u8> = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                .as_bytes(),
        );
    }
    if let Some((field, bytes)) = file {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"t.torrent\"\r\nContent-Type: application/x-bittorrent\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

async fn upload(
    s: &TestState,
    fields: &[(&str, &str)],
    file: Option<(&str, &[u8])>,
    key: bool,
) -> (StatusCode, serde_json::Value) {
    let (ct, body) = multipart(fields, file);
    let mut req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/torrents/upload")
        .header("content-type", ct);
    if key {
        req = req.header("X-API-Key", KEY);
    }
    let resp = router(s.state.clone())
        .oneshot(req.body(axum::body::Body::from(body)).unwrap())
        .await
        .unwrap();
    let code = resp.status();
    (code, body_json(resp).await)
}

fn held(
    s: &TestState,
    engine: &str,
    hash: &str,
) -> Option<std::sync::Arc<typhon_engine::torrent::meta::TorrentState>> {
    manager(s, engine).get(&typhon_engine::torrent::hex_decode(hash).unwrap())
}

/// ⭐ The engine, the folder, the paused state and the tags an upload names are
/// the ones the torrent gets, and the answer names the engine it landed in.
#[tokio::test]
async fn an_upload_lands_where_it_says_with_what_it_says() {
    let s = st("upl-ok", "");
    let dir = s.dir.join("incoming");
    let bytes = torrent_bytes("movie.bin");
    let (code, v) = upload(
        &s,
        &[
            ("savepath", dir.to_str().unwrap()),
            ("engine", "hoard"),
            ("category", "films"),
            ("tags", "a,b"),
            ("paused", "true"),
            ("create_subfolder", "true"),
        ],
        Some(("torrents", &bytes)),
        true,
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "movie.bin");
    assert_eq!(v["engine"], "hoard");
    assert_eq!(v["mode"], "hoard");
    let hash = v["info_hash"].as_str().unwrap().to_string();
    assert_eq!(hash.len(), 40);
    assert!(held(&s, "race", &hash).is_none(), "not in the other engine");
    let t = held(&s, "hoard", &hash).expect("in the engine it named");
    assert_eq!(*t.save_path.read(), dir.join("movie"), "the subfolder");
    assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!t.seed_mode, "the data was not declared good");
    let store = s.store.lock().unwrap();
    assert_eq!(store.category_of(&hash, "hoard").unwrap(), "films");
}

/// The other spellings clients use for the same fields: `file`, `save_path`,
/// `stopped`, `skip_recheck`, and a `mode` that picks the engine by its role.
#[tokio::test]
async fn an_upload_takes_the_other_spellings_of_its_fields() {
    let s = st("upl-alias", "");
    let dir = s.dir.join("incoming");
    let bytes = torrent_bytes("alias.bin");
    let (code, v) = upload(
        &s,
        &[
            ("save_path", dir.to_str().unwrap()),
            ("mode", "Race"),
            ("stopped", "1"),
            ("skip_recheck", "1"),
            ("create_subfolder", "0"),
        ],
        Some(("file", &bytes)),
        true,
    )
    .await;

    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["engine"], "race");
    assert_eq!(v["mode"], "race");
    let t = held(&s, "race", v["info_hash"].as_str().unwrap()).expect("held");
    assert_eq!(*t.save_path.read(), dir, "no subfolder was asked for");
    assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        t.seed_mode,
        "skip_recheck is the caller vouching for the data"
    );
}

/// Everything an upload can get wrong is refused with the reason, and nothing
/// is added: no file, an engine that does not exist, a mode that is neither,
/// bytes that are no torrent, a torrent already held, and a missing key.
#[tokio::test]
async fn an_upload_that_cannot_be_added_says_why_and_adds_nothing() {
    let s = st("upl-refused", "");
    let dir = s.dir.join("incoming");
    let d = dir.to_str().unwrap();
    let bytes = torrent_bytes("refused.bin");
    let held_count = || manager(&s, "race").all().len() + manager(&s, "hoard").all().len();

    let (code, v) = upload(&s, &[("savepath", d)], None, true).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "no torrent file in request");

    let (code, v) = upload(
        &s,
        &[("savepath", d), ("engine", "nowhere")],
        Some(("torrents", &bytes)),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "no engine nowhere");

    let (code, v) = upload(
        &s,
        &[("savepath", d), ("mode", "cold")],
        Some(("torrents", &bytes)),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "mode must be race or hoard, not \"cold\"");

    let (code, v) = upload(
        &s,
        &[("savepath", d)],
        Some(("torrents", b"this is not a torrent")),
        true,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(
        v["error"].as_str().unwrap().contains("did not parse"),
        "{v}"
    );

    let (code, v) = upload(&s, &[("savepath", d)], Some(("torrents", &bytes)), false).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(held_count(), 0, "nothing was added by any of the above");

    let (code, v) = upload(&s, &[("savepath", d)], Some(("torrents", &bytes)), true).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    let (code, v) = upload(&s, &[("savepath", d)], Some(("torrents", &bytes)), true).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("already added"),
        "{v}"
    );
    assert_eq!(held_count(), 1, "the second upload added nothing");

    // No folder anywhere: neither a savepath nor a category that has one.
    let other = torrent_bytes("nowhere.bin");
    let (code, v) = upload(&s, &[], Some(("torrents", &other)), true).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(
        v["error"],
        "no save path: category \"\" is unknown and no savepath was given"
    );
    assert_eq!(held_count(), 1, "still only the one");
}

// ---------------------------------------------------------------------------
// What a category rename carries
// ---------------------------------------------------------------------------

fn workflow(
    id: &str,
    when: serde_json::Value,
    then: serde_json::Value,
) -> crate::store::StoredWorkflow {
    let body = serde_json::json!({
        "id": id, "name": id, "enabled": false, "when": when, "then": then,
    });
    crate::store::StoredWorkflow {
        id: id.into(),
        name: id.into(),
        body: body.to_string(),
        enabled: false,
        position: 0,
        interval_secs: 0,
        last_run: 0,
    }
}

fn magnet(hash: &str, category: &str) -> crate::store::MagnetRow {
    crate::store::MagnetRow {
        info_hash: hash.into(),
        uri: format!("magnet:?xt=urn:btih:{hash}"),
        name: hash.into(),
        engine: "race".into(),
        category: category.into(),
        save_path: String::new(),
        tags: String::new(),
        paused: false,
        seed_mode: false,
        options: String::new(),
        added_at: 1,
        attempts: 0,
        next_try: 0,
        state: "resolving".into(),
        error: String::new(),
    }
}

fn cond(field: &str, op: &str, value: &str) -> serde_json::Value {
    serde_json::json!({"kind": "cond", "field": field, "op": op, "value": value})
}

/// ⭐ Every place that names a category by its name follows the rename, and
/// only where it names it: an equality on the category (any case) in a
/// workflow, however deep, and a `set_category` to it; a `contains` is the
/// operator's text and a condition on another field is not a category. A job
/// already running keeps its params, and what names another category stays.
#[test]
fn a_rename_is_followed_where_a_category_is_named_and_nowhere_else() {
    let s = state_from("rename-refs", "");
    let (hash_a, hash_b, hash_c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
    let (hash_d, hash_e) = ("d".repeat(40), "e".repeat(40));
    {
        let store = s.store.lock().unwrap();
        store
            .rss_rule_put(
                "r-films",
                r#"{"enabled":true,"mustContain":"x","assignedCategory":"films"}"#,
            )
            .unwrap();
        store
            .rss_rule_put(
                "r-tv",
                r#"{"enabled":true,"mustContain":"y","assignedCategory":"tv"}"#,
            )
            .unwrap();
        store
            .put_setting(
                "watch_folders",
                r#"[{"path":"/w1","category":"films"},{"path":"/w2","category":"tv"},{"path":"/w3","category":"films"}]"#,
            )
            .unwrap();
        store
            .put_workflow(&workflow(
                "deep",
                serde_json::json!({"kind": "all", "of": [
                    cond("category", "eq", "Films"),
                    {"kind": "not", "of": cond("category", "ne", "films")},
                    {"kind": "any", "of": [
                        cond("category", "contains", "films"),
                        cond("name", "eq", "films"),
                        cond("category", "eq", "tv"),
                    ]},
                ]}),
                serde_json::json!([
                    {"type": "set_category", "to": "films"},
                    {"type": "set_category", "to": "tv"},
                ]),
            ))
            .unwrap();
        store
            .put_workflow(&workflow(
                "elsewhere",
                cond("category", "eq", "tv"),
                serde_json::json!([{"type": "pause"}]),
            ))
            .unwrap();
        store.put_magnet(&magnet(&hash_a, "films")).unwrap();
        store.put_magnet(&magnet(&hash_b, "tv")).unwrap();
        // The first job created is the one claimed: running, so left alone.
        store
            .create_job("move_data", &hash_c, r#"{"category":"films"}"#, 1)
            .unwrap();
        store.claim_next_job().expect("claimed");
        store
            .create_job(
                "move_data",
                &hash_d,
                r#"{"category":"films","to_category":"tv","save_path":"/x"}"#,
                1,
            )
            .unwrap();
        store
            .create_job("graduate", &hash_e, r#"{"to_category":"films"}"#, 1)
            .unwrap();
    }

    let v = rename_category_references(&s.state, "films", "movies").expect("followed");
    assert_eq!(
        v,
        serde_json::json!({"rss_rules": 1, "watch_folders": 2, "workflows": 1, "magnets": 1, "jobs": 2})
    );

    let store = s.store.lock().unwrap();
    let rule = |name: &str| {
        store
            .rss_rules()
            .unwrap()
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap()
            .1
    };
    assert!(rule("r-films").contains("movies") && !rule("r-films").contains("films"));
    assert!(rule("r-tv").contains("\"tv\""));
    let folders: Vec<(String, String)> = serde_json::from_str::<Vec<crate::watch::Folder>>(
        &store.setting("watch_folders").unwrap().unwrap(),
    )
    .unwrap()
    .into_iter()
    .map(|f| (f.path, f.category))
    .collect();
    assert_eq!(
        folders,
        vec![
            ("/w1".to_string(), "movies".to_string()),
            ("/w2".to_string(), "tv".to_string()),
            ("/w3".to_string(), "movies".to_string()),
        ]
    );
    let body = |id: &str| -> serde_json::Value {
        let w = store
            .workflows()
            .unwrap()
            .into_iter()
            .find(|w| w.id == id)
            .unwrap();
        serde_json::from_str(&w.body).unwrap()
    };
    let deep = body("deep");
    assert_eq!(
        deep["when"]["of"][0]["value"], "movies",
        "an equality, any case"
    );
    assert_eq!(
        deep["when"]["of"][1]["of"]["value"], "movies",
        "under a not"
    );
    assert_eq!(
        deep["when"]["of"][2]["of"][0]["value"], "films",
        "a contains is text"
    );
    assert_eq!(
        deep["when"]["of"][2]["of"][1]["value"], "films",
        "another field"
    );
    assert_eq!(deep["when"]["of"][2]["of"][2]["value"], "tv");
    assert_eq!(deep["then"][0]["to"], "movies");
    assert_eq!(deep["then"][1]["to"], "tv");
    assert_eq!(body("elsewhere")["when"]["value"], "tv");
    let cat_of = |h: &str| {
        store
            .magnets()
            .unwrap()
            .into_iter()
            .find(|m| m.info_hash == h)
            .unwrap()
            .category
    };
    assert_eq!(cat_of(&hash_a), "movies");
    assert_eq!(cat_of(&hash_b), "tv");
    let params = |h: &str| {
        store
            .list_jobs(10)
            .unwrap()
            .into_iter()
            .find(|j| j.info_hash == h)
            .unwrap()
            .params
    };
    assert_eq!(
        params(&hash_c),
        r#"{"category":"films"}"#,
        "running: untouched"
    );
    let queued: serde_json::Value = serde_json::from_str(&params(&hash_d)).unwrap();
    assert_eq!(queued["category"], "movies");
    assert_eq!(queued["to_category"], "tv");
    assert_eq!(queued["save_path"], "/x");
    let graduating: serde_json::Value = serde_json::from_str(&params(&hash_e)).unwrap();
    assert_eq!(graduating["to_category"], "movies");
}

/// A store that refuses one kind of reference does not stop the others from
/// being followed, and what it refused is named in the answer.
#[test]
fn a_reference_the_store_refuses_is_named_and_the_others_are_still_followed() {
    let s = state_from("rename-refused", "");
    {
        let store = s.store.lock().unwrap();
        store
            .rss_rule_put(
                "r-films",
                r#"{"enabled":true,"mustContain":"x","assignedCategory":"films"}"#,
            )
            .unwrap();
        store.execute_batch_for_tests("DROP TABLE magnets;");
    }

    let err = rename_category_references(&s.state, "films", "movies")
        .expect_err("the magnets could not be read");
    assert!(err.starts_with("magnets: "), "{err}");
    assert!(!err.contains(';'), "only the one refusal: {err}");

    let rules = s.store.lock().unwrap().rss_rules().unwrap();
    assert!(
        rules[0].1.contains("movies"),
        "the rule was followed all the same: {}",
        rules[0].1
    );
}
