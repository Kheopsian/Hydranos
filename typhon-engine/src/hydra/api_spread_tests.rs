//! A category that spreads its new torrents over several engines, here and on
//! nodes: which engine an add lands on, what it refuses, what it never touches.
//!
//! The choice itself is tested without a disk or a network in `spread::tests`;
//! what is checked here is that the routes and the add paths use it, and that a
//! category that says nothing about it behaves as it did before.

use super::testing::*;
use super::*;
use std::sync::{Arc, Mutex};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const NODE_KEY: &str = "fedcba9876543210fedcba9876543210";
const PIECE: u64 = 16_384;

/// Two extra race engines beside the built-in race and hoard.
const TWO_TUNNELS: &str = "[daemon]\napi_key = \"0123456789abcdef0123456789abcdef\"\n\
    [[engine]]\nname = \"vpn1\"\nrole = \"race\"\n\
    [[engine]]\nname = \"vpn2\"\nrole = \"race\"\n";

fn plain(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn tunnels(tag: &str) -> TestState {
    state_from(tag, TWO_TUNNELS)
}

fn torrent(name: &str, len: u64) -> Vec<u8> {
    let pieces = len.div_ceil(PIECE) as usize;
    let mut info = format!(
        "d6:lengthi{len}e4:name{}:{name}12:piece lengthi{PIECE}e6:pieces{}:",
        name.len(),
        pieces * 20
    )
    .into_bytes();
    info.extend(std::iter::repeat_n(b'Q', pieces * 20));
    info.push(b'e');
    let announce = "https://tracker.example/announce";
    let mut out = format!("d8:announce{}:{announce}4:info", announce.len()).into_bytes();
    out.extend(info);
    out.push(b'e');
    out
}

/// The folder a category files into, with the payload of `name` already in it,
/// so a seed-mode add has its data.
fn folder(s: &TestState, name: &str, len: u64) -> String {
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), vec![5u8; len as usize]).unwrap();
    dir.to_string_lossy().into_owned()
}

fn category(s: &TestState, name: &str, doc: serde_json::Value) {
    let mut all: serde_json::Map<String, serde_json::Value> = s
        .store
        .lock()
        .unwrap()
        .meta_doc("categories")
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default();
    all.insert(name.to_string(), doc);
    s.store
        .lock()
        .unwrap()
        .put_meta("categories", &serde_json::to_string(&all).unwrap())
        .unwrap();
}

/// A torrent straight into an engine, to give it something to weigh.
fn preload(s: &TestState, engine: &str, name: &str, len: u64) {
    let dir = folder(s, name, len);
    s.engines
        .get(engine)
        .expect("engine")
        .manager
        .add_torrent_bytes(&torrent(name, len), &dir, true, true)
        .unwrap_or_else(|e| panic!("preload {name}: {e}"));
}

/// Add through the funnel every add path ends in, asking for no engine.
fn add(s: &TestState, cat: &str, name: &str, len: u64) -> Result<(), String> {
    let dir = folder(s, name, len);
    add_torrent_bytes(&s.state, &torrent(name, len), cat, &dir, "", true, true, "").map(|_| ())
}

/// Whether `engine` holds the torrent `add` made of (`name`, `len`).
fn holds(s: &TestState, engine: &str, name: &str, len: u64) -> bool {
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&torrent(name, len)).unwrap();
    s.engines
        .get(engine)
        .expect("engine")
        .manager
        .get(&meta.info_hash)
        .is_some()
}

fn held(s: &TestState, engine: &str) -> usize {
    s.engines.get(engine).expect("engine").manager.len()
}

// ---- the default is the old behaviour ------------------------------------

/// ⭐ The point that matters most: a category that says nothing about
/// spreading, on an install with one engine per role, files where it always did.
#[test]
fn a_category_without_the_new_keys_still_goes_to_its_mode_engine() {
    let s = plain("spread-default");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "films",
        serde_json::json!({"save_path": dir, "mode": "hoard"}),
    );
    category(
        &s,
        "fast",
        serde_json::json!({"save_path": dir, "mode": "race"}),
    );

    add(&s, "films", "a", PIECE).unwrap();
    add(&s, "fast", "b", PIECE).unwrap();
    add(&s, "fast", "c", PIECE).unwrap();

    // Each one where its category's mode says, not merely the right counts.
    assert!(
        holds(&s, "hoard", "a", PIECE),
        "the hoard category's torrent"
    );
    assert!(holds(&s, "race", "b", PIECE) && holds(&s, "race", "c", PIECE));
    assert_eq!((held(&s, "hoard"), held(&s, "race")), (1, 2));
    // And the lookup the other callers use is the old one, to the letter.
    assert_eq!(placement(&s, "films", "").0, "hoard");
    assert_eq!(placement(&s, "fast", "").0, "race");
    assert_eq!(placement(&s, "no-such", "").0, "race");
}

