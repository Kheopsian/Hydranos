//! Moves to a node as persistent jobs, node edit and key rotation.
//!
//! Each "node" here is a real HTTP server on loopback: either a fixture that
//! answers the two routes the watcher reads, or a whole second Hydranos router
//! for the key rotation, so both halves of that exchange are the shipped code.

use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const NODE_KEY: &str = "fedcba9876543210fedcba9876543210";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xCDu8; 20];
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

/// A torrent in this instance's race engine, saved under the test's own dir.
fn add_local(s: &TestState, name: &str) -> String {
    let engines = s.engines.engines();
    let engine = engines.iter().find(|e| e.id == "race").expect("race engine");
    let save = s.dir.join("data");
    std::fs::create_dir_all(&save).unwrap();
    let (ih, _) = engine
        .manager
        .add_torrent_bytes(&torrent_bytes(name), save.to_str().unwrap(), true, true)
        .unwrap_or_else(|e| panic!("add {name}: {e}"));
    typhon_engine::torrent::hex_encode(&ih)
}

struct Served {
    url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn serve(app: axum::Router) -> Served {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    Served { url: format!("http://{addr}"), _shutdown: tx }
}

/// A node hosting ONE engine, `vpn1`, and only the per-id page route: the
/// 4.3 poll (`/api/vpn1/page`) gets a 404 here, as it did on a real node.
async fn node_with_vpn1(hash: String, progress: f64) -> Served {
    use axum::routing::get;
    let app = axum::Router::new()
        .route("/api/engines", get(|| async { Json(serde_json::json!([{"id": "vpn1"}])) }))
        .route(
            "/api/engines/:id/page",
            get(move |Path(id): Path<String>| {
                let hash = hash.clone();
                async move {
                    if id != "vpn1" {
                        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no engine"})));
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({"rows": [{"info_hash": hash, "progress": progress}], "total": 1})),
                    )
                }
            }),
        );
    serve(app).await
}

fn declare(s: &TestState, name: &str, url: &str, api_key: &str) {
    s.store
        .lock()
        .unwrap()
        .put_node(&crate::store::Node {
            name: name.into(),
            url: url.into(),
            api_key: api_key.into(),
            enabled: true,
            added_at: 1_700_000_000,
        })
        .unwrap();
}

fn wait_job(s: &TestState, hash: &str, node: &str, engine: &str, deadline: i64) -> String {
    let params = serde_json::json!({"name": "x", "node": node, "engine": engine, "deadline": deadline}).to_string();
    s.store.lock().unwrap().create_waiting_job("handoff", hash, &params, 16384).unwrap()
}

fn far_future() -> i64 {
    4_000_000_000
}

fn held_locally(s: &TestState, hash: &str) -> bool {
    find_torrent(&s.state, hash).is_some()
}

/// ⭐⭐ #69: a move to a node's EXTRA engine completes. 4.3 polled a route
/// that exists for race and hoard only, never confirmed, and kept the local
/// copy after six hours.
#[tokio::test]
async fn a_move_to_a_nodes_extra_engine_is_confirmed_and_drops_the_local_copy() {
    let s = st("handoff-vpn1");
    let hash = add_local(&s, "handoff-vpn1-alpha");
    let node = node_with_vpn1(hash.clone(), 1.0).await;
    declare(&s, "seedbox", &node.url, NODE_KEY);
    let id = wait_job(&s, &hash, "seedbox", "vpn1", far_future());

    assert_eq!(handoff_watch_once(&s.state).await, 1, "one local copy dropped");
    assert_eq!(s.store.lock().unwrap().job(&id).unwrap().state, "done");
    assert!(!held_locally(&s, &hash), "the local copy is gone once the node holds it all");
}

/// With no engine named, every engine of the node is asked: the far side's
/// category decided where the torrent went.
#[tokio::test]
async fn a_move_with_no_engine_named_is_found_in_whichever_engine_took_it() {
    let s = st("handoff-any");
    let hash = add_local(&s, "handoff-any-alpha");
    let node = node_with_vpn1(hash.clone(), 1.0).await;
    declare(&s, "seedbox", &node.url, NODE_KEY);
    let id = wait_job(&s, &hash, "seedbox", "", far_future());
    assert_eq!(handoff_watch_once(&s.state).await, 1);
    assert_eq!(s.store.lock().unwrap().job(&id).unwrap().state, "done");
}

