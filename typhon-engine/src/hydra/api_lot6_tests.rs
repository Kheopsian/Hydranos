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

/// ⭐ Each switch of an upload means what it says in both directions: `true`
/// and `1` turn it on, anything else (`false`, `0`, a word) leaves it off, and
/// the canonical names work as well as the aliases.
#[tokio::test]
async fn an_upload_switch_is_on_for_true_and_one_and_off_for_anything_else() {
    let s = st("upl-switches", "");
    let dir = s.dir.join("incoming");
    let d = dir.to_str().unwrap();
    for (name, fields, paused, seed) in [
        (
            "sw1.bin",
            vec![("paused", "true"), ("seed_mode", "true")],
            true,
            true,
        ),
        (
            "sw2.bin",
            vec![("stopped", "1"), ("skip_checking", "1")],
            true,
            true,
        ),
        (
            "sw3.bin",
            vec![("paused", "false"), ("seed_mode", "false")],
            false,
            false,
        ),
        (
            "sw4.bin",
            vec![("stopped", "0"), ("skip_checking", "0")],
            false,
            false,
        ),
        (
            "sw5.bin",
            vec![("paused", "yes"), ("skip_recheck", "on")],
            false,
            false,
        ),
    ] {
        let bytes = torrent_bytes(name);
        let mut all = vec![
            ("savepath", d),
            ("engine", "hoard"),
            ("create_subfolder", "0"),
        ];
        all.extend(fields.iter().copied());
        let (code, v) = upload(&s, &all, Some(("torrents", &bytes)), true).await;
        assert_eq!(code, StatusCode::OK, "{name}: {v}");
        let t = held(&s, "hoard", v["info_hash"].as_str().unwrap()).expect("held");
        assert_eq!(
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
            paused,
            "{name} paused"
        );
        assert_eq!(t.seed_mode, seed, "{name} seed mode");
    }
    // The subfolder switch: on for true/1, off for false/anything else.
    for (name, value, folder) in [
        ("sf1.bin", "true", "sf1"),
        ("sf2.bin", "1", "sf2"),
        ("sf3.bin", "false", "sf3.bin"),
        ("sf4.bin", "no", "sf4.bin"),
    ] {
        let bytes = torrent_bytes(name);
        let fields = [
            ("savepath", d),
            ("engine", "hoard"),
            ("create_subfolder", value),
        ];
        let (code, v) = upload(&s, &fields, Some(("torrents", &bytes)), true).await;
        assert_eq!(code, StatusCode::OK, "{name}: {v}");
        let t = held(&s, "hoard", v["info_hash"].as_str().unwrap()).expect("held");
        let want = if folder == name {
            dir.clone()
        } else {
            dir.join(folder)
        };
        assert_eq!(
            *t.save_path.read(),
            want,
            "{name}: create_subfolder={value}"
        );
    }
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

/// ⭐ Which engine an upload's `engine` and `mode` name: an engine by its id
/// first, a mode by the engine called after it, and only then by any engine
/// that has the role. A node that renamed its roles gets the engine whose role
/// it is, never one that merely carries the name.
#[test]
fn a_mode_names_the_engine_called_after_it_then_any_engine_with_the_role() {
    let engine = |s: &TestState, id: &str, mode: &str| add_target_engine(&s.state, id, mode);

    let s = st("mode-default", "");
    assert_eq!(engine(&s, "", "race"), Ok("race".into()));
    assert_eq!(engine(&s, "", "hoard"), Ok("hoard".into()));
    assert_eq!(
        engine(&s, "", " HoArD "),
        Ok("hoard".into()),
        "trimmed, any case"
    );
    assert_eq!(
        engine(&s, "", ""),
        Ok(String::new()),
        "the category decides"
    );
    assert_eq!(
        engine(&s, "", "cold"),
        Err("mode must be race or hoard, not \"cold\"".into())
    );
    assert_eq!(
        engine(&s, " hoard ", "race"),
        Ok("hoard".into()),
        "an id wins over a mode"
    );
    assert_eq!(engine(&s, "ghost", "race"), Err("no engine ghost".into()));

    let extra = st(
        "mode-extra",
        "\n[[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26981\n",
    );
    assert_eq!(engine(&extra, "", "race"), Ok("race".into()), "not vpn1");
    assert_eq!(engine(&extra, "vpn1", ""), Ok("vpn1".into()));

    // `race` is now a hoard-role engine: nothing is called after the race role,
    // and the one engine that has it is named by its role.
    let swapped = st(
        "mode-swapped",
        "\n[[engine]]\nengine_id = \"race\"\nrole = \"hoard\"\n[engine.session]\nlisten_port = 26982\n",
    );
    assert_eq!(
        engine(&swapped, "", "hoard"),
        Ok("hoard".into()),
        "the one called hoard"
    );
    assert_eq!(
        engine(&swapped, "", "race"),
        Err("no engine has the race role".into()),
        "an engine named race that is not one"
    );
    let fallback = st(
        "mode-fallback",
        "\n[[engine]]\nengine_id = \"race\"\nrole = \"hoard\"\n[engine.session]\nlisten_port = 26983\n\
         [[engine]]\nengine_id = \"vpn9\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26984\n",
    );
    assert_eq!(
        engine(&fallback, "", "race"),
        Ok("vpn9".into()),
        "by its role"
    );
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

/// ⭐ A workflow whose only mention of the category is the category it files
/// into is followed, and counted; watched folders that name no such category
/// are not rewritten at all (not even into the same list in another spelling),
/// and a rename that finds nothing says so with zeros.
#[test]
fn a_rename_counts_a_workflow_that_only_files_into_the_category_and_rewrites_nothing_else() {
    let s = state_from("rename-action-only", "");
    let folders = r#"[{"path":"/w","category":"tv"}]"#;
    {
        let store = s.store.lock().unwrap();
        store.put_setting("watch_folders", folders).unwrap();
        store
            .put_workflow(&workflow(
                "act",
                cond("name", "eq", "x"),
                serde_json::json!([{"type": "set_category", "to": "films"}, {"type": "pause"}]),
            ))
            .unwrap();
    }

    let v = rename_category_references(&s.state, "films", "movies").expect("followed");

    assert_eq!(
        v,
        serde_json::json!({"rss_rules": 0, "watch_folders": 0, "workflows": 1, "magnets": 0, "jobs": 0})
    );
    let store = s.store.lock().unwrap();
    assert_eq!(
        store.setting("watch_folders").unwrap().as_deref(),
        Some(folders),
        "untouched, byte for byte"
    );
    let w: serde_json::Value = serde_json::from_str(&store.workflows().unwrap()[0].body).unwrap();
    assert_eq!(w["then"][0]["to"], "movies");
    assert_eq!(w["then"][1]["type"], "pause");
    drop(store);

    let v = rename_category_references(&s.state, "nothing-like-it", "x").expect("nothing to do");
    assert_eq!(
        v,
        serde_json::json!({"rss_rules": 0, "watch_folders": 0, "workflows": 0, "magnets": 0, "jobs": 0})
    );
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

// ---------------------------------------------------------------------------
// Graduating a torrent by hand
// ---------------------------------------------------------------------------

async fn graduate(s: &TestState, hash: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let resp = post_torrent_graduate(
        State(s.state.clone()),
        Path(hash.to_string()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

/// ⭐ The route queues a move of the DATA to another engine, and says no, with
/// the reason, to everything that is not one: nothing is queued by any of them.
#[tokio::test]
async fn a_graduation_asked_for_wrongly_is_refused_and_queues_nothing() {
    let s = st("grad-refused", "");
    let (hash, file) = seed(&s, "race", "ga", "", 100);
    let pool = s.dir.join("pool");

    let (code, v) = graduate(&s, &"0".repeat(40), r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
    for (body, error) in [
        ("{}", "engine is required"),
        (r#"{"engine":"race"}"#, "already in race"),
        (r#"{"engine":"nope"}"#, "no engine named nope"),
        (
            r#"{"engine":"hoard"}"#,
            "save_path is required (or a category that has one)",
        ),
        (
            r#"{"engine":"hoard","category":"unknown"}"#,
            "save_path is required (or a category that has one)",
        ),
        ("not json at all", "engine is required"),
    ] {
        let (code, v) = graduate(&s, &hash, body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert_eq!(v["error"], error, "{body}");
    }

    // Files already at the destination are never moved onto.
    std::fs::create_dir_all(&pool).unwrap();
    std::fs::write(pool.join("ga"), b"somebody else's").unwrap();
    let body = serde_json::json!({"engine": "hoard", "save_path": pool.to_str().unwrap()});
    let (code, v) = graduate(&s, &hash, &body.to_string()).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["reason"], "destination_taken");

    assert!(s.store.lock().unwrap().list_jobs(10).unwrap().is_empty());
    assert!(file.exists() && in_store(&s, &hash));
    assert_eq!(std::fs::read(pool.join("ga")).unwrap(), b"somebody else's");
}

/// The destination is the one named, or else the category's folder; a
/// second ask while the first is queued is refused.
#[tokio::test]
async fn a_graduation_is_queued_to_the_folder_named_or_the_categorys() {
    let s = st("grad-queued", "");
    let (named, _) = seed(&s, "race", "gb", "", 100);
    let (by_category, _) = seed(&s, "race", "gc", "", 100);
    let (pool, elsewhere) = (s.dir.join("pool"), s.dir.join("elsewhere"));
    s.store
        .lock()
        .unwrap()
        .put_meta(
            "categories",
            &serde_json::json!({
                "pool": {"save_path": pool.to_str().unwrap(), "mode": "hoard"},
            })
            .to_string(),
        )
        .unwrap();

    let body = serde_json::json!({
        "engine": "hoard", "category": "pool", "save_path": elsewhere.to_str().unwrap(),
    });
    let (code, v) = graduate(&s, &named, &body.to_string()).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "queued");
    assert_eq!(v["plan"]["to"], elsewhere.to_str().unwrap(), "{v}");
    let job_named = v["job"].as_str().unwrap().to_string();

    let (code, v) = graduate(&s, &named, &body.to_string()).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        v["error"],
        "a move is already queued or running for this torrent"
    );

    let (code, v) = graduate(&s, &by_category, r#"{"engine":"hoard","category":"pool"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    let job_category = v["job"].as_str().unwrap().to_string();

    let jobs = s.store.lock().unwrap().list_jobs(10).unwrap();
    assert_eq!(jobs.len(), 2);
    let params = |h: &str, answered: &str| -> serde_json::Value {
        let j = jobs.iter().find(|j| j.info_hash == h).unwrap();
        assert_eq!(j.kind, "graduate");
        assert_eq!(j.id, answered, "the answer names the job it queued");
        serde_json::from_str(&j.params).unwrap()
    };
    let a = params(&named, &job_named);
    assert_eq!(a["save_path"], elsewhere.to_str().unwrap());
    assert_eq!(a["to_engine"], "hoard");
    assert_eq!(a["from_engine"], "race");
    assert_eq!(a["to_category"], "pool");
    let b = params(&by_category, &job_category);
    assert_eq!(
        b["save_path"],
        pool.to_str().unwrap(),
        "the category's folder"
    );
    assert_eq!(b["to_category"], "pool");
}

/// ⭐ A graduation across filesystems copies the data, so the target needs
/// room for exactly those bytes: one byte short is refused with the reason,
/// exactly enough is queued.
#[tokio::test]
async fn a_graduation_needs_room_for_exactly_the_bytes_it_copies() {
    struct Clean(std::path::PathBuf);
    impl Drop for Clean {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
            crate::jobs::FREE_SPACE.with(|c| c.set(None));
        }
    }
    let s = st("grad-room", "");
    let shm = std::path::Path::new("/dev/shm");
    if !shm.exists() || crate::jobs::same_filesystem(shm, &s.dir) {
        eprintln!("skipped: /dev/shm is not a separate filesystem here");
        return;
    }
    let src = shm.join(format!("hydra-grad-room-{}", std::process::id()));
    let _clean = Clean(src.clone());
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("gd"), vec![0u8; SIZE as usize]).unwrap();
    let (hash, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("gd"),
        "",
        src.to_str().unwrap(),
        "",
        true,
        true,
        "race",
    )
    .expect("added");
    let body =
        serde_json::json!({"engine": "hoard", "save_path": s.dir.join("pool").to_str().unwrap()})
            .to_string();

    crate::jobs::FREE_SPACE.with(|c| c.set(Some(SIZE - 1)));
    let (code, v) = graduate(&s, &hash, &body).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["reason"], "space");
    assert_eq!(v["error"], "not enough free space at the target");
    assert!(s.store.lock().unwrap().list_jobs(10).unwrap().is_empty());

    crate::jobs::FREE_SPACE.with(|c| c.set(Some(SIZE)));
    let (code, v) = graduate(&s, &hash, &body).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["plan"]["copy_bytes"], SIZE);
    assert_eq!(s.store.lock().unwrap().list_jobs(10).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// The reannounce button
// ---------------------------------------------------------------------------

use crate::announce::scheduler::{BumpOutcome, BumpReq};

async fn reannounce(s: &TestState, hash: &str) -> (StatusCode, serde_json::Value) {
    let resp = reannounce_one(
        State(s.state.clone()),
        Path(hash.to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

/// What the scheduler would answer: one reply per request, as scripted, and
/// the requests it was handed.
fn scheduler_answering(
    s: &TestState,
    replies: Vec<BumpOutcome>,
) -> std::sync::Arc<std::sync::Mutex<Vec<(String, bool)>>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BumpReq>(8);
    s.engines.get("hoard").unwrap().bump.set(tx).ok().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut replies = replies.into_iter();
        while let Some(req) = rx.recv().await {
            log.lock()
                .unwrap()
                .push((req.info_hash.clone(), req.forced));
            if let (Some(o), Some(reply)) = (replies.next(), req.reply) {
                let _ = reply.send(o);
            }
        }
    });
    seen
}

/// ⭐ What the button says is what the scheduler said, not that the message
/// was sent: bumped, already announcing, in cooldown (with the wait, rounded
/// up), and not on the network at all.
#[tokio::test]
async fn the_reannounce_button_says_what_the_scheduler_answered() {
    let s = st("reann", "");
    let (hash, _) = seed(&s, "hoard", "ra", "", 0);
    let upper = hash.to_uppercase();

    let (code, v) = reannounce(&s, &"0".repeat(40)).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "engine is not announcing");

    let seen = scheduler_answering(
        &s,
        vec![
            BumpOutcome::Bumped,
            BumpOutcome::InFlight,
            BumpOutcome::Cooldown {
                retry_in: std::time::Duration::from_secs(30),
            },
        ],
    );
    let (code, v) = reannounce(&s, &upper).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        serde_json::json!({"status": "ok", "engine": "hoard", "bumped": true})
    );
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        serde_json::json!({"status": "in_flight", "engine": "hoard", "bumped": false})
    );
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(
        v,
        serde_json::json!({
            "status": "cooldown", "engine": "hoard", "bumped": false, "retry_after_secs": 31
        })
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            (hash.clone(), true),
            (hash.clone(), true),
            (hash.clone(), true)
        ],
        "a hand-pressed announce is forced, and the hash is lower case"
    );
}

/// A scheduler that does not answer in time is "queued", one that went away
/// is "not announcing", and a queue that is full refuses the click.
#[tokio::test(start_paused = true)]
async fn a_reannounce_nobody_answered_is_queued_dropped_or_refused() {
    let s = st("reann-silent", "");
    let (hash, _) = seed(&s, "hoard", "rb", "", 0);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<BumpReq>(1);
    s.engines.get("hoard").unwrap().bump.set(tx).ok().unwrap();

    // Nobody reads: the first fills the queue's one place and waits out the
    // five seconds, the second finds it full.
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{v}");
    assert_eq!(
        v,
        serde_json::json!({"status": "queued", "engine": "hoard", "bumped": false})
    );
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"], "announce queue is busy, try again");

    // The scheduler wakes up and drops every request without a word.
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    tokio::task::yield_now().await;
    let (code, v) = reannounce(&s, &hash).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "engine is not announcing");
}

// ---------------------------------------------------------------------------
// The add that the race disk cannot hold
// ---------------------------------------------------------------------------

/// ⭐ Refused BEFORE anything is written, with the numbers, when what the disk
/// has left after what is promised and the reserve is less than the torrent;
/// only a race engine is guarded, and only when the operator switched it on.
#[test]
fn a_race_the_disk_cannot_hold_is_refused_before_a_file_is_written() {
    let full = "\n[race_drain]\nadd_block_enabled = true\nreserve_free_gb = 100000000\n";
    let add = |s: &TestState, name: &str, engine: &str| {
        let dir = s.dir.join("incoming");
        add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            dir.to_str().unwrap(),
            "",
            true,
            true,
            engine,
        )
    };

    let s = st("admit-full", full);
    let err = add(&s, "aa", "race").expect_err("the reserve is more than any disk");
    assert!(err.starts_with("race disk full: "), "{err}");
    assert!(err.contains(", 100000000 GB reserved; "), "{err}");
    assert!(err.ends_with("this torrent needs 0 GB"), "{err}");
    assert!(manager(&s, "race").all().is_empty(), "nothing was added");
    assert!(
        s.store
            .read()
            .unwrap()
            .all_hashes("race")
            .unwrap()
            .is_empty(),
        "nor stored"
    );
    add(&s, "ab", "hoard").expect("the hoard is not the race disk's business");

    let off = st("admit-off", "\n[race_drain]\nreserve_free_gb = 100000000\n");
    add(&off, "ac", "race").expect("switched off: no refusal");
    let roomy = st("admit-roomy", "\n[race_drain]\nadd_block_enabled = true\n");
    add(&roomy, "ad", "race").expect("no reserve: the disk holds 16 KiB");
}