/// The 3.x keys are dead and stay dead: a stored category carrying them behaves
/// as one that does not, on a document written by 3.x.
#[test]
fn the_3x_routing_keys_are_still_ignored() {
    let s = plain("spread-3x-dead");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "old",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "strategy": "fill_then_next", "placement": ["hoard"],
            "agents": {"hoard": "/elsewhere"}, "min_free_bytes": i64::MAX,
        }),
    );
    add(&s, "old", "a", PIECE).expect("min_free_bytes does not refuse an add");
    assert_eq!((held(&s, "race"), held(&s, "hoard")), (1, 0));

    // A save through the API keeps them in the document, unread.
    let kept = categories_map(&s).remove("old").unwrap();
    assert_eq!(kept.strategy, "fill_then_next");
    assert!(kept.targets.is_empty() && kept.spread.is_empty() && kept.reserve_bytes == 0);
}

// ---- local spreading ------------------------------------------------------

/// ⭐ Spreading is something the operator asks for. A category with none of the
/// three keys files by its mode alone even when several local engines share the
/// role (a prod node runs a second hoard engine that must not start taking the
/// hoard adds).
#[test]
fn a_category_without_any_key_ignores_the_other_engines_of_its_role() {
    let s = tunnels("spread-no-key-two-engines");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "fast",
        serde_json::json!({"save_path": dir, "mode": "race"}),
    );
    category(
        &s,
        "empty-keys",
        serde_json::json!({"save_path": dir, "mode": "race", "targets": [], "reserve_bytes": 0}),
    );
    for n in ["a", "b", "c"] {
        add(&s, "fast", n, PIECE).unwrap();
    }
    add(&s, "empty-keys", "d", PIECE).unwrap();
    assert_eq!(
        (held(&s, "race"), held(&s, "vpn1"), held(&s, "vpn2")),
        (4, 0, 0)
    );
}

/// Any one of the three keys turns it on: here only a reserve.
#[test]
fn a_reserve_alone_turns_the_spread_on() {
    let s = tunnels("spread-reserve-only");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "fast",
        serde_json::json!({"save_path": dir, "mode": "race", "reserve_bytes": 1}),
    );
    for n in ["a", "b", "c"] {
        add(&s, "fast", n, PIECE).unwrap();
    }
    assert_eq!(
        (held(&s, "race"), held(&s, "vpn1"), held(&s, "vpn2")),
        (1, 1, 1)
    );
}

/// No `targets` but a `spread`: every local engine of the category's mode shares the adds, and
/// the engine of the OTHER mode is never one of them.
#[test]
fn without_targets_the_engines_of_the_mode_share_the_adds() {
    let s = tunnels("spread-role");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "fast",
        serde_json::json!({"save_path": dir, "mode": "race", "spread": "bytes"}),
    );
    for n in ["a", "b", "c", "d", "e", "f"] {
        add(&s, "fast", n, PIECE).unwrap();
    }
    assert_eq!(
        (
            held(&s, "race"),
            held(&s, "vpn1"),
            held(&s, "vpn2"),
            held(&s, "hoard")
        ),
        (2, 2, 2, 0)
    );
}

fn two_targets(s: &TestState, spread: &str) {
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["vpn1", "vpn2"], "spread": spread,
        }),
    );
    // Neither is `race`, where the old routing sends everything: a pass that
    // still lands there is not choosing. vpn1 holds three small torrents (more
    // of them, fewer bytes), vpn2 one big one.
    for n in ["s1", "s2", "s3"] {
        preload(s, "vpn1", n, PIECE);
    }
    preload(s, "vpn2", "big", 8 * PIECE);
}

#[test]
fn count_sends_it_to_the_engine_holding_the_fewest_torrents() {
    let s = tunnels("spread-count");
    two_targets(&s, "count");
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!((held(&s, "vpn1"), held(&s, "vpn2")), (3, 2));
}

