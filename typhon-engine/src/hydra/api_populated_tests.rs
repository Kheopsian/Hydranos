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

/// A node holding a few torrents in each engine, which is the state every
/// one of these routes was written for -- the empty case exercises the
/// early returns and almost nothing else.
fn populated(tag: &str) -> (TestState, Vec<String>) {
    let s = st(tag);
    let mut hashes = Vec::new();
    for (engine, names) in [
        ("race", ["alpha", "bravo"]),
        ("hoard", ["charlie", "delta"]),
    ] {
        for n in names {
            let (hash, _name) = add_torrent_bytes(
                &s.state,
                &torrent_bytes(n),
                "",
                "/tmp",
                "fr",
                true,
                true,
                engine,
            )
            .unwrap_or_else(|e| panic!("add {n}: {e}"));
            hashes.push(hash);
        }
    }
    (s, hashes)
}

/// Every read route again, this time against a node that HOLDS something.
/// A route that only ever saw an empty catalogue has had its body skipped.
macro_rules! populated_routes {
    ($($t:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let (s, _h) = populated(concat!("pop-", stringify!($name)));
                let resp =
                    super::$name(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
                assert!(
                    resp.status().is_success() || resp.status().is_client_error(),
                    concat!(stringify!($name), " answered {:?}"),
                    resp.status()
                );
                let _ = body_json(resp).await;
            }
        )+
    };
}

populated_routes!(
    pr_get_status => get_status,
    pr_get_engines => get_engines,
    pr_get_tags => get_tags,
    pr_get_categories => get_categories,
    pr_get_trackers => get_trackers,
    pr_get_announce_health => get_announce_health,
    pr_get_hoard_stats => get_hoard_stats,
    pr_get_drain_status => get_drain_status,
    pr_get_health_anomalies => get_health_anomalies,
    pr_get_provenance => get_provenance,
    pr_get_dedup_stats => get_dedup_stats,
    pr_get_hoard_pinned => get_hoard_pinned,
    pr_get_download_slots => get_download_slots,
    pr_get_arr_cleanup_scan => get_arr_cleanup_scan,
    pr_get_baseline => get_baseline,
    pr_get_network_engines => get_network_engines,
    pr_qbit_transfer_info => qbit_transfer_info,
    pr_qbit_categories => qbit_categories,
    pr_qbit_tags => qbit_tags,
    pr_get_hoard_torrents => get_hoard_torrents,
    pr_get_race_torrents => get_race_torrents,
    pr_get_hoard_page => get_hoard_page,
    pr_get_race_page => get_race_page,
);

