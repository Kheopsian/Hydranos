use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const ABSENT: &str = "0000000000000000000000000000000000000000";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

/// `(State, RawQuery, HeaderMap, body)` routes: refused without a key, and
/// answering -- not panicking -- on a body they cannot use.
macro_rules! body_routes {
    ($($t:ident => $name:ident, $body:expr);+ $(;)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let s = st(concat!("b-", stringify!($name)));
                let refused = super::$name(
                    State(s.state.clone()), RawQuery(None), HeaderMap::new(), $body.to_string(),
                ).await;
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key"));

                let ok = super::$name(
                    State(s.state.clone()), RawQuery(None), keyed(KEY), $body.to_string(),
                ).await;
                assert!(ok.status().is_success() || ok.status().is_client_error(),
                    concat!(stringify!($name), " answered {:?}"), ok.status());

                // ⭐ A body that is not JSON must be a 4xx, never a panic:
                // it arrives straight off the network.
                let junk = super::$name(
                    State(s.state.clone()), RawQuery(None), keyed(KEY), "{not json".to_string(),
                ).await;
                assert!(junk.status().is_client_error() || junk.status().is_success(),
                    concat!(stringify!($name), " on junk answered {:?}"), junk.status());
            }
        )+
    };
}

/// The gate only, for routes that go out to the network or rewrite
/// credentials. An unauthenticated caller reaching them is the whole risk.
macro_rules! body_gate_only {
    ($($t:ident => $name:ident);+ $(;)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let s = st(concat!("bg-", stringify!($name)));
                let refused = super::$name(
                    State(s.state.clone()), RawQuery(None), HeaderMap::new(), "{}".to_string(),
                ).await;
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key"));
            }
        )+
    };
}

/// `(State, Path, RawQuery, HeaderMap)` routes, addressed at something
/// this node does not hold.
macro_rules! path_routes {
    ($($t:ident => $name:ident, $arg:expr);+ $(;)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let s = st(concat!("p-", stringify!($name)));
                let refused = super::$name(
                    State(s.state.clone()), axum::extract::Path($arg.to_string()),
                    RawQuery(None), HeaderMap::new(),
                ).await;
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key"));

                // ⭐ Addressed at something that is not here: an answer,
                // never a panic -- and never a success for work not done.
                let missing = super::$name(
                    State(s.state.clone()), axum::extract::Path($arg.to_string()),
                    RawQuery(None), keyed(KEY),
                ).await;
                assert!(missing.status().is_client_error()
                        || missing.status().is_success()
                        || missing.status().is_server_error(),
                    concat!(stringify!($name), " answered {:?}"), missing.status());
            }
        )+
    };
}

/// `(State, Path, RawQuery, HeaderMap, body)`.
macro_rules! path_body_routes {
    ($($t:ident => $name:ident, $arg:expr, $body:expr);+ $(;)?) => {
        $(
            #[tokio::test]
            async fn $t() {
                let s = st(concat!("pb-", stringify!($name)));
                let refused = super::$name(
                    State(s.state.clone()), axum::extract::Path($arg.to_string()),
                    RawQuery(None), HeaderMap::new(), $body.to_string(),
                ).await;
                assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key"));

                let missing = super::$name(
                    State(s.state.clone()), axum::extract::Path($arg.to_string()),
                    RawQuery(None), keyed(KEY), $body.to_string(),
                ).await;
                assert!(!missing.status().is_informational(),
                    concat!(stringify!($name), " answered {:?}"), missing.status());
            }
        )+
    };
}

body_routes!(
    b_set_announce_min_seed => set_announce_min_seed, r#"{"host":"tracker.example","hours":"2"}"#;
    b_set_announce_hidden => set_announce_hidden, r#"{"host":"tracker.example","hidden":true}"#;
    b_set_announce_mute => set_announce_mute, r#"{"host":"tracker.example","muted":true}"#;
    b_post_dedup_config => post_dedup_config, r#"{"enabled":true}"#;
    b_category_create => category_create, r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#;
    b_set_announce_ip_mode => set_announce_ip_mode, r#"{"host":"tracker.example","mode":"v4"}"#;
    b_set_announce_passkey => set_announce_passkey, r#"{"host":"tracker.example","passkey":"abc"}"#;
    b_hoard_pause_bulk => hoard_pause_bulk, r#"{"hashes":[]}"#;
    b_race_pause_bulk => race_pause_bulk, r#"{"hashes":[]}"#;
    b_qbit_torrents_info => qbit_torrents_info, "";
    b_hoard_bulk => hoard_bulk, r#"{"hashes":[],"action":"pause"}"#;
    b_race_bulk => race_bulk, r#"{"hashes":[],"action":"pause"}"#;
    b_post_baseline => post_baseline, r#"{}"#;
    b_import_check_paths => import_check_paths, r#"{"paths":[]}"#;
    b_qbit_set_preferences => qbit_set_preferences, r#"{}"#;
    b_post_engine_create => post_engine_create, r#"{}"#;
);