#[test]
fn bytes_sends_it_to_the_engine_holding_the_fewest_bytes() {
    let s = tunnels("spread-bytes");
    two_targets(&s, "bytes");
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!((held(&s, "vpn1"), held(&s, "vpn2")), (4, 1));
}

/// Bytes is the default when `spread` is absent.
#[test]
fn bytes_is_the_default_strategy() {
    let s = tunnels("spread-bytes-default");
    two_targets(&s, "");
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!((held(&s, "vpn1"), held(&s, "vpn2")), (4, 1));
}

#[test]
fn fill_keeps_the_first_target_in_the_order_written() {
    let s = tunnels("spread-fill");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["vpn2", "race", "vpn1"], "spread": "fill",
        }),
    );
    preload(&s, "vpn2", "heavy", 8 * PIECE);
    for n in ["a", "b", "c"] {
        add(&s, "c", n, PIECE).unwrap();
    }
    // The emptiest engine is not what fill looks at: the first one is.
    assert_eq!(
        (held(&s, "vpn2"), held(&s, "race"), held(&s, "vpn1")),
        (4, 0, 0)
    );
}

/// Every engine of a node shares one filesystem at the category's folder, so
/// free space cannot tell local engines apart: the tie goes to the first target.
#[test]
fn free_space_on_one_filesystem_is_a_tie_and_goes_to_the_first_target() {
    let s = tunnels("spread-free-local");
    two_targets(&s, "free_space");
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!((held(&s, "vpn1"), held(&s, "vpn2")), (4, 1));
}

#[test]
fn a_reserve_nothing_can_meet_refuses_the_add_and_adds_nothing() {
    let s = tunnels("spread-reserve-all");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["race", "vpn1"], "reserve_bytes": u64::MAX,
        }),
    );
    let e = add(&s, "c", "new", PIECE).unwrap_err();
    assert!(e.contains("no target can take it"), "{e}");
    assert_eq!((held(&s, "race"), held(&s, "vpn1")), (0, 0));
}

/// A reserve that the disk meets changes nothing about the choice.
#[test]
fn a_reserve_the_disk_meets_does_not_get_in_the_way() {
    let s = tunnels("spread-reserve-ok");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["vpn1", "race"], "spread": "fill", "reserve_bytes": 1,
        }),
    );
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!((held(&s, "vpn1"), held(&s, "race")), (1, 0));
}

#[test]
fn a_target_that_no_longer_exists_is_ignored_and_the_others_are_used() {
    let s = tunnels("spread-gone");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["deleted-engine", "ghost-node:vpn1", "vpn2"],
        }),
    );
    add(&s, "c", "new", PIECE).unwrap();
    assert_eq!(held(&s, "vpn2"), 1);

    // All of them gone: refused, not sent somewhere else.
    category(
        &s,
        "d",
        serde_json::json!({"save_path": dir, "mode": "race", "targets": ["deleted-engine"]}),
    );
    let e = add(&s, "d", "other", PIECE).unwrap_err();
    assert!(e.contains("no usable target"), "{e}");
}

#[test]
fn an_explicit_engine_beats_the_categorys_targets() {
    let s = tunnels("spread-explicit");
    let dir = folder(&s, "new", PIECE);
    category(
        &s,
        "c",
        serde_json::json!({
            "save_path": dir, "mode": "race",
            "targets": ["vpn1"], "reserve_bytes": u64::MAX,
        }),
    );
    // The reserve would refuse every spread add, and the engine is not even a
    // target: the caller named it, so it goes there.
    add_torrent_bytes(
        &s.state,
        &torrent("new", PIECE),
        "c",
        &dir,
        "",
        true,
        true,
        "vpn2",
    )
    .expect("an explicit engine wins");
    assert_eq!(held(&s, "vpn2"), 1);
}

// ---- registering a category ----------------------------------------------