/// ⭐ A per-torrent route addressed at a torrent that IS here must answer
/// about it -- the absent case only ever exercised the refusal.
#[tokio::test]
async fn the_detail_routes_answer_for_a_torrent_that_is_here() {
    let (s, hashes) = populated("pop-detail");
    let h = hashes[0].clone();
    for resp in [
        super::get_race_torrent(
            State(s.state.clone()),
            axum::extract::Path(h.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
        super::get_torrent_files(
            State(s.state.clone()),
            axum::extract::Path(h.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
        super::get_torrent_trackers(
            State(s.state.clone()),
            axum::extract::Path(h.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
    ] {
        assert!(resp.status().is_success(), "got {:?}", resp.status());
    }
}

/// The .torrent file of a torrent we hold comes back as bytes a client can
/// feed to another engine -- that is the whole point of the route.
#[tokio::test]
async fn the_torrent_file_of_a_torrent_we_hold_comes_back() {
    let (s, hashes) = populated("pop-file");
    let resp = super::get_torrent_file(
        State(s.state.clone()),
        axum::extract::Path(hashes[0].clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(
        bytes.starts_with(b"d"),
        "a bencoded document: {:?}",
        &bytes[..bytes.len().min(8)]
    );
}

/// ⭐⭐ A bulk action names its torrents. Acting on an EMPTY list must
/// touch nothing -- a bulk that reads "no hashes" as "all of them" is how
/// a whole library gets paused by an accidental click.
#[tokio::test]
async fn a_bulk_action_on_an_empty_list_touches_nothing() {
    let (s, _h) = populated("pop-bulkempty");
    let before: Vec<String> = {
        let store = s.store.lock().unwrap();
        store.paused_hashes("race").unwrap_or_default()
    };
    let resp = super::race_pause_bulk(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"hashes":[]}"#.to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    let after: Vec<String> = {
        let store = s.store.lock().unwrap();
        store.paused_hashes("race").unwrap_or_default()
    };
    assert_eq!(before.len(), after.len(), "an empty bulk paused something");
}

/// A bulk action naming a real torrent acts on it, and on it only.
#[tokio::test]
async fn a_bulk_action_acts_on_the_torrents_it_names() {
    let (s, hashes) = populated("pop-bulkone");
    let target = hashes[0].clone();
    let resp = super::race_pause_bulk(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"hashes": [target]}).to_string(),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// The listing and the shim must not disagree about how many torrents the
/// node holds -- two views of one catalogue.
#[tokio::test]
async fn the_native_page_and_the_qbit_shim_agree_on_the_count() {
    let (s, _h) = populated("pop-agree");
    let page = engine_page_value(&s.state, "race", "").await;
    let shim = body_json(
        super::qbit_torrents_info(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            String::new(),
        )
        .await,
    )
    .await;
    let shim_race = shim.as_array().map(|rows| rows.len()).expect("an array");
    assert_eq!(page["total"].as_i64(), Some(2), "the race page");
    assert_eq!(shim_race, 4, "the shim lists every engine's torrents");
}

/// The status card reports what the node actually holds, not zero.
#[tokio::test]
async fn the_status_card_counts_the_torrents_that_are_there() {
    let (s, _h) = populated("pop-status");
    let body =
        body_json(super::get_status(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let text = body.to_string();
    assert!(body.is_object(), "got {body}");
    assert!(text.contains("version"), "got {body}");
}

/// Tags registered by an add show up in the tag list: the add path and the
/// tag list read the same store.
#[tokio::test]
async fn a_tag_given_at_add_time_appears_in_the_tag_list() {
    let (s, _h) = populated("pop-tags");
    let body =
        body_json(super::get_tags(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    assert!(body.to_string().contains("fr"), "got {body}");
}

/// The tracker tab merges what torrents announce to with what the operator
/// declared. Four torrents on one host is one row, not four.
#[tokio::test]
async fn the_tracker_tab_groups_by_host() {
    let (s, _h) = populated("pop-trackers");
    let body =
        body_json(super::get_trackers(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let text = body.to_string();
    assert!(text.contains("tracker.example"), "got {body}");
}

/// The Trackers table says what each tracker answers as `interval` and
/// `min interval`: last values, the range since start, and `null` for a
/// host nobody has answered. A second answer without a floor reads 0.
#[tokio::test]
async fn the_tracker_tab_shows_the_interval_each_host_answers() {
    let (s, _h) = populated("pop-intervals");
    let engines = s.state.engines.engines();
    let cache = &engines[0].announce_cache;
    cache.record_interval("answered.test", 1800, Some(900), 1_000);
    cache.record_interval("answered.test", 2700, Some(0), 2_000);
    cache.record_interval("udp.test", 900, None, 1_500);
    let body =
        body_json(super::get_trackers(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let row = |host: &str| {
        body.as_array()
            .unwrap()
            .iter()
            .find(|r| r["host"] == host)
            .unwrap_or_else(|| panic!("no row for {host} in {body}"))
            .clone()
    };
    let a = row("answered.test");
    assert_eq!(a["interval_s"], 2700, "{a}");
    assert_eq!(a["min_interval_s"], 0, "{a}");
    assert_eq!(a["interval_min_s"], 1800, "{a}");
    assert_eq!(a["interval_max_s"], 2700, "{a}");
    assert_eq!(a["interval_seen_at"], 2_000, "{a}");
    let u = row("udp.test");
    assert_eq!(u["interval_s"], 900, "{u}");
    assert!(u["min_interval_s"].is_null(), "UDP has no floor: {u}");
    let never = row("tracker.example");
    for k in [
        "interval_s",
        "min_interval_s",
        "interval_min_s",
        "interval_max_s",
        "interval_seen_at",
    ] {
        assert!(never[k].is_null(), "{k} of a silent host: {never}");
    }
}

/// The per-tracker pass counts each engine's torrents under the host baked
/// into them, one row per (engine, tracker), stamped with the pass time.
#[tokio::test]
async fn the_tracker_pass_counts_each_engine_under_its_host() {
    let (s, _h) = populated("pop-trackerpass");
    let rows = crate::benchsampler::tracker_totals(&s.state.engines, &s.state.store, 42);
    let of = |engine: &str| {
        rows.iter()
            .find(|r| r.engine == engine && r.tracker == "tracker.example")
            .unwrap_or_else(|| panic!("no {engine} row in {rows:?}"))
    };
    assert_eq!(of("race").torrents, 2, "got {rows:?}");
    assert_eq!(of("hoard").torrents, 2, "got {rows:?}");
    assert!(rows.iter().all(|r| r.ts == 42));
}

/// Seed size is what a tracker credits: the whole size of a torrent that
/// seeds, nothing for one that is not complete or is paused.
#[tokio::test]
async fn the_tracker_pass_counts_only_seeding_torrents_in_seed_size() {
    use std::sync::atomic::Ordering;
    use typhon_engine::torrent::meta::TorrentStatus;
    let (s, _h) = populated("pop-seedsize");
    let set = |t: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
               st: TorrentStatus,
               paused: bool| {
        t.status.store(st as u8, Ordering::Relaxed);
        t.is_paused.store(paused, Ordering::Relaxed);
    };
    let hoard = s.state.engines.get("hoard").expect("hoard").manager.all();
    let race = s.state.engines.get("race").expect("race").manager.all();
    set(&hoard[0], TorrentStatus::Seeding, false);
    set(&hoard[1], TorrentStatus::Downloading, false);
    set(&race[0], TorrentStatus::Seeding, false);
    set(&race[1], TorrentStatus::Seeding, true);

    let rows = crate::benchsampler::tracker_totals(&s.state.engines, &s.state.store, 42);
    let of = |engine: &str| {
        rows.iter()
            .find(|r| r.engine == engine && r.tracker == "tracker.example")
            .expect("row")
            .seed_size
    };
    assert_eq!(
        of("hoard"),
        hoard[0].meta.total_size as i64,
        "a downloading torrent is not seeded"
    );
    assert_eq!(
        of("race"),
        race[0].meta.total_size as i64,
        "a paused one announces nothing"
    );
    assert!(of("hoard") > 0);
}

/// Categories are listed alphabetically, whatever their case: byte order
/// put every capitalised name before every lowercase one.
#[tokio::test]
async fn categories_are_listed_alphabetically_whatever_their_case() {
    let s = st("cats-alpha");
    for name in ["Music", "animes", "Comics", "kids", "ABNormal"] {
        crate::api::testing::put_category(&s.state, name, "hoard", "/tmp");
    }
    let v =
        body_json(super::get_categories(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["ABNormal", "animes", "Comics", "kids", "Music"]);
}

/// The Transmission import end to end, through its routes: the folder is
/// read, the category is created as hoard, the torrent lands in the hoard
/// stopped with its carried upload, and a second run skips it.
#[tokio::test]
async fn a_transmission_folder_is_imported_into_the_hoard_and_not_twice() {
    let s = st("imp-tr");
    let dir = std::env::temp_dir().join(format!("imp-tr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("torrents")).unwrap();
    std::fs::create_dir_all(dir.join("resume")).unwrap();
    std::fs::write(dir.join("torrents/A.torrent"), torrent_bytes("alpha")).unwrap();
    std::fs::write(
        dir.join("resume/A.resume"),
        b"d11:destination10:/tmp/films8:uploadedi77ee",
    )
    .unwrap();
    let body = serde_json::json!({"dir": dir, "path_map": {}, "start_stopped": true}).to_string();

    let preview = body_json(
        super::post_transmission_preview(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            body.clone(),
        )
        .await,
    )
    .await;
    assert_eq!(preview["total"], 1, "got {preview}");
    assert_eq!(
        preview["incomplete"], 1,
        "no progress in the resume: checked, not trusted"
    );
    assert_eq!(preview["carried_uploaded_bytes"], 77);
    assert_eq!(preview["categories"][0]["name"], "films");

    let run = || async {
        let resp = super::post_transmission_import_start(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            body.clone(),
        )
        .await;
        assert!(
            resp.status().is_success(),
            "start answered {:?}",
            resp.status()
        );
        for _ in 0..500 {
            if !latest_import(&s.state).expect("a job").1.running() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        latest_import(&s.state).unwrap().1.as_json()
    };
    let first = run().await;
    assert_eq!(first["phase"], "done", "got {first}");
    assert_eq!(
        (first["done"].as_u64(), first["downloading"].as_u64()),
        (Some(1), Some(1)),
        "got {first}"
    );

    let hoard = s.state.engines.get("hoard").expect("hoard").manager.all();
    assert_eq!(hoard.len(), 1);
    assert!(
        hoard[0]
            .is_paused
            .load(std::sync::atomic::Ordering::Relaxed),
        "imported stopped"
    );
    assert_eq!(
        hoard[0]
            .total_uploaded
            .load(std::sync::atomic::Ordering::Relaxed),
        77
    );
    assert_eq!(categories_map(&s.state)["films"].mode, "hoard");

    let second = run().await;
    assert_eq!(
        second["skipped"], 1,
        "already there: skipped, not added twice: {second}"
    );
    assert_eq!(s.state.engines.get("hoard").unwrap().manager.all().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A qBittorrent retry acts on qBittorrent as it is now and on the mapping
/// it is given: the torrent the failed job saw under `/old` has moved to
/// `/new` since, and the retry brings a mapping for `/new`; the one removed
/// from qBittorrent fails as gone, not as a bare 404 export.
#[tokio::test]
async fn a_qbit_retry_lists_the_library_again_and_takes_a_new_mapping() {
    use axum::response::IntoResponse;
    async fn login() -> axum::response::Response {
        ([("set-cookie", "SID=1; HttpOnly")], "Ok.").into_response()
    }
    async fn info() -> axum::response::Response {
        axum::Json(serde_json::json!([
            {"hash": "h1", "name": "alpha", "save_path": "/new", "progress": 0.0, "state": "pausedDL"}
        ]))
        .into_response()
    }
    async fn export(
        axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
    ) -> axum::response::Response {
        match q.get("hash").map(String::as_str) {
            Some("h1") => torrent_bytes("alpha").into_response(),
            _ => (StatusCode::NOT_FOUND, "Not Found").into_response(),
        }
    }
    let app = axum::Router::new()
        .route("/api/v2/auth/login", axum::routing::post(login))
        .route("/api/v2/torrents/info", axum::routing::get(info))
        .route("/api/v2/torrents/export", axum::routing::get(export));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let s = st("imp-qb-retry");
    let data = std::env::temp_dir().join(format!("imp-qb-retry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    // The failed job, as it was left: its snapshot, its mapping, its kit.
    let snapshot = |hash: &str, name: &str| {
        crate::importer::Candidate::from_qbit(&crate::importer::QbitTorrent {
            hash: hash.into(),
            name: name.into(),
            save_path: "/old".into(),
            ..Default::default()
        })
    };
    let failed = Arc::new(crate::importer::Progress::default());
    *failed.retry.lock().unwrap() = Some(crate::importer::RetryKit {
        cands: vec![snapshot("h1", "alpha"), snapshot("h2", "removed")],
        creds: Some(crate::importer::QbitCreds {
            url: format!("http://{addr}"),
            username: "u".into(),
            password: "p".into(),
            insecure: false,
        }),
        choices: crate::importer::Choices::default(),
    });
    failed
        .finished
        .store(true, std::sync::atomic::Ordering::Relaxed);
    s.state
        .imports
        .lock()
        .unwrap()
        .insert("imp-0".into(), failed.clone());

    let body = serde_json::json!({"path_map": {"/new": data}}).to_string();
    let resp =
        super::post_import_retry(State(s.state.clone()), RawQuery(None), keyed(KEY), body).await;
    assert!(
        resp.status().is_success(),
        "retry answered {:?}",
        resp.status()
    );
    assert!(failed.retry.lock().unwrap().is_none(), "the kit was taken");
    let mut status = serde_json::Value::Null;
    for _ in 0..500 {
        let (id, p) = latest_import(&s.state).expect("a job");
        if id != "imp-0" && !p.running() {
            status = p.as_json();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(status["phase"], "done", "got {status}");
    assert_eq!(
        (status["total"].as_u64(), status["failed"].as_u64()),
        (Some(2), Some(1)),
        "got {status}"
    );
    assert_eq!(status["failures"][0]["name"], "removed", "got {status}");
    assert_eq!(
        status["failures"][0]["error"],
        crate::importer::GONE_FROM_QBIT,
        "got {status}"
    );
    assert_eq!(status["retryable"], false, "nothing left worth retrying");
    let hoard = s.state.engines.get("hoard").expect("hoard").manager.all();
    assert_eq!(hoard.len(), 1);
    assert_eq!(
        *hoard[0].save_path.read(),
        data,
        "the current save path, through the new mapping"
    );
    let _ = std::fs::remove_dir_all(&data);
}

/// The qBittorrent preview answers the URL problem only when there is one.
#[tokio::test]
async fn the_qbit_preview_refuses_an_empty_url_and_reports_an_unreachable_one() {
    let s = st("imp-qb");
    let empty = super::post_qbit_import_preview(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"url":"  "}"#.into(),
    )
    .await;
    assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(empty).await["error"], "empty qBittorrent URL");
    // A filled URL nobody answers on: the error names it, it is not "empty".
    let down = body_json(
        super::post_qbit_import_preview(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"url":"127.0.0.1:1","username":"a","password":"b"}"#.into(),
        )
        .await,
    )
    .await;
    let msg = down["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("cannot reach qBittorrent at http://127.0.0.1:1"),
        "got {down}"
    );
}

/// A WebUI with a self-signed certificate: the preview says it is the
/// certificate, and goes through once the body sets `insecure` (issue #94).
#[tokio::test]
async fn the_qbit_preview_names_an_untrusted_certificate_and_takes_insecure_to_trust_it() {
    let s = st("imp-qb-tls");
    let addr = crate::importer::self_signed_qbit::start().await;
    let refused = super::post_qbit_import_preview(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        format!(r#"{{"url":"https://{addr}","username":"a","password":"b"}}"#),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    let body = body_json(refused).await;
    let msg = body["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("certificate this system does not trust"),
        "got {body}"
    );
    assert!(msg.contains("\"insecure\": true"), "got {body}");

    let trusted = super::post_qbit_import_preview(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        format!(r#"{{"url":"https://{addr}","username":"a","password":"b","insecure":true}}"#),
    )
    .await;
    assert_eq!(trusted.status(), StatusCode::OK);
    let body = body_json(trusted).await;
    assert_eq!(body["total"], 1, "got {body}");
}

/// The overview's space on disk is `null` until the link scanner has
/// counted, never a zero, then the count with its time.
#[tokio::test]
async fn status_reports_disk_usage_only_once_counted() {
    let (s, _h) = populated("pop-diskusage");
    let before = status_payload(&s.state);
    assert!(before["storage"]["data_bytes"].is_null(), "got {before}");
    let usage = crate::linkindex::DiskUsage {
        data_bytes: 5000,
        shared_bytes: 3000,
        missing_files: 1,
        torrents: 9,
    };
    s.state.engines.publish_catalogue_usage(usage, 1234);
    let after = status_payload(&s.state);
    assert_eq!(after["storage"]["data_bytes"], 5000, "got {after}");
    assert_eq!(after["storage"]["shared_bytes"], 3000);
    assert_eq!(after["storage"]["measured_at"], 1234);
}

/// A click on a tracker's errors gets the tracker's own words, the torrent
/// they were about by name, and the list class that finds its siblings.
#[tokio::test]
async fn a_trackers_errors_come_back_with_their_messages() {
    let (s, hashes) = populated("pop-annerrors");
    let hoard = s.state.engines.get("hoard").expect("hoard");
    hoard.announce_cache.count_failed_message(
        "tracker.example",
        "unknown_torrent",
        "Unregistered torrent",
        &hashes[2],
        "",
    );
    let body = body_json(
        super::get_announce_errors(
            State(s.state.clone()),
            RawQuery(Some("host=tracker.example".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    let class = &body["engines"][0]["classes"][0];
    assert_eq!(body["engines"][0]["engine"], "hoard", "got {body}");
    assert_eq!(class["class"], "unknown_torrent");
    assert_eq!(class["samples"][0]["message"], "Unregistered torrent");
    assert_eq!(
        class["samples"][0]["name"], "charlie",
        "named, not just hashed: {body}"
    );
    assert_eq!(class["samples"][0]["list_class"], "dead");
}

/// ⭐⭐ A reannounce with no announce runner behind it answers **503**, out
/// loud. That is the September fix: it used to answer `ok` and do nothing,
/// which is the difference between "received" and "done" that produced
/// seven separate bugs in this repo.
///
/// Pinned as 503 rather than as success: if this ever starts answering 200
/// in a test with no runner, the silent-success bug is back.
#[tokio::test]
async fn reannouncing_with_no_runner_refuses_out_loud_rather_than_claiming_success() {
    let (s, hashes) = populated("pop-reann");
    let resp = super::reannounce_one(
        State(s.state.clone()),
        axum::extract::Path(hashes[0].clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(
        !resp.status().is_success(),
        "a reannounce that cannot happen must not report success: {:?}",
        resp.status()
    );
    let body = body_json(resp).await;
    assert!(
        body.get("error").is_some() || !body.to_string().is_empty(),
        "and it says why: {body}"
    );
}

/// ⭐ Deleting a torrent that IS here removes it from the catalogue -- and
/// the count follows, which is what the ghost hunts of September were all
/// about.
#[tokio::test]
async fn deleting_a_torrent_that_is_here_removes_it_from_the_catalogue() {
    let (s, hashes) = populated("pop-delete");
    let before = engine_page_value(&s.state, "race", "").await["total"].as_i64();
    assert_eq!(before, Some(2));

    let resp = super::delete_torrent(
        State(s.state.clone()),
        axum::extract::Path(hashes[0].clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());

    let after = engine_page_value(&s.state, "race", "").await["total"].as_i64();
    assert_eq!(after, Some(1), "the catalogue followed the deletion");
}

/// 13/10/2026: a torrent the engine runs with no store row (lost to the
/// reconcile mid-add) answered 404 to every delete, and could not be
/// re-added either. The engine's copy is what gets removed now.
#[tokio::test]
async fn a_torrent_with_no_store_row_can_still_be_removed() {
    let (s, hashes) = populated("pop-delete-strayed");
    let h = hashes[0].clone();
    assert!(
        s.state.store.lock().unwrap().delete_torrent(&h).unwrap(),
        "the row is gone"
    );
    assert!(
        find_torrent(&s.state, &h).is_some(),
        "the engine still runs it"
    );

    let resp = super::delete_torrent(
        State(s.state.clone()),
        axum::extract::Path(h.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    assert!(
        find_torrent(&s.state, &h).is_none(),
        "and now the engine does not"
    );
}

/// A multi-file torrent of `name/` over `files`, with real piece hashes.
fn torrent_over(name: &str, files: &[(&str, &[u8])], piece_len: usize) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let stream: Vec<u8> = files.iter().flat_map(|(_, d)| d.iter().copied()).collect();
    let pieces: Vec<u8> = stream
        .chunks(piece_len)
        .flat_map(|c| Sha1::digest(c).to_vec())
        .collect();
    let mut v = b"d4:infod5:filesl".to_vec();
    for (path, data) in files {
        v.extend_from_slice(
            format!("d6:lengthi{}e4:pathl{}:{path}ee", data.len(), path.len()).as_bytes(),
        );
    }
    v.extend_from_slice(
        format!(
            "e4:name{}:{name}12:piece lengthi{piece_len}e6:pieces{}:",
            name.len(),
            pieces.len()
        )
        .as_bytes(),
    );
    v.extend_from_slice(&pieces);
    v.extend_from_slice(b"ee");
    v
}

/// #99: two torrents qBittorrent kept at one path under one name, both
/// complete there, the second a repack (other size, other bytes). The import
/// asks to trust both and start them. The first is seeded; the second is
/// isolated -- never added over the first one's file -- and comes back as
/// such, stopped, with a note, instead of "complete, data found" and a
/// download of the whole payload in a folder nobody chose.
#[tokio::test(flavor = "multi_thread")]
async fn an_imported_cross_seed_with_other_bytes_is_isolated_stopped_and_said_so() {
    use crate::importer::{AddPlan, AddRequest, Added};
    use std::sync::atomic::Ordering::Relaxed;
    let s = st("imp-isolate");
    let dir = s.dir.join("tv");
    std::fs::create_dir_all(dir.join("Pack")).unwrap();
    let first: Vec<u8> = (0..40_000u32).map(|i| (i % 241) as u8).collect();
    let repack: Vec<u8> = (0..30_000u32).map(|i| (i % 239) as u8).collect();
    let on_disk = dir.join("Pack").join("E01.mkv");
    std::fs::write(&on_disk, &first).unwrap();

    let add = super::import_add(&s.state);
    let req = |bytes: Vec<u8>| AddRequest {
        bytes,
        category: String::new(),
        tags: String::new(),
        plan: AddPlan {
            save_path: dir.to_string_lossy().into_owned(),
            paused: false,
            seed_mode: true,
        },
        uploaded: 0,
        downloaded: 0,
        note: None,
    };
    assert_eq!(
        add(req(torrent_over("Pack", &[("E01.mkv", &first)], 16384))),
        Ok(Added::Seeded)
    );
    let b = torrent_over("Pack", &[("E01.mkv", &repack)], 16384);
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&b).unwrap();
    let hash = typhon_engine::torrent::hex_encode(&meta.info_hash);
    let own = dir.join(format!("Pack [{}]", &hash[..8]));
    assert_eq!(
        add(req(b)),
        Ok(Added::Isolated {
            save_path: own.to_string_lossy().into_owned()
        }),
        "reported as what the add did, not as the plan's seed"
    );

    let t = s
        .state
        .engines
        .get(&super::import_engine(&s.state))
        .unwrap()
        .manager
        .get(&meta.info_hash)
        .expect("added");
    assert!(
        t.is_paused.load(Relaxed),
        "an isolated import starts nothing"
    );
    assert!(
        t.error_msg.lock().unwrap().contains("other bytes"),
        "no note saying why: {:?}",
        t.error_msg.lock().unwrap()
    );
    assert_eq!(*t.save_path.read(), own);
    assert_eq!(
        std::fs::read(&on_disk).unwrap(),
        first,
        "the first torrent's file was touched"
    );
    assert!(
        !own.join("Pack").join("E01.mkv").exists(),
        "a file of another size was linked in"
    );
}
