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

fn populated(tag: &str) -> (TestState, String) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (hash, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "fr",
        true,
        true,
        "race",
    )
    .expect("added");
    (s, hash)
}

/// The rest of the read surface, on a node that holds something.
///
/// Two properties per route, and they are the ones that were actually
/// broken in this repo: it REFUSES an unauthenticated caller (the open API
/// of 10/09), and it does not panic on real state.
macro_rules! rest_routes {
    ($($t:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let (s, _h) = populated(concat!("rest-", stringify!($name)));

                let refused = super::$name(
                    State(s.state.clone()), RawQuery(None), HeaderMap::new()
                ).await;
                assert_eq!(
                    refused.status(), StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key")
                );

                let allowed = super::$name(
                    State(s.state.clone()), RawQuery(None), keyed(KEY)
                ).await;
                assert!(
                    !allowed.status().is_server_error(),
                    concat!(stringify!($name), " answered {:?}"),
                    allowed.status()
                );
            }
        )+
    };
}

rest_routes!(
    rest_get_bench_current => get_bench_current,
    rest_get_bench_records => get_bench_records,
    rest_get_drain_graduations => get_drain_graduations,
    rest_get_drain_history => get_drain_history,
    rest_get_fs_browse => get_fs_browse,
    rest_get_live_announce_policy => get_live_announce_policy,
    rest_get_logs => get_logs,
    rest_get_network_interfaces => get_network_interfaces,
    rest_get_network_mode => get_network_mode,
    rest_get_nodes => get_nodes,
    rest_get_port_forward => get_port_forward,
    rest_get_qbit_import_events => get_qbit_import_events,
    rest_get_race_events => get_race_events,
    rest_get_startup_pause => get_startup_pause,
    rest_get_tracker_stats_current => get_tracker_stats_current,
    rest_get_vpn_speedtest_history => get_vpn_speedtest_history,
    rest_get_vpn_speedtest_latest => get_vpn_speedtest_latest,
    rest_get_wireguard => get_wireguard,
    rest_qbit_preferences => qbit_preferences,
    rest_get_health_anomalies => get_health_anomalies,
    rest_get_provenance => get_provenance,
    rest_get_arr_cleanup_scan => get_arr_cleanup_scan,
);

/// ⭐ The health endpoint has NO key gate on purpose: it is what a
/// container orchestrator polls, and it must answer before anyone has
/// configured anything. It therefore must not leak state either.
#[tokio::test]
async fn the_health_endpoint_answers_without_a_key() {
    let (s, _h) = populated("rest-health");
    let resp = super::get_health(State(s.state.clone())).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
}

/// The changelog is compiled into the binary and served, so a release
/// that cannot describe itself is visible immediately.
#[tokio::test]
async fn the_changelog_is_served_from_the_binary() {
    let resp = super::get_changelog().await;
    assert!(resp.status().is_success());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        !bytes.is_empty(),
        "a release with no entry cannot describe itself"
    );
}