async fn create(s: &TestState, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
    let r = category_create(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    (r.status(), body_json(r).await)
}

#[tokio::test]
async fn a_category_is_saved_and_listed_with_its_spread() {
    let s = tunnels("spread-register");
    let (status, _) = create(
        &s,
        serde_json::json!({
            "name": "fast", "save_path": "/d", "mode": "race",
            "targets": ["vpn1", " vpn2 "], "spread": "Free_Space", "reserve_bytes": 5_000_000_000u64,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let list =
        body_json(get_categories(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    let fast = &list[0];
    assert_eq!(fast["targets"], serde_json::json!(["vpn1", "vpn2"]));
    assert_eq!(fast["spread"], "free_space");
    assert_eq!(fast["reserve_bytes"], 5_000_000_000u64);
}

#[tokio::test]
async fn a_category_without_the_keys_lists_without_them() {
    let s = tunnels("spread-register-none");
    let (status, _) = create(
        &s,
        serde_json::json!({"name": "plain", "save_path": "/d", "mode": "race"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let list =
        body_json(get_categories(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
    for k in ["targets", "spread", "reserve_bytes"] {
        assert!(list[0].get(k).is_none(), "{k} appeared: {}", list[0]);
    }
}

#[tokio::test]
async fn a_target_whose_role_is_not_the_mode_is_refused() {
    let s = tunnels("spread-role-mismatch");
    let (status, body) = create(
        &s,
        serde_json::json!({
            "name": "fast", "save_path": "/d", "mode": "race", "targets": ["vpn1", "hoard"],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let e = body["error"].as_str().unwrap();
    assert!(
        e.contains("hoard") && e.contains("role") && e.contains("mode is race"),
        "{e}"
    );
    assert!(categories_map(&s).is_empty(), "nothing was saved");
}

#[tokio::test]
async fn an_unknown_target_or_a_bad_value_is_refused() {
    let s = tunnels("spread-bad-values");
    for (body, needle) in [
        (serde_json::json!({"targets": ["vpn9"]}), "vpn9"),
        (serde_json::json!({"targets": ["nowhere:vpn1"]}), "no node"),
        (serde_json::json!({"targets": ["vpn1", "vpn1"]}), "twice"),
        (serde_json::json!({"targets": "vpn1"}), "list of strings"),
        (
            serde_json::json!({"spread": "least_load"}),
            "spread must be",
        ),
        (
            serde_json::json!({"reserve_bytes": "10GB"}),
            "reserve_bytes",
        ),
        (serde_json::json!({"reserve_bytes": -1}), "reserve_bytes"),
    ] {
        let mut full = serde_json::json!({"name": "x", "save_path": "/d", "mode": "race"});
        for (k, v) in body.as_object().unwrap() {
            full[k] = v.clone();
        }
        let (status, resp) = create(&s, full).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            resp["error"].as_str().unwrap().contains(needle),
            "{body}: {resp}"
        );
    }
}

/// Changing the mode of a category whose targets no longer match it is refused
/// too, not discovered at the next add.
#[tokio::test]
async fn an_update_that_leaves_a_target_in_the_wrong_role_is_refused() {
    let s = tunnels("spread-update-role");
    let (status, _) = create(
        &s,
        serde_json::json!({"name": "fast", "save_path": "/d", "mode": "race", "targets": ["vpn1"]}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let r = category_update(
        State(s.state.clone()),
        Path("fast".to_string()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"mode": "hoard"}).to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // A change that touches neither is not held up by the targets.
    let r = category_update(
        State(s.state.clone()),
        Path("fast".to_string()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"save_path": "/elsewhere"}).to_string(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(categories_map(&s)["fast"].targets, vec!["vpn1"]);
}

/// The shim's own category edits keep the new keys: the write path normalises
/// every entry through a typed struct, and a field it does not know is dropped.
#[tokio::test]
async fn a_shim_edit_keeps_the_spread_keys() {
    let s = tunnels("spread-shim-edit");
    let (status, _) = create(
        &s,
        serde_json::json!({
            "name": "fast", "save_path": "/d", "mode": "race",
            "targets": ["vpn1", "vpn2"], "spread": "count", "reserve_bytes": 7,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    edit_categories(&s, |doc| {
        doc.get_mut("fast").unwrap()["save_path"] = "/moved".into();
    })
    .unwrap();
    let c = categories_map(&s).remove("fast").unwrap();
    assert_eq!(c.save_path, "/moved");
    assert_eq!(
        (c.targets, c.spread, c.reserve_bytes),
        (vec!["vpn1".into(), "vpn2".into()], "count".into(), 7)
    );
}

// ---- nodes ----------------------------------------------------------------

struct Served {
    url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn serve(app: axum::Router) -> Served {
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
    }
}

type Uploads = Arc<Mutex<Vec<Vec<(String, String)>>>>;

/// A node with one engine `vpn1` (role race). `measure` is what it answers for
/// the engine's measure, or `None` for a node too old to have the route.
async fn node(measure: Option<serde_json::Value>, uploads: Uploads) -> Served {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/api/engines",
            get(|| async { Json(serde_json::json!([{"id": "vpn1", "role": "race"}])) }),
        )
        .route(
            "/api/engines/:id/measure",
            get(move || {
                let m = measure.clone();
                async move {
                    match m {
                        Some(m) => (StatusCode::OK, Json(m)),
                        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({}))),
                    }
                }
            }),
        )
        .route(
            "/api/torrents/upload",
            post(move |mut mp: axum::extract::Multipart| {
                let uploads = uploads.clone();
                async move {
                    let mut fields = Vec::new();
                    while let Ok(Some(f)) = mp.next_field().await {
                        let name = f.name().unwrap_or_default().to_string();
                        let value = if name == "torrents" {
                            format!("{} bytes", f.bytes().await.map(|b| b.len()).unwrap_or(0))
                        } else {
                            f.text().await.unwrap_or_default()
                        };
                        fields.push((name, value));
                    }
                    uploads.lock().unwrap().push(fields);
                    Json(serde_json::json!({
                        "info_hash": "ignored", "name": "from-the-node",
                        "engine": "vpn1", "mode": "race",
                    }))
                }
            }),
        );
    serve(app).await
}

fn declare(s: &TestState, name: &str, url: &str) {
    s.store
        .lock()
        .unwrap()
        .put_node(&crate::store::Node {
            name: name.into(),
            url: url.into(),
            api_key: NODE_KEY.into(),
            enabled: true,
            added_at: 1_700_000_000,
        })
        .unwrap();
}

/// POST one file to the real router's native upload.
async fn upload(s: &TestState, fields: &[(&str, &str)], name: &str, len: u64) -> serde_json::Value {
    let app = router(s.state.clone());
    let served = serve(app).await;
    let part = reqwest::multipart::Part::bytes(torrent(name, len))
        .file_name(format!("{name}.torrent"))
        .mime_str("application/x-bittorrent")
        .unwrap();
    let mut form = reqwest::multipart::Form::new().part("torrents", part);
    for (k, v) in fields {
        form = form.text(k.to_string(), v.to_string());
    }
    let res = reqwest::Client::new()
        .post(format!("{}/api/torrents/upload", served.url))
        .header("X-API-Key", KEY)
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    let mut v: serde_json::Value = res.json().await.unwrap();
    v["_status"] = status.into();
    v
}

/// A category over `race` and the node's `nas:vpn1`; the local engine already
/// holds more bytes than the node, so the node is the one to pick.
async fn over_a_node(
    tag: &str,
    measure: Option<serde_json::Value>,
    extra: serde_json::Value,
) -> (TestState, Served, Uploads) {
    let s = plain(tag);
    let uploads: Uploads = Default::default();
    let n = node(measure, uploads.clone()).await;
    declare(&s, "nas", &n.url);
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    let mut doc = serde_json::json!({
        "save_path": dir, "mode": "race", "targets": ["race", "nas:vpn1"],
    });
    for (k, v) in extra.as_object().unwrap() {
        doc[k] = v.clone();
    }
    category(&s, "films", doc);
    preload(&s, "race", "already-here", 4 * PIECE);
    (s, n, uploads)
}

fn quiet() -> serde_json::Value {
    serde_json::json!({"torrents": 0, "bytes": 0, "free_bytes": 1_000_000_000u64})
}

/// A target on a node is chosen like any other, and the torrent is sent there:
/// its bytes, the category and the node's engine, and the answer says where.
#[tokio::test]
async fn a_node_target_that_wins_gets_the_torrent_and_the_answer_says_so() {
    let (s, _n, uploads) = over_a_node("spread-node", Some(quiet()), serde_json::json!({})).await;
    let answer = upload(
        &s,
        &[("category", "films"), ("paused", "true")],
        "fresh",
        2 * PIECE,
    )
    .await;
    assert_eq!(answer["_status"], 200, "{answer}");
    assert_eq!(answer["node"], "nas");
    assert_eq!(answer["engine"], "vpn1");
    assert_eq!(answer["name"], "from-the-node");

    let sent = uploads.lock().unwrap();
    assert_eq!(sent.len(), 1, "one upload reached the node");
    let field = |k: &str| sent[0].iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(field("category").as_deref(), Some("films"));
    assert_eq!(field("engine").as_deref(), Some("vpn1"));
    assert_eq!(field("paused").as_deref(), Some("true"));
    assert!(field("torrents").unwrap().ends_with(" bytes"));
    // No folder of this machine is sent: the node uses its own category's.
    assert_eq!(field("save_path"), None);
    assert_eq!(held(&s, "race"), 1, "nothing new here");
}

/// A burst does not all go to the node: what was just sent counts at once.
#[tokio::test]
async fn a_burst_is_spread_not_sent_to_one_target() {
    let (s, _n, uploads) = over_a_node(
        "spread-burst",
        Some(serde_json::json!({"torrents": 0, "bytes": 0, "free_bytes": 1_000_000_000u64})),
        serde_json::json!({"spread": "count"}),
    )
    .await;
    // race holds 1; the node 0. Count: node, then (1 vs 1, first) race, then node.
    let a = upload(&s, &[("category", "films")], "n1", PIECE).await;
    let b = upload(&s, &[("category", "films")], "n2", PIECE).await;
    let c = upload(&s, &[("category", "films")], "n3", PIECE).await;
    assert_eq!(a["node"], "nas");
    assert!(b.get("node").is_none(), "{b}");
    assert_eq!(c["node"], "nas");
    assert_eq!(uploads.lock().unwrap().len(), 2);
}

/// free_space reads the NODE's folder, and picks the roomier machine.
#[tokio::test]
async fn free_space_picks_the_node_with_more_room() {
    let (s, _n, uploads) = over_a_node(
        "spread-free-node",
        Some(serde_json::json!({"torrents": 99, "bytes": 99, "free_bytes": u64::MAX / 2})),
        serde_json::json!({"spread": "free_space"}),
    )
    .await;
    let a = upload(&s, &[("category", "films")], "n1", PIECE).await;
    assert_eq!(a["node"], "nas", "{a}");
    assert_eq!(uploads.lock().unwrap().len(), 1);
}

/// A node that cannot be measured stays a candidate: here the local engine is
/// below the reserve, and the node is the only one left.
#[tokio::test]
async fn a_node_that_cannot_be_measured_stays_a_candidate() {
    let (s, _n, uploads) = over_a_node(
        "spread-unmeasurable",
        None,
        serde_json::json!({"reserve_bytes": u64::MAX}),
    )
    .await;
    let a = upload(&s, &[("category", "films")], "n1", PIECE).await;
    assert_eq!(a["node"], "nas", "{a}");
    assert_eq!(uploads.lock().unwrap().len(), 1);
}

/// The same node, measured and below the reserve, is out: the add is refused
/// and nothing is sent anywhere.
#[tokio::test]
async fn every_target_below_the_reserve_refuses_and_sends_nothing() {
    let (s, _n, uploads) = over_a_node(
        "spread-all-full",
        Some(serde_json::json!({"torrents": 0, "bytes": 0, "free_bytes": 10})),
        serde_json::json!({"reserve_bytes": u64::MAX}),
    )
    .await;
    let a = upload(&s, &[("category", "films")], "n1", PIECE).await;
    assert_eq!(a["_status"], 400, "{a}");
    assert!(
        a["error"]
            .as_str()
            .unwrap()
            .contains("no target can take it"),
        "{a}"
    );
    assert!(uploads.lock().unwrap().is_empty());
    assert_eq!(held(&s, "race"), 1);
}

/// An explicit engine is never second-guessed, not even for a node's sake.
#[tokio::test]
async fn an_explicit_engine_stays_local_whatever_the_targets() {
    let (s, _n, uploads) =
        over_a_node("spread-explicit-node", Some(quiet()), serde_json::json!({})).await;
    let a = upload(
        &s,
        &[("category", "films"), ("engine", "hoard")],
        "n1",
        PIECE,
    )
    .await;
    assert_eq!(a["engine"], "hoard", "{a}");
    assert!(uploads.lock().unwrap().is_empty());
    assert_eq!(held(&s, "hoard"), 1);
}

/// The qBittorrent shim lists the torrents of THIS node's engines only, so a
/// client that adds and then follows the hash would lose one sent to a node.
/// Its adds (the funnel) therefore stay on local targets, even when a node
/// would win on every measure.
#[tokio::test]
async fn the_shim_path_never_sends_an_add_to_a_node() {
    let (s, _n, uploads) =
        over_a_node("spread-shim-local", Some(quiet()), serde_json::json!({})).await;
    add(&s, "films", "via-shim", PIECE).unwrap();
    assert_eq!(held(&s, "race"), 2);
    assert!(uploads.lock().unwrap().is_empty());

    // With only node targets, the shim path refuses, and says why.
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    category(
        &s,
        "remote-only",
        serde_json::json!({"save_path": dir, "mode": "race", "targets": ["nas:vpn1"]}),
    );
    let e = add(&s, "remote-only", "other", PIECE).unwrap_err();
    assert!(e.contains("no usable target") && e.contains("node"), "{e}");
    assert!(uploads.lock().unwrap().is_empty());
}

/// The register-time check reaches the node: its engine must exist and play
/// the category's role.
#[tokio::test]
async fn a_node_target_is_checked_against_the_node() {
    let s = plain("spread-node-register");
    let n = node(Some(quiet()), Default::default()).await;
    declare(&s, "nas", &n.url);
    let ok = create(
        &s,
        serde_json::json!({"name": "a", "save_path": "/d", "mode": "race", "targets": ["race", "nas:vpn1"]}),
    )
    .await;
    assert_eq!(ok.0, StatusCode::CREATED, "{}", ok.1);
    let (st, body) = create(
        &s,
        serde_json::json!({"name": "b", "save_path": "/d", "mode": "race", "targets": ["nas:vpn9"]}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("no engine vpn9"),
        "{body}"
    );
    let (st, body) = create(
        &s,
        serde_json::json!({"name": "c", "save_path": "/d", "mode": "hoard", "targets": ["nas:vpn1"]}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("role race"),
        "{body}"
    );
}

// ---- the two small routes -------------------------------------------------

#[tokio::test]
async fn the_measure_route_reports_what_an_engine_holds() {
    let s = plain("spread-measure");
    let dir = s.dir.join("data").to_string_lossy().into_owned();
    std::fs::create_dir_all(&dir).unwrap();
    category(
        &s,
        "films",
        serde_json::json!({"save_path": dir, "mode": "race"}),
    );
    preload(&s, "race", "a", 3 * PIECE);
    preload(&s, "race", "b", PIECE);

    let r = get_engine_measure(
        State(s.state.clone()),
        Path("race".into()),
        RawQuery(Some("category=films".into())),
        keyed(KEY),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = body_json(r).await;
    assert_eq!(v["engine"], "race");
    assert_eq!(v["torrents"], 2);
    assert_eq!(v["bytes"], 4 * PIECE);
    assert!(v["free_bytes"].as_u64().unwrap() > 0, "{v}");

    // No such category here: the figures, and no free space to claim.
    let v = body_json(
        get_engine_measure(
            State(s.state.clone()),
            Path("race".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await,
    )
    .await;
    assert!(v["free_bytes"].is_null(), "{v}");

    let r = get_engine_measure(
        State(s.state.clone()),
        Path("nope".into()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let r = get_engine_measure(
        State(s.state.clone()),
        Path("race".into()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_targets_route_lists_engines_here_and_on_nodes_in_the_strings_targets_takes() {
    let s = tunnels("spread-targets-list");
    let n = node(Some(quiet()), Default::default()).await;
    declare(&s, "nas", &n.url);
    declare(&s, "dead", "http://127.0.0.1:1");
    let v =
        body_json(get_category_targets(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let rows: Vec<(String, Option<String>, String)> = v["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["target"].as_str().unwrap().to_string(),
                t["node"].as_str().map(str::to_string),
                t["role"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("race".to_string(), None, "race".to_string()),
            ("hoard".to_string(), None, "hoard".to_string()),
            ("vpn1".to_string(), None, "race".to_string()),
            ("vpn2".to_string(), None, "race".to_string()),
            (
                "nas:vpn1".to_string(),
                Some("nas".to_string()),
                "race".to_string()
            ),
        ]
    );
    assert_eq!(v["targets"][4]["engine"], "vpn1");
    assert_eq!(v["unreachable"][0]["node"], "dead");
}