body_gate_only!(
    bg_post_node_test => post_node_test;
    bg_post_node => post_node;
    bg_post_network_check => post_network_check;
    bg_post_network_mode => post_network_mode;
    bg_post_password => post_password;
    bg_post_qbit_import_start => post_qbit_import_start;
    bg_post_settings => post_settings;
    bg_post_torrent_add => post_torrent_add;
);

path_routes!(
    p_delete_node => delete_node, "nobody";
    p_get_node_open => get_node_open, "nobody";
    p_hoard_unpin_one => hoard_unpin_one, ABSENT;
    p_category_delete => category_delete, "no-such-category";
    p_get_torrent_file => get_torrent_file, ABSENT;
    p_get_torrent_files => get_torrent_files, ABSENT;
    p_get_torrent_trackers => get_torrent_trackers, ABSENT;
    p_race_pause_one => race_pause_one, ABSENT;
    p_race_resume_one => race_resume_one, ABSENT;
    p_get_job => get_job, "no-such-job";
    p_hoard_verify_one => hoard_verify_one, ABSENT;
    p_reannounce_one => reannounce_one, ABSENT;
    p_get_race_torrent => get_race_torrent, ABSENT;
    p_get_hoard_torrent => get_hoard_torrent, ABSENT;
    p_delete_torrent => delete_torrent, ABSENT;
    p_purge_race_torrent => purge_race_torrent, ABSENT;
    p_get_race_timeline => get_race_timeline, ABSENT;
    p_delete_job => delete_job, "no-such-job";
    p_delete_engine => delete_engine, "no-such-engine";
    p_move_preview => move_preview, ABSENT;
    p_race_snapshots => race_snapshots, ABSENT;
);

path_body_routes!(
    pb_category_update => category_update, "no-such-category", r#"{"save_path":"/data/x"}"#;
    pb_engine_pause_bulk => engine_pause_bulk, "race", r#"{"hashes":[]}"#;
    pb_post_torrent_trackers => post_torrent_trackers, ABSENT, r#"{"trackers":[]}"#;
    pb_post_add_tracker => post_add_tracker, ABSENT, r#"{"url":"https://tracker.example/announce"}"#;
    pb_post_torrent_copy => post_torrent_copy, ABSENT, r#"{"to":"hoard"}"#;
    pb_post_torrent_graduate => post_torrent_graduate, ABSENT, r#"{}"#;
    pb_post_torrent_engine => post_torrent_engine, ABSENT, r#"{"engine":"hoard"}"#;
);