/// Per-torrent timeline routes: refused without a key, and answering for a
/// torrent that is actually here.
#[tokio::test]
async fn the_per_torrent_timeline_routes_are_gated_and_answer() {
    let (s, hash) = populated("rest-timeline");
    for resp in [
        super::get_race_timeline(
            State(s.state.clone()),
            axum::extract::Path(hash.clone()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await,
        super::race_snapshots(
            State(s.state.clone()),
            axum::extract::Path(hash.clone()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await,
    ] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    let ok = super::get_race_timeline(
        State(s.state.clone()),
        axum::extract::Path(hash),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(!ok.status().is_server_error(), "got {:?}", ok.status());
}

/// ⭐ The timeline must SERVE what was recorded, not merely answer 200.
///
/// The test above -- "does not 500" -- passed for the whole life of the V4
/// port, during which this handler returned two hard-coded empty arrays
/// while the recorder filled bench.db underneath. Asserting on the shape of
/// the answer is the difference between a route that replies and a route
/// that works.
#[tokio::test]
async fn the_timeline_serves_the_recorded_event_and_snapshot() {
    let (s, hash) = populated("rest-timeline-data");
    let bench = std::sync::Arc::new(std::sync::Mutex::new(
        crate::benchdb::BenchDb::open_in_memory().expect("bench"),
    ));
    {
        let db = bench.lock().unwrap();
        db.record(&crate::benchdb::RaceEvent {
            ts: 1000.0,
            info_hash: hash.clone(),
            event: "added".into(),
            name: "a.release".into(),
            size: 42,
            ..Default::default()
        })
        .expect("record event");
        db.record_snapshot(&crate::benchdb::RaceSnapshot {
            ts: 1005.0,
            info_hash: hash.clone(),
            progress: 0.5,
            download_rate: 1234.0,
            ..Default::default()
        })
        .expect("record snapshot");
        // A different torrent's rows must not leak into this timeline.
        db.record_snapshot(&crate::benchdb::RaceSnapshot {
            ts: 1006.0,
            info_hash: "f".repeat(40),
            progress: 0.9,
            ..Default::default()
        })
        .expect("record other");
    }
    let mut state = s.state.clone();
    state.bench = Some(bench);

    let resp = super::get_race_timeline(
        State(state),
        axum::extract::Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;

    let events = body["events"].as_array().expect("events array");
    assert_eq!(events.len(), 1, "the recorded event is served: {body}");
    assert_eq!(events[0]["event"], "added");

    let snaps = body["snapshots"].as_array().expect("snapshots array");
    assert_eq!(snaps.len(), 1, "only THIS torrent's snapshot: {body}");
    assert_eq!(snaps[0]["progress"], 0.5);
    assert_eq!(snaps[0]["download_rate"], 1234.0);
}

/// No measurement database is not an error: the timeline is observability,
/// and losing it must never take the API down with it.
#[tokio::test]
async fn a_timeline_without_a_bench_db_is_empty_rather_than_a_failure() {
    let (s, hash) = populated("rest-timeline-nobench");
    assert!(s.state.bench.is_none(), "the fixture has no bench db");
    let resp = super::get_race_timeline(
        State(s.state.clone()),
        axum::extract::Path(hash),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["events"].as_array().map(|a| a.len()), Some(0));
    assert_eq!(body["snapshots"].as_array().map(|a| a.len()), Some(0));
}

/// ⚠️ `get_fs_browse` walks the filesystem from a path the CALLER gives.
/// A path outside what the daemon should show is the one thing it must not
/// serve, and a missing one must not panic.
#[tokio::test]
async fn browsing_a_path_that_does_not_exist_is_not_a_crash() {
    let (s, _h) = populated("rest-browse");
    let resp = super::get_fs_browse(
        State(s.state.clone()),
        RawQuery(Some("path=/tmp/typhon-no-such-dir-4a7c".into())),
        keyed(KEY),
    )
    .await;
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// The logs route serves the ring buffer. On a fresh process it may be
/// empty, and empty must be an empty list rather than a failure.
#[tokio::test]
async fn the_log_route_answers_on_an_empty_buffer() {
    let (s, _h) = populated("rest-logs");
    let resp = super::get_logs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let _ = body_json(resp).await;
}

/// ⭐⭐ `bench` is None in this fixture, which is a NORMAL state: the
/// timeline is observability and losing it must never cost the seedbox.
/// Every route that reads it answers empty rather than failing.
#[tokio::test]
async fn the_measurement_routes_answer_empty_when_there_is_no_bench_db() {
    let (s, _h) = populated("rest-nobench");
    assert!(s.bench.is_none(), "the fixture has no bench database");
    for resp in [
        super::get_bench_current(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
        super::get_bench_records(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
        super::get_race_events(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    ] {
        assert!(
            !resp.status().is_server_error(),
            "a missing bench db must not be a server error: {:?}",
            resp.status()
        );
    }
}