/// ⭐ What is already promised to downloads in flight counts against the room:
/// with a quota of exactly two torrents' worth, a second race is admitted; one
/// byte less and it is refused, because the first has not written its bytes
/// yet.
#[test]
fn what_is_promised_to_downloads_in_flight_counts_against_the_room() {
    let add_race = |s: &TestState, name: &str| {
        let dir = s.dir.join("incoming");
        add_torrent_bytes(
            &s.state,
            &torrent_bytes(name),
            "",
            dir.to_str().unwrap(),
            "",
            true,
            false,
            "race",
        )
    };
    for (quota, admitted) in [(2 * SIZE, true), (2 * SIZE - 1, false), (3 * SIZE, true)] {
        let s = st(
            &format!("admit-promised-{quota}"),
            "\n[race_drain]\nadd_block_enabled = true\n",
        );
        crate::volumes::save_quota(&s.state, &mount(&s), Some(quota)).unwrap();
        add_race(&s, "first").expect("the first has the whole quota to itself");
        assert_eq!(
            manager(&s, "race").all()[0].meta.total_size,
            SIZE,
            "and it has written nothing yet"
        );
        let second = add_race(&s, "second");
        assert_eq!(second.is_ok(), admitted, "quota {quota}: {second:?}");
        if let Err(e) = second {
            assert!(e.starts_with("race disk full: "), "{e}");
        }
        assert_eq!(
            manager(&s, "race").all().len(),
            if admitted { 2 } else { 1 }
        );
    }
}