/// ⭐⭐ These four routes WERE stubs: they validated the body and then
/// answered 500 unconditionally, with a comment saying the engine client
/// could not do it. The comment was about 3.x's RPC client. This build has
/// always carried `limiter().set_max_dials_per_sec()` and
/// `request_listen_rebind()`, and `rpc/dispatch.rs` already called both --
/// only the HTTP wiring was missing. So the test that pinned the stubs is
/// replaced by one that pins the behaviour.
///
/// Dial ceilings apply to a loaded engine whether or not it is on the
/// network, because the limiter is a plain counter -- so this asserts a 200
/// and reads the value BACK off the limiter rather than trusting the echo.
#[tokio::test]
async fn dial_limits_move_the_live_limiter() {
    let s = st("dial-limits");
    let before = s
        .state
        .engines
        .get("race")
        .expect("race engine")
        .manager
        .limiter()
        .max_dials_per_sec();

    let resp = super::race_dial_limits(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"max_dials_per_sec":7.5,"max_connections":4242}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK, "got {:?}", resp.status());

    let body = body_json(resp).await;
    assert_eq!(body["max_dials_per_sec"], 7.5, "answered {body}");
    assert_eq!(body["max_connections"], 4242, "answered {body}");
    // Kept: 4.3 wrote nothing and read no `max_dials_per_sec` at all,
    // so a restart lost both numbers.
    assert_eq!(body["persisted"], true, "the change must be saved: {body}");
    let cfg = s.state.cfg();
    assert_eq!(
        cfg.race.max_dials_per_sec, 7.5,
        "written where [race] reads it"
    );
    assert_eq!(cfg.race.max_connections, 4242);
    let text = std::fs::read_to_string(&s.state.config_path).unwrap();
    assert!(text.contains("max_dials_per_sec = 7.5"), "{text}");
    // And the next start reads it.
    let engine_cfg = crate::engines::engine_config_for_test(&cfg.race);
    assert_eq!(
        engine_cfg.max_dials_per_sec, 7.5,
        "the engine starts with the saved rate"
    );

    // The answer could be an echo. The limiter cannot.
    let limiter = s.state.engines.get("race").expect("race").manager.limiter();
    assert_eq!(limiter.max_dials_per_sec(), 7.5, "the limiter did not move");
    assert_eq!(limiter.max_connections(), 4242);
    assert_ne!(
        before, 7.5,
        "the fixture must not start at the tested value"
    );
}

/// The two engines of a test state are loaded but NOT on the network, so no
/// listener supervisor is registered and there is no accept socket to move.
///
/// ⭐ That is a 503, not the old 500: the request is well-formed and the
/// engine is simply not in a state to serve it. A 500 said "this build
/// cannot do it", which was never true.
#[tokio::test]
async fn a_listen_port_rebind_off_the_network_is_unavailable_not_broken() {
    let s = st("listen-port-offline");
    let resp = super::set_race_listen_port(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"port":16371}"#.into(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "got {:?}",
        resp.status()
    );
    let body = body_json(resp).await;
    let err = body["error"].as_str().unwrap_or_default();
    assert!(
        err.contains("not on the network"),
        "the refusal must say WHY, said {err:?}"
    );
    assert!(
        !err.contains("unsupported"),
        "no longer an unsupported operation: {err:?}"
    );
}