/// ⭐ #68: the wait is a row, so it is visible, carries progress, and a
/// passed deadline ends it as FAILED with the local copy kept.
#[tokio::test]
async fn an_incomplete_move_records_progress_and_fails_safe_at_its_deadline() {
    let s = st("handoff-partial");
    let hash = add_local(&s, "handoff-partial-alpha");
    let node = node_with_vpn1(hash.clone(), 0.5).await;
    declare(&s, "seedbox", &node.url, NODE_KEY);

    let id = wait_job(&s, &hash, "seedbox", "vpn1", far_future());
    assert_eq!(handoff_watch_once(&s.state).await, 0);
    let j = s.store.lock().unwrap().job(&id).unwrap();
    assert_eq!(j.state, "waiting", "still waiting");
    assert_eq!(j.progress_bytes, 8192, "half of 16 KiB, as the node reported");

    // Another torrent the node never listed, past its deadline.
    s.store.lock().unwrap().cancel_job(&id);
    let other = add_local(&s, "handoff-partial-beta");
    let id = wait_job(&s, &other, "seedbox", "vpn1", 1);
    assert_eq!(handoff_watch_once(&s.state).await, 0);
    let j = s.store.lock().unwrap().job(&id).unwrap();
    assert_eq!(j.state, "failed");
    assert!(j.error.contains("local copy is kept"), "{}", j.error);
    assert!(held_locally(&s, &other));
}

