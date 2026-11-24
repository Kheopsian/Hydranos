use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn with_key(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

/// Refused without a key, and answering with one, on a fresh install.
macro_rules! read_routes {
    ($($test_name:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $test_name() {
                let s = with_key(concat!("r2-", stringify!($name)));
                let refused =
                    super::$name(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await;
                assert_eq!(
                    refused.status(),
                    StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key")
                );
                let allowed =
                    super::$name(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
                assert!(
                    allowed.status().is_success() || allowed.status().is_client_error(),
                    concat!(stringify!($name), " answered {:?}"),
                    allowed.status()
                );
            }
        )+
    };
}

/// The gate ONLY.
///
/// These routes restart the daemon, open a stream that never ends, or go
/// out to the network. Calling them with a valid key inside a test would
/// do the thing. The gate is what matters here anyway: an unauthenticated
/// caller reaching `post_restart` is the whole risk.
macro_rules! gate_only {
    ($($test_name:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $test_name() {
                let s = with_key(concat!("gate-", stringify!($name)));
                let refused =
                    super::$name(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await;
                assert_eq!(
                    refused.status(),
                    StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key")
                );
                let with_empty =
                    super::$name(State(s.state.clone()), RawQuery(None), keyed("")).await;
                assert_eq!(with_empty.status(), StatusCode::UNAUTHORIZED);
            }
        )+
    };
}

read_routes!(
    r_get_nodes => get_nodes,
    r_get_announce_health => get_announce_health,
    r_get_vpn_speedtest_latest => get_vpn_speedtest_latest,
    r_get_vpn_speedtest_history => get_vpn_speedtest_history,
    r_get_startup_pause => get_startup_pause,
    r_get_provenance => get_provenance,
    r_get_hoard_page => get_hoard_page,
    r_get_race_page => get_race_page,
    r_get_drain_history => get_drain_history,
    r_get_drain_graduations => get_drain_graduations,
    r_get_arr_cleanup_scan => get_arr_cleanup_scan,
    r_get_qbit_import_status => get_qbit_import_status,
    r_get_trackers => get_trackers,
    r_get_network_mode => get_network_mode,
    r_get_tracker_stats_current => get_tracker_stats_current,
    r_get_bench_records => get_bench_records,
    r_get_bench_range => get_bench_range,
    r_get_tracker_stats_range => get_tracker_stats_range,
    r_get_network_interfaces => get_network_interfaces,
    r_agents_gone => agents_gone,
    r_move_remote_gone => move_remote_gone,
    r_get_network_engines => get_network_engines,
    r_get_qbit_import_events => get_qbit_import_events,
    r_get_status => get_status,
    r_get_logs => get_logs,
    r_get_bench_current => get_bench_current,
    r_get_port_forward => get_port_forward,
    r_get_bench_compare => get_bench_compare,
    r_get_wireguard => get_wireguard,
    r_get_live_announce_policy => get_live_announce_policy,
    r_get_health_anomalies => get_health_anomalies,
    r_get_race_events => get_race_events,
    r_qbit_version => qbit_version,
    r_qbit_webapi_version => qbit_webapi_version,
    r_qbit_build_info => qbit_build_info,
    r_qbit_preferences => qbit_preferences,
    r_qbit_transfer_info => qbit_transfer_info,
    r_get_fs_browse => get_fs_browse,
    r_post_node_enrol => post_node_enrol,
    r_hoard_pause_all => hoard_pause_all,
    r_hoard_resume_all => hoard_resume_all,
);

/// The same, for the shim routes that also read a form body: cross-seed
/// sends their parameters there.
macro_rules! read_routes_with_body {
    ($($test_name:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $test_name() {
                let s = with_key(concat!("r2b-", stringify!($name)));
                let refused = super::$name(
                    State(s.state.clone()), RawQuery(None), HeaderMap::new(), String::new(),
                )
                .await;
                assert_eq!(
                    refused.status(),
                    StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key")
                );
                let allowed = super::$name(
                    State(s.state.clone()), RawQuery(None), keyed(KEY), String::new(),
                )
                .await;
                assert!(
                    allowed.status().is_success() || allowed.status().is_client_error(),
                    concat!(stringify!($name), " answered {:?}"),
                    allowed.status()
                );
            }
        )+
    };
}

read_routes_with_body!(
    r_qbit_torrent_files => qbit_torrent_files,
    r_qbit_torrent_properties => qbit_torrent_properties,
    r_qbit_torrent_trackers => qbit_torrent_trackers,
    r_qbit_torrent_export => qbit_torrent_export,
    r_post_import_retry => post_import_retry,
);

gate_only!(
    g_post_restart => post_restart,
    g_post_settings_restart => post_settings_restart,
    g_post_settings_reset => post_settings_reset,
    g_post_startup_release => post_startup_release,
    g_stream_events => stream_events,
    g_stream_logs => stream_logs,
    g_get_update_check => get_update_check,
    g_vpn_speedtest_run => vpn_speedtest_run,
    g_drain_now => drain_now,
);

/// ⚠️ The qBittorrent shim is the *arr stack's only door, and it has no
/// place to put a header: it logs in and rides a cookie. That must not
/// mean the shim is open.
#[tokio::test]
async fn the_qbit_shim_refuses_an_unauthenticated_caller_too() {
    let s = with_key("shim-auth");
    for resp in [
        super::qbit_version(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        super::qbit_preferences(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        super::qbit_transfer_info(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
    ] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

/// The status card is what the whole UI hydrates from: it must answer on a
/// node that holds nothing, and name its version.
#[tokio::test]
async fn the_status_card_answers_on_an_empty_node() {
    let s = with_key("status");
    let body =
        body_json(super::get_status(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(body.is_object(), "got {body}");
    assert!(
        body.get("version").and_then(|v| v.as_str()).is_some(),
        "the status names the version it is: {body}"
    );
}

/// A fresh install has declared no node. That is an empty list, not an
/// error and not the local node pretending to be a remote one.
#[tokio::test]
async fn a_fresh_install_has_no_declared_nodes() {
    let s = with_key("nodes-empty");
    let body =
        body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    let empty = body.as_array().map(|a| a.is_empty()).unwrap_or(false)
        || body
            .get("nodes")
            .and_then(|n| n.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false);
    assert!(empty, "got {body}");
}

/// The graphs are sent as many points as the page can draw, not a fixed few
/// hundred: a short window stays raw, a long one is folded to the ask.
#[test]
fn bench_bucket_follows_the_points_the_page_asks_for() {
    let day = 86_400.0;
    // 15 minutes of 5 s samples is 180 points: raw, whatever the default.
    assert_eq!(bench_bucket("", 0.0, 900.0), 5.0);
    assert_eq!(bench_bucket("points=1000", 0.0, 900.0), 5.0);
    // A day to 1 000 points is 86 s; a month to the same is 43 min.
    assert!((bench_bucket("points=1000", 0.0, day) - 86.4).abs() < 1e-9);
    assert!((bench_bucket("points=1000", 0.0, 30.0 * day) - 2_592.0).abs() < 1e-9);
    // Without an ask the default is used, and a silly ask is held to 100..4000.
    assert!((bench_bucket("", 0.0, day) - day / 1_500.0).abs() < 1e-9);
    assert!((bench_bucket("points=1", 0.0, day) - day / 100.0).abs() < 1e-9);
    assert!((bench_bucket("points=999999", 0.0, 30.0 * day) - 30.0 * day / 4_000.0).abs() < 1e-9);
}