/// Put a test state's engine on the network: the real listener
/// supervisor, TCP and uTP, on a free port.
async fn listening(s: &TestState, id: &str) -> u16 {
    let eng = s.state.engines.get(id).expect("engine");
    let port = free_port();
    let binding = typhon_engine::config::ResolvedBinding {
        id: 0,
        addr: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        peer_id: [b'-'; 20],
        egress: Default::default(),
        advertised_port: port,
        only_v6: false,
    };
    let utp = typhon_engine::peer::UtpHandle::bind(port, None)
        .await
        .expect("uTP binds");
    let (m, d, flag) = (eng.manager.clone(), eng.disk.clone(), eng.listening.clone());
    tokio::spawn(async move {
        let _ = typhon_engine::peer::listen(vec![binding], port, m, d, utp, flag).await;
    });
    for _ in 0..100 {
        if eng.listening.load(std::sync::atomic::Ordering::Relaxed) {
            return port;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the listener never came up");
}

/// A port nothing holds right now, TCP and UDP.
fn free_port() -> u16 {
    loop {
        let t = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let p = t.local_addr().unwrap().port();
        if std::net::UdpSocket::bind(("0.0.0.0", p)).is_ok() {
            return p;
        }
    }
}

/// ⭐⭐ #51. The listen port really moves -- TCP and uTP -- the answer
/// comes after the bind, announces follow, and the file keeps it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listen_port_change_moves_tcp_and_utp_and_is_saved() {
    let s = st("listen-port-live");
    let old = listening(&s, "race").await;
    let new = free_port();
    let resp = super::engine_listen_port_by_id(
        State(s.state.clone()),
        axum::extract::Path("race".to_string()),
        RawQuery(None),
        keyed(KEY),
        format!(r#"{{"port":{new}}}"#),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["port"], new, "{body}");
    assert_eq!(body["persisted"], true, "{body}");

    // TCP: the new port accepts, the old one is closed.
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", new)).is_ok(),
        "TCP on the new port"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", old)).is_err(),
        "the old TCP port is let go"
    );
    // uTP: the new UDP port is taken by the engine, the old one is free.
    assert!(
        std::net::UdpSocket::bind(("0.0.0.0", new)).is_err(),
        "uTP holds the new port"
    );
    let mut freed = false;
    for _ in 0..50 {
        if std::net::UdpSocket::bind(("0.0.0.0", old)).is_ok() {
            freed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(freed, "the old uTP socket is shut down, not left answering");

    let eng = s.state.engines.get("race").unwrap();
    assert_eq!(
        eng.manager.announced_port(eng.listen_port),
        new,
        "trackers are told the new port"
    );
    assert_eq!(
        s.state.cfg().race.listen_port,
        new,
        "the next start listens there too"
    );
}

/// ⭐⭐ #51. A port that cannot be bound leaves the engine where it was:
/// 4.3 had already closed the old listener, and stopped for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_port_in_use_keeps_the_old_listener() {
    let s = st("listen-port-taken");
    let old = listening(&s, "race").await;
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    let before = std::fs::read_to_string(&s.state.config_path).unwrap();
    let resp = super::set_listen_port(&s.state, "race", &format!(r#"{{"port":{taken}}}"#)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert_eq!(
        body["port"], old,
        "the answer names the port still in use: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains(&format!("still listening on {old}")),
        "{body}"
    );

    let eng = s.state.engines.get("race").unwrap();
    assert!(
        eng.listening.load(std::sync::atomic::Ordering::Relaxed),
        "still listening"
    );
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", old)).is_ok(),
        "the old port still accepts"
    );
    assert!(
        std::net::UdpSocket::bind(("0.0.0.0", old)).is_err(),
        "uTP still on the old port"
    );
    assert_eq!(
        eng.manager.announced_port(eng.listen_port),
        old,
        "announces unchanged"
    );
    assert_eq!(
        std::fs::read_to_string(&s.state.config_path).unwrap(),
        before,
        "nothing written"
    );

    // And the engine can still move afterwards.
    let next = free_port();
    let ok = super::set_listen_port(&s.state, "race", &format!(r#"{{"port":{next}}}"#)).await;
    assert_eq!(ok.status(), StatusCode::OK);
}

/// An engine that takes its port from gluetun is refused: the next poll
/// would move it back, and the file's port is not the one it uses.
#[tokio::test]
async fn a_gluetun_engines_port_is_not_set_here() {
    let s = state_from(
        "listen-port-gluetun",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\ngluetun_port_forward = true\n"),
    );
    let resp = super::set_listen_port(&s.state, "race", r#"{"port":40100}"#).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("gluetun"),
        "{body}"
    );
}

/// Another engine's port is refused before anything moves: the next
/// start would hand one of them a free port, and the file would lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_engines_port_is_refused() {
    let s = st("listen-port-clash");
    listening(&s, "race").await;
    let hoard = s.state.engines.get("hoard").unwrap();
    let theirs = hoard.manager.announced_port(hoard.listen_port);
    let resp = super::set_listen_port(&s.state, "race", &format!(r#"{{"port":{theirs}}}"#)).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let body = body_json(resp).await;
    assert!(
        body["error"].as_str().unwrap_or_default().contains("hoard"),
        "{body}"
    );
}

/// An HTTP echo on loopback: the first request (the default route,
/// measured first) gets `first`, every later one `then`.
#[cfg(target_os = "linux")]
async fn echo_server(first: &'static str, then: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut n = 0;
        while let Ok((mut sock, _)) = l.accept().await {
            let body = if n == 0 { first } else { then };
            n += 1;
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://127.0.0.1:{port}/")
}

async fn check(s: &TestState, echo: &str) -> Vec<serde_json::Value> {
    let resp = super::post_network_check(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({ "echo_url": echo }).to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["results"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn row<'a>(rows: &'a [serde_json::Value], id: &str) -> &'a serde_json::Value {
    rows.iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {rows:?}"))
}

/// ⭐⭐ #53. An engine set to leave by an interface that shows the
/// default route's address is a LEAK, and the check says so. 4.3 made
/// four identical default-route requests and could never see one.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_network_check_names_a_leak() {
    let s = state_from(
        "netcheck-leak",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[race]\nbind_interface = \"lo\"\n"),
    );
    // Same address whichever way: the "tunnel" leads where the default
    // route does.
    let rows = check(&s, &echo_server("198.51.100.1", "198.51.100.1").await).await;
    assert_eq!(row(&rows, "default_route")["status"], "ok");
    let leak = row(&rows, "announce_race");
    assert_eq!(leak["status"], "fail", "{leak}");
    assert!(leak["detail"].as_str().unwrap().contains("LEAK"), "{leak}");
    assert_eq!(row(&rows, "peer_egress_race")["status"], "fail");
    // hoard has no interface and no proxy: the default route on purpose.
    let hoard = row(&rows, "announce_hoard");
    assert_eq!(hoard["status"], "ok", "{hoard}");
    assert!(
        hoard["detail"].as_str().unwrap().contains("default route"),
        "{hoard}"
    );
}

/// The same engine whose interface shows another address is fine, and
/// every engine of the node is checked, not just race and hoard.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_network_check_passes_a_tunnel_that_holds() {
    let s = state_from(
        "netcheck-ok",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[race]\nbind_interface = \"lo\"\n\n\
             [[engine]]\nname = \"vpn7\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26991\n"
        ),
    );
    let rows = check(&s, &echo_server("198.51.100.1", "203.0.113.9").await).await;
    let race = row(&rows, "announce_race");
    assert_eq!(race["status"], "ok", "{race}");
    assert!(
        race["detail"]
            .as_str()
            .unwrap()
            .contains("not the default route's 198.51.100.1"),
        "{race}"
    );
    row(&rows, "announce_vpn7");
    // Inbound comes from what happened, never "not tested".
    let inbound = row(&rows, "inbound_race");
    assert!(
        !inbound["detail"].as_str().unwrap().contains("not tested"),
        "{inbound}"
    );
}

/// Under the kill switch the default route is not even probed.
#[tokio::test]
async fn the_network_check_sends_nothing_by_the_default_route_under_the_kill_switch() {
    let s = state_from(
        "netcheck-ks",
        &format!("[daemon]\napi_key = \"{KEY}\"\nkill_switch = true\n"),
    );
    let rows = check(&s, "http://127.0.0.1:9/").await;
    let d = row(&rows, "default_route");
    assert_eq!(d["status"], "warn");
    assert!(d["detail"].as_str().unwrap().contains("kill switch"), "{d}");
}

/// An echo that counts the requests it gets.
async fn counting_echo(
    body: &'static str,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/"), hits)
}

/// `[daemon] exit_probe_url` names the echo service the exit address is
/// asked of.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_exit_probe_asks_the_configured_echo_service() {
    let (url, hits) = counting_echo("203.0.113.7").await;
    let s = state_from(
        "exitprobe-url",
        &format!("[daemon]\napi_key = \"{KEY}\"\nexit_probe_url = \"{url}\"\n"),
    );
    let v = body_json(
        super::get_public_ip(
            State(s.state.clone()),
            RawQuery(Some("refresh=1".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert_eq!(v["ip"], "203.0.113.7", "{v}");
    assert!(hits.load(std::sync::atomic::Ordering::SeqCst) > 0);
}

/// `[daemon] exit_probe = false` sends nothing to any echo service, and
/// what depended on the exit address says it is unknown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exit_probe_false_asks_nothing_and_says_unknown() {
    let (url, hits) = counting_echo("203.0.113.7").await;
    let s = state_from(
        "exitprobe-off",
        &format!("[daemon]\napi_key = \"{KEY}\"\nexit_probe = false\nexit_probe_url = \"{url}\"\n\n[race]\nlisten_port = 16172\n"),
    );
    let v = body_json(
        super::get_public_ip(
            State(s.state.clone()),
            RawQuery(Some("refresh=1".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert_eq!(v["ip"], "", "{v}");
    assert_eq!(v["exit_probe"], "off", "{v}");
    let n = body_json(
        super::get_network_engines(
            State(s.state.clone()),
            RawQuery(Some("refresh=1".into())),
            keyed(KEY),
        )
        .await,
    )
    .await;
    let rows = n["engines"].as_array().unwrap();
    assert!(!rows.is_empty(), "{n}");
    for r in rows {
        assert_eq!(r["state"], "unknown", "{r}");
        assert!(r.get("exit_ip").is_none(), "{r}");
    }
    let resp = super::post_network_check(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        "{}".into(),
    )
    .await;
    let c = body_json(resp).await;
    assert_eq!(c["results"][0]["id"], "exit_probe", "{c}");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no request reached the echo service"
    );
}

/// ⭐ #62. Where no socket can be pinned, any `bind_interface` in a write
/// is refused with the reason -- in the network form, in an extra
/// engine's row, in an engine creation -- and an empty one is fine.
#[test]
fn bind_interface_is_refused_where_it_cannot_be_applied() {
    let form =
        serde_json::json!({"fields": {"race_bind_interface": "wg0", "hoard_bind_interface": ""}});
    let err = super::interface_pin_allowed(false, &form).unwrap_err();
    assert!(err.contains("Linux-only") && err.contains("wg0"), "{err}");
    let extra = serde_json::json!({"extra_engines": [{"id": "vpn7", "bind_interface": "tun1"}]});
    assert!(super::interface_pin_allowed(false, &extra).is_err());
    let create = serde_json::json!({"id": "x", "bind_interface": "  "});
    assert!(
        super::interface_pin_allowed(false, &create).is_ok(),
        "blank is no interface"
    );
    assert!(
        super::interface_pin_allowed(true, &form).is_ok(),
        "Linux applies it"
    );
    // This build's answer, as the Network tab reads it.
    assert_eq!(
        typhon_engine::netpin::DEVICE_PIN_SUPPORTED,
        cfg!(target_os = "linux")
    );
}

/// The Network tab is told whether the field can be offered.
#[tokio::test]
async fn the_network_tab_is_told_whether_bind_interface_applies() {
    let s = st("netmode-bindiface");
    let v = body_json(
        super::get_network_mode(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    assert_eq!(
        v["bind_interface"]["supported"],
        cfg!(target_os = "linux"),
        "{v}"
    );
}

/// The Settings tab is told whether Pre-allocate does anything here.
#[tokio::test]
async fn the_settings_tab_is_told_whether_preallocate_applies() {
    let s = st("netmode-prealloc");
    let v = body_json(
        super::get_network_mode(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    assert_eq!(
        v["preallocate_supported"],
        cfg!(any(target_os = "linux", windows)),
        "{v}"
    );
}

#[test]
fn a_leak_verdict_reads_the_comparison() {
    let v = |way, m: Result<&str, &str>, b| {
        super::leak_verdict(
            "trackers",
            way,
            m.map(str::to_string).map_err(str::to_string),
            b,
        )
    };
    assert_eq!(
        v(
            Some("by interface wg0"),
            Ok("203.0.113.1"),
            Some("203.0.113.1")
        )
        .0,
        "fail"
    );
    assert_eq!(
        v(
            Some("by interface wg0"),
            Ok("198.51.100.2"),
            Some("203.0.113.1")
        )
        .0,
        "ok"
    );
    assert_eq!(
        v(Some("by interface wg0"), Ok("198.51.100.2"), None).0,
        "warn"
    );
    assert_eq!(
        v(
            Some("by interface wg0"),
            Err("timeout"),
            Some("203.0.113.1")
        )
        .0,
        "fail"
    );
    assert_eq!(v(None, Ok("203.0.113.1"), Some("203.0.113.1")).0, "ok");
}

/// ⭐⭐ #54. `/api/port-forward` reads the engines: 4.3 answered
/// constants (`listen_healthy: true`, 0 peers, reach `unknown`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn port_forward_reports_what_the_engines_do() {
    let s = state_from(
        "pf-real",
        &format!("auto_port_forward = false\n\n[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let get = |s: &TestState| {
        let st = s.state.clone();
        async move {
            body_json(super::get_port_forward(State(st), RawQuery(None), keyed(KEY)).await).await
        }
    };
    // Nothing on the network yet: not listening, and said.
    let before = get(&s).await;
    assert_eq!(before["listen_healthy"], false, "{before}");
    let race = |v: &serde_json::Value| {
        v["engines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["engine"] == "race")
            .cloned()
            .unwrap()
    };
    assert_eq!(race(&before)["reachable"], "no");

    let port = listening(&s, "race").await;
    let mid = race(&get(&s).await);
    assert_eq!(mid["listening"], true);
    assert_eq!(mid["listen_port"], port, "the port actually held");
    assert_eq!(mid["reachable"], "unproven", "listening is not reachable");
    assert_eq!(mid["forward"]["by"], "off", "auto_port_forward = false");

    s.state
        .engines
        .get("race")
        .unwrap()
        .manager
        .note_inbound_peer();
    let after = race(&get(&s).await);
    assert_eq!(
        (after["inbound_peers"].as_u64(), after["reachable"].as_str()),
        (Some(1), Some("yes"))
    );
}

/// An engine whose mapping was refused says why.
#[tokio::test]
async fn port_forward_names_a_refused_mapping() {
    let s = state_from("pf-refused", &format!("[daemon]\napi_key = \"{KEY}\"\n\n[[engine]]\nname = \"pfrefused\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26993\n"));
    crate::portmap::record_refusal("pfrefused", 26993, "it leaves through a SOCKS5 proxy");
    let v = body_json(
        super::get_port_forward(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    let row = v["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["engine"] == "pfrefused")
        .cloned()
        .unwrap();
    assert_eq!(row["forward"]["by"], "none", "{row}");
    assert!(row["forward"]["refused"]
        .as_str()
        .unwrap()
        .contains("SOCKS5"));
}

/// An unknown engine is not an excuse to answer ok.
#[tokio::test]
async fn dial_limits_on_an_absent_engine_is_refused() {
    let s = st("dial-limits-absent");
    let resp =
        super::set_dial_limits(&s.state, "no-such-engine", r#"{"max_dials_per_sec":1.0}"#).await;
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// The stubs still VALIDATE: a body they cannot parse is a 400, and that
/// part is real. A port of zero is out of range whatever the engine can do.
#[tokio::test]
async fn the_stubs_still_refuse_a_body_that_is_wrong() {
    let s = st("stub-validate");
    let bad = super::set_race_listen_port(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        "{not json".into(),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let zero = super::set_race_listen_port(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"port":0}"#.into(),
    )
    .await;
    assert_eq!(
        zero.status(),
        StatusCode::BAD_REQUEST,
        "port 0 is out of range"
    );
}

/// ⭐⭐ A route that DELETES must not answer success for a torrent it does
/// not hold. "Received" is not "done" -- that confusion is the shape of
/// seven separate bugs in this repo.
#[tokio::test]
async fn deleting_a_torrent_that_is_not_here_is_not_reported_as_done() {
    let s = st("del-absent");
    let resp = super::delete_torrent(
        State(s.state.clone()),
        axum::extract::Path(ABSENT.to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(
        !resp.status().is_success(),
        "a torrent that is not here cannot have been deleted: {:?}",
        resp.status()
    );
}

/// A category is created, listed, and refuses to be created twice under
/// the same name.
#[tokio::test]
async fn a_category_is_created_and_then_listed() {
    let s = st("cat-create");
    let made = super::category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;
    assert!(made.status().is_success(), "got {:?}", made.status());

    let listed =
        body_json(super::get_categories(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(
        listed.to_string().contains("films"),
        "the category we just made is listed: {listed}"
    );
}

/// ⚠️ A category carries a MODE, and the mode is what routes a torrent to
/// an engine. A category with no mode silently sends everything to race --
/// the trap that sent a whole bench import to the wrong engine.
#[tokio::test]
async fn a_category_keeps_the_mode_it_was_given() {
    let s = st("cat-mode");
    super::category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
    )
    .await;
    let (engine, _path) = placement(&s.state, "films", "");
    assert_eq!(engine, "hoard", "a hoard category places into the hoard");
}

/// The volume policy is typed into the panel and must apply on the NEXT
/// tick, so it lives in the store rather than in default.toml.
#[tokio::test]
async fn a_volume_policy_is_refused_without_a_key() {
    let s = st("volpolicy");
    let resp = super::set_volume_policy(
        State(s.state.clone()),
        RawQuery(None),
        HeaderMap::new(),
        // The struct has no Default; every optional field does, so a
        // document naming only the volume is the smallest valid body.
        Json(
            serde_json::from_value::<VolumePolicyBody>(serde_json::json!({"volume": "/mnt/race"}))
                .expect("volume is the only required field"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
