use super::testing::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// The whole router, served on a real loopback port. Bound to :0 so tests
/// never collide, and shut down with the test.
struct Served {
    url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _state: TestState,
}

async fn serve(tag: &str) -> Served {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    // Building it is itself the assertion -- see `the_router_builds`.
    let app = super::router(s.state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Served {
        url: format!("http://{addr}"),
        _shutdown: tx,
        _state: s,
    }
}

/// ⭐⭐ The first test in this file that builds the REAL router. Every other
/// API test calls handlers directly, which is why a route could be absent,
/// shadowed or misspelt without a single failure: `set_listen_port` carried
/// a doc comment reading "NOT ROUTED YET, on purpose" while it had been
/// routed all along, and nothing contradicted it.
///
/// `matchit` panics when a pattern conflicts with one already registered,
/// and that panic happens while BUILDING. So a parameter segment that
/// collides with a literal one -- `/api/:engine/torrents` next to
/// `/api/engines/:id` -- is caught here rather than at boot, in front of an
/// operator.
#[tokio::test]
async fn the_router_builds() {
    let s = state_from("router-builds", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let _app = super::router(s.state.clone());
}

/// Every route this pass touches, in the spelling a caller actually uses.
///
/// Kept as data rather than one test each: the point is that NONE of them
/// is missing, and a list makes the one that vanished obvious.
const TOUCHED: &[&str] = &[
    // The four stubs that answer 500 unconditionally.
    "/api/race/listen-port",
    "/api/hoard/listen-port",
    "/api/race/dial-limits",
    "/api/hoard/dial-limits",
    // The two qBit-shim stubs that answer an empty OK.
    "/api/v2/torrents/reannounce",
    "/api/v2/torrents/recheck",
    // The role-in-the-path family.
    "/api/race/torrents",
    "/api/hoard/torrents",
    "/api/race/page",
    "/api/hoard/page",
    "/api/hoard/stats",
    "/api/hoard/pinned",
    "/api/race/settings",
    "/api/hoard/download-slots",
    "/api/hoard/pause",
    "/api/race/pause",
    "/api/hoard/pause-all",
    "/api/hoard/resume-all",
    "/api/hoard/torrents/bulk",
    "/api/race/torrents/bulk",
    // The canonical, engine-addressed spelling this pass introduces.
    "/api/engines/race/torrents",
    "/api/engines/hoard/torrents",
    "/api/engines/race/page",
    "/api/engines/hoard/pinned",
    "/api/engines/race/pause-all",
    "/api/engines/race/resume-all",
    "/api/engines/race/pause",
    "/api/engines/race/torrents/bulk",
];

/// A route that exists answers 405 to a method it does not declare; one
/// that does not exist answers 404. So TRACE separates "the path is in the
/// table" from "the path is not", and it does it WITHOUT running the
/// handler -- no pause, no purge, no restart as a side effect of asking.
///
/// ⚠ Only valid for routes declared with a concrete method. A route on
/// `axum::routing::any` accepts TRACE and would execute; none of the paths
/// above is one.
#[tokio::test]
async fn every_touched_route_is_in_the_table() {
    let srv = serve("route-table").await;
    let c = reqwest::Client::new();
    let mut missing = Vec::new();
    for path in TOUCHED {
        let resp = c
            .request(reqwest::Method::TRACE, format!("{}{path}", srv.url))
            .header("X-API-Key", KEY)
            .send()
            .await
            .expect("request");
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            missing.push(*path);
        }
    }
    assert!(
        missing.is_empty(),
        "these paths are not in the route table: {missing:?}"
    );
}

/// The 4.3 stubs removed in 4.4 stay removed.
///
/// Each one answered a success-shaped body while doing nothing, so the
/// regression to guard against is not a crash but a route quietly coming
/// back. 404 for a path that is gone; 405 for a verb dropped from a path
/// whose GET is kept (race settings, download slots).
#[tokio::test]
async fn the_removed_stub_routes_stay_removed() {
    let srv = serve("route-removed").await;
    let c = reqwest::Client::new();
    let cases: &[(reqwest::Method, &str, reqwest::StatusCode)] = &[
        (
            reqwest::Method::GET,
            "/api/race/choking",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::GET,
            "/api/opt/flags",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::POST,
            "/api/opt/flags",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::POST,
            "/api/hoard/verify-downloading",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::POST,
            "/api/hoard/restart-stuck",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::POST,
            "/api/arr-cleanup/execute",
            reqwest::StatusCode::NOT_FOUND,
        ),
        (
            reqwest::Method::POST,
            "/api/race/settings",
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            reqwest::Method::POST,
            "/api/hoard/download-slots",
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
        ),
        (
            reqwest::Method::DELETE,
            "/api/hoard/download-slots",
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
        ),
    ];
    let mut wrong = Vec::new();
    for (method, path, want) in cases {
        let resp = c
            .request(method.clone(), format!("{}{path}", srv.url))
            .header("X-API-Key", KEY)
            .body("{}")
            .send()
            .await
            .expect("request");
        if resp.status() != *want {
            wrong.push(format!("{method} {path}: {} (want {want})", resp.status()));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The 3.x agent API and move-remote answer 410 on every path and verb,
/// and name where to go. Behind the key: without one, a 401 like any other
/// route, so the 410 tells an anonymous caller nothing.
#[tokio::test]
async fn the_3x_agent_api_answers_gone_with_the_replacement() {
    let srv = serve("route-gone").await;
    let c = reqwest::Client::new();
    let cases: &[(reqwest::Method, &str, &str)] = &[
        (reqwest::Method::GET, "/api/agents", "/api/nodes"),
        (reqwest::Method::POST, "/api/agents", "/api/engines"),
        (reqwest::Method::GET, "/api/agents/removed", "/api/nodes"),
        (reqwest::Method::GET, "/api/agents/torrents", "/api/nodes"),
        (reqwest::Method::POST, "/api/agents/test", "/api/nodes"),
        (reqwest::Method::PUT, "/api/agents/seedbox-de", "/api/nodes"),
        (
            reqwest::Method::DELETE,
            "/api/agents/seedbox-de",
            "/api/nodes",
        ),
        (
            reqwest::Method::POST,
            "/api/agents/restore/seedbox-de",
            "/api/nodes",
        ),
        (
            reqwest::Method::POST,
            "/api/agents/seedbox-de/action",
            "/api/nodes",
        ),
        (
            reqwest::Method::POST,
            "/api/jobs/move-remote",
            "/api/selection/handoff",
        ),
    ];
    let mut wrong = Vec::new();
    for (method, path, names) in cases {
        let resp = c
            .request(method.clone(), format!("{}{path}", srv.url))
            .header("X-API-Key", KEY)
            .send()
            .await
            .expect("request");
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status != reqwest::StatusCode::GONE || !body.contains(names) {
            wrong.push(format!("{method} {path}: {status} {body}"));
        }
        let anon = c
            .request(method.clone(), format!("{}{path}", srv.url))
            .send()
            .await
            .expect("request");
        if anon.status() != reqwest::StatusCode::UNAUTHORIZED {
            wrong.push(format!("{method} {path} without a key: {}", anon.status()));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

/// The managed-WireGuard routes are NOT part of the 3.x removal: they stay
/// for the work that brings managed WireGuard back.
#[tokio::test]
async fn the_wireguard_routes_are_still_in_the_table() {
    let srv = serve("route-wg").await;
    let c = reqwest::Client::new();
    for path in [
        "/api/network/wireguard",
        "/api/network/wireguard/configs",
        "/api/network/wireguard/configs/x.conf",
        "/api/network/wireguard/engines",
    ] {
        let resp = c
            .request(reqwest::Method::TRACE, format!("{}{path}", srv.url))
            .header("X-API-Key", KEY)
            .send()
            .await
            .expect("request");
        assert_ne!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{path} left the table"
        );
    }
}

/// The daemon's route and kill switch are saved by the real router, read
/// back, refused without a key, and a kill switch with no way out is
/// reported as refusing the daemon's requests.
#[tokio::test]
async fn the_egress_route_saves_the_kill_switch_and_reports_it() {
    let srv = serve("route-egress").await;
    let c = reqwest::Client::new();
    let url = format!("{}/api/network/egress", srv.url);
    assert_eq!(
        c.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let bad = c
        .post(&url)
        .header("X-API-Key", KEY)
        .body(r#"{"socks5_host":"10.0.0.1","socks5_port":0}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);

    let saved: serde_json::Value = c
        .post(&url)
        .header("X-API-Key", KEY)
        .body(r#"{"kill_switch":true}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["kill_switch"], true, "{saved:#}");
    assert!(
        saved["daemon"]["error"]
            .as_str()
            .unwrap()
            .contains("kill switch"),
        "{saved:#}"
    );
    assert!(saved["not_covered"]
        .as_array()
        .is_some_and(|a| !a.is_empty()));

    let got: serde_json::Value = c
        .post(&url)
        .header("X-API-Key", KEY)
        .body(r#"{"egress":"proxy","socks5_host":"10.0.0.1","socks5_port":1080,"socks5_user":"u","socks5_pass":"p","kill_switch":true}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(got["daemon"]["socks5_host"], "10.0.0.1");
    assert_eq!(got["daemon"]["error"], "", "{got:#}");
    assert!(
        !got["daemon"]["route"].as_str().unwrap().contains(":p@"),
        "credentials never shown in the route line"
    );
    let again: serde_json::Value = c
        .get(&url)
        .header("X-API-Key", KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        again["daemon"]["socks5_port"], 1080,
        "read back from the file"
    );
    assert_eq!(again["egress"], "proxy");
    assert_eq!(again["daemon"]["via_kind"], "proxy");

    // Back to the mode's own rule, and the switch back to "deduced".
    let auto: serde_json::Value = c
        .post(&url)
        .header("X-API-Key", KEY)
        .body(r#"{"egress":"auto","kill_switch":null}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(auto["kill_switch"], serde_json::Value::Null, "{auto:#}");
    assert_eq!(auto["armed"], false, "direct mode, nothing written");
    assert_eq!(auto["daemon"]["via_kind"], "direct");
    assert_eq!(
        auto["daemon"]["socks5_host"], "10.0.0.1",
        "untouched: not in the body"
    );
    // A typo and an unknown engine are refused, nothing written.
    for body in [
        r#"{"egress":"tunnel"}"#,
        r#"{"egress":"engine:nope"}"#,
        r#"{"allow_direct":{"nope":true}}"#,
        r#"{"kill_switch":"yes"}"#,
    ] {
        let r = c
            .post(&url)
            .header("X-API-Key", KEY)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST, "{body}");
    }
    // allow_direct on an engine, by the route.
    let d: serde_json::Value = c
        .post(&url)
        .header("X-API-Key", KEY)
        .body(r#"{"allow_direct":{"hoard":true}}"#)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hoard = d["engines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["engine"] == "hoard")
        .unwrap()
        .clone();
    assert_eq!(hoard["allow_direct"], true, "{d:#}");
}

/// `[daemon] bypass` is written by the route, read back from the file, left
/// alone by a body that does not send it, removed by an empty list, and a
/// list with one unreadable entry writes nothing.
#[tokio::test]
async fn the_egress_route_saves_the_bypass_list() {
    let srv = serve("route-egress-bypass").await;
    let c = reqwest::Client::new();
    let url = format!("{}/api/network/egress", srv.url);
    let post = |body: &'static str| c.post(&url).header("X-API-Key", KEY).body(body).send();
    let saved: serde_json::Value = post(r#"{"bypass":["192.168.1.0/24","prowlarr"]}"#)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        saved["daemon"]["bypass"],
        serde_json::json!(["192.168.1.0/24", "prowlarr"]),
        "{saved:#}"
    );
    let r = post(r#"{"bypass":["192.168.1.0/24","0.0.0.0/0"]}"#)
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    let r = post(r#"{"bypass":"10.0.0.0/33"}"#).await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);
    let kept: serde_json::Value = post(r#"{"kill_switch":null}"#)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        kept["daemon"]["bypass"],
        serde_json::json!(["192.168.1.0/24", "prowlarr"]),
        "untouched by a body without it, and by the refused ones"
    );
    let text: serde_json::Value = post(r#"{"bypass":"10.0.0.5, nas.lan"}"#)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        text["daemon"]["bypass"],
        serde_json::json!(["10.0.0.5", "nas.lan"])
    );
    let cleared: serde_json::Value = post(r#"{"bypass":[]}"#)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cleared["daemon"]["bypass"], serde_json::json!([]));
}