/// ⭐ A cancelled move never deletes, even when the node reports complete.
#[tokio::test]
async fn a_cancelled_move_keeps_the_local_copy() {
    let s = st("handoff-cancel");
    let hash = add_local(&s, "handoff-cancel-alpha");
    let node = node_with_vpn1(hash.clone(), 1.0).await;
    declare(&s, "seedbox", &node.url, NODE_KEY);
    let id = wait_job(&s, &hash, "seedbox", "vpn1", far_future());

    let resp = super::delete_job(State(s.state.clone()), Path(id.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(resp.status(), StatusCode::OK, "a waiting job can be cancelled");
    assert_eq!(handoff_watch_once(&s.state).await, 0);
    assert_eq!(s.store.lock().unwrap().job(&id).unwrap().state, "cancelled");
    assert!(held_locally(&s, &hash), "nothing deleted after a cancel");
}

/// A node removed while a move waits on it ends the move, local copy kept,
/// rather than polling nothing for six hours.
#[tokio::test]
async fn a_move_to_a_removed_node_fails_and_keeps_the_local_copy() {
    let s = st("handoff-gone");
    let hash = add_local(&s, "handoff-gone-alpha");
    let id = wait_job(&s, &hash, "nobody", "", far_future());
    assert_eq!(handoff_watch_once(&s.state).await, 0);
    let j = s.store.lock().unwrap().job(&id).unwrap();
    assert_eq!(j.state, "failed");
    assert!(j.error.contains("removed"), "{}", j.error);
    assert!(held_locally(&s, &hash));
}

/// A node that does not answer is asked again: no verdict, no deletion.
#[tokio::test]
async fn a_node_that_does_not_answer_leaves_the_move_waiting() {
    let s = st("handoff-down");
    let hash = add_local(&s, "handoff-down-alpha");
    declare(&s, "seedbox", "http://127.0.0.1:1", NODE_KEY);
    let id = wait_job(&s, &hash, "seedbox", "vpn1", far_future());
    assert_eq!(handoff_watch_once(&s.state).await, 0);
    assert_eq!(s.store.lock().unwrap().job(&id).unwrap().state, "waiting");
    assert!(held_locally(&s, &hash));
}

/// A waiting move shows on the Jobs list, with its state and destination.
#[tokio::test]
async fn a_waiting_move_is_listed_on_the_jobs_route() {
    let s = st("handoff-listed");
    let id = wait_job(&s, &"a".repeat(40), "seedbox", "vpn1", far_future());
    let v = body_json(super::get_jobs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    let row = v.as_array().unwrap().iter().find(|j| j["id"] == serde_json::json!(id)).expect("listed");
    assert_eq!(row["type"], "handoff");
    assert_eq!(row["state"], "waiting");
    assert_eq!(row["params"]["node"], "seedbox");
}

// ---------------------------------------------------------------------------
// #70: edit, rename, key rotation
// ---------------------------------------------------------------------------

async fn patch(s: &TestState, name: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let resp = super::patch_node(State(s.state.clone()), Path(name.into()), RawQuery(None), keyed(KEY), body.to_string()).await;
    let code = resp.status();
    (code, body_json(resp).await)
}

#[tokio::test]
async fn a_node_can_be_renamed_and_its_waiting_moves_follow() {
    let s = st("node-rename");
    declare(&s, "alpha", "http://10.0.0.5:8199", NODE_KEY);
    declare(&s, "beta", "http://10.0.0.6:8199", NODE_KEY);
    let id = wait_job(&s, &"b".repeat(40), "alpha", "", far_future());

    let (code, _) = patch(&s, "alpha", serde_json::json!({"name": "beta"})).await;
    assert_eq!(code, StatusCode::CONFLICT, "a rename never merges two nodes");
    let (code, _) = patch(&s, "alpha", serde_json::json!({"name": "a/b"})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, v) = patch(&s, "alpha", serde_json::json!({"name": "gamma"})).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "gamma");

    let store = s.store.lock().unwrap();
    assert!(store.node("alpha").unwrap().is_none());
    assert_eq!(store.node("gamma").unwrap().unwrap().api_key, NODE_KEY, "the key went with it");
    let p: serde_json::Value = serde_json::from_str(&store.job(&id).unwrap().params).unwrap();
    assert_eq!(p["node"], "gamma");
}

#[tokio::test]
async fn editing_a_node_is_refused_for_loopback_unknown_nodes_and_empty_bodies() {
    let s = st("node-edit-refusals");
    declare(&s, "alpha", "http://10.0.0.5:8199", NODE_KEY);
    let (code, _) = patch(&s, "alpha", serde_json::json!({"url": "http://127.0.0.1:8199"})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = patch(&s, "alpha", serde_json::json!({})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = patch(&s, "nobody", serde_json::json!({"name": "x"})).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // A new address that does not answer is not saved.
    let (code, _) = patch(&s, "alpha", serde_json::json!({"url": "http://192.0.2.1:1"})).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(s.store.lock().unwrap().node("alpha").unwrap().unwrap().url, "http://10.0.0.5:8199");
}

/// ⭐⭐ The whole rotation, against a real second Hydranos: the node adopts a
/// key it minted, the controller stores it, the old key is refused from then
/// on, and the new one is in the node's default.toml for the next boot.
#[tokio::test]
async fn rotating_a_nodes_key_switches_both_sides_and_retires_the_old_key() {
    let controller = st("rotate-controller");
    let node = state_from("rotate-node", &format!("[daemon]\napi_key = \"{NODE_KEY}\"\n"));
    let served = serve(crate::api::router(node.state.clone())).await;
    declare(&controller, "seedbox", &served.url, NODE_KEY);

    let resp = super::post_node_rotate_key(
        State(controller.state.clone()),
        Path("seedbox".into()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let new_key = v["api_key"].as_str().expect("the new key is returned once").to_string();
    assert_ne!(new_key, NODE_KEY);
    assert_eq!(v["health"]["online"], true, "the node answers the new key: {v}");

    assert_eq!(controller.store.lock().unwrap().node("seedbox").unwrap().unwrap().api_key, new_key);
    assert_eq!(node.state.cfg().daemon.api_key, new_key, "live on the node");
    let on_disk = std::fs::read_to_string(&node.state.config_path).unwrap();
    assert!(on_disk.contains(&new_key), "persisted for the next boot");
    assert!(!authorised(&node.state, &keyed(NODE_KEY), ""), "the old key is refused");
    assert!(authorised(&node.state, &keyed(&new_key), ""));
}

/// The confirmation accepts the pending key and nothing else; the current
/// key cannot confirm a rotation it did not see.
#[tokio::test]
async fn a_confirmation_with_any_other_key_is_refused() {
    let node = state_from("rotate-wrong", &format!("[daemon]\napi_key = \"{NODE_KEY}\"\n"));
    for k in [NODE_KEY, "", "not-the-pending-key-at-all"] {
        let mut h = HeaderMap::new();
        if !k.is_empty() {
            h.insert("X-Api-Key", k.parse().unwrap());
        }
        let resp = super::post_api_key_confirm(State(node.state.clone()), h).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{k:?}");
    }
    assert_eq!(node.state.cfg().daemon.api_key, NODE_KEY);
}

/// Starting a rotation needs the current key, like any other route.
#[tokio::test]
async fn starting_a_rotation_needs_the_current_key() {
    let node = state_from("rotate-guard", &format!("[daemon]\napi_key = \"{NODE_KEY}\"\n"));
    let resp = super::post_api_key_rotate(State(node.state.clone()), RawQuery(None), keyed("wrong")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
