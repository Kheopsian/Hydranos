//! Moving a torrent from one engine to another and removing it through the
//! qBittorrent shim: what stays, what goes, and what is put back when a step
//! refuses.

use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";
const SIZE: u64 = 16_384;

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi{SIZE}e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        "Q".repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

/// A paused seed on `engine`, its file on disk under `dir`.
fn seed(s: &TestState, engine: &str, name: &str, dir: &std::path::Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(name), vec![5u8; SIZE as usize]).unwrap();
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
    .expect("added")
    .0
}

async fn to_engine(
    s: &TestState,
    hash: &str,
    query: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let resp = post_torrent_engine(
        State(s.state.clone()),
        Path(hash.to_string()),
        RawQuery(Some(query.to_string())),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

fn sessions(s: &TestState, hash: &str) -> Vec<String> {
    s.store.lock().unwrap().sessions_of(hash)
}

/// ⭐ The torrent crosses with its own state and its data stays where it is:
/// its counters, its stopped flag, and the store's row follow it, and the
/// source lets go.
#[tokio::test]
async fn a_torrent_moved_to_another_engine_arrives_as_it_was() {
    let s = st("eng-move");
    let dir = s.dir.join("payload");
    let hash = seed(&s, "race", "mv.bin", &dir);
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    s.engines
        .get("race")
        .unwrap()
        .manager
        .get(&ih)
        .unwrap()
        .restore_lifetime(5_000, 700);

    let (code, v) = to_engine(&s, &hash, "", r#"{"engine":"hoard"}"#).await;

    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        serde_json::json!({"status": "ok", "from": "race", "to": "hoard"})
    );
    assert!(s.engines.get("race").unwrap().manager.get(&ih).is_none());
    let t = s
        .engines
        .get("hoard")
        .unwrap()
        .manager
        .get(&ih)
        .expect("in the target");
    assert_eq!(
        t.total_uploaded.load(std::sync::atomic::Ordering::Relaxed),
        5_000
    );
    assert!(
        t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
        "a stopped torrent stays stopped"
    );
    assert_eq!(*t.save_path.read(), dir, "the data is where it was");
    assert_eq!(sessions(&s, &hash), ["hoard"], "the store follows");
    assert_eq!(
        std::fs::read(dir.join("mv.bin")).unwrap(),
        vec![5u8; SIZE as usize]
    );
}

/// ⭐ The folder the store knows is the one the torrent arrives with, and with
/// no folder in the store the engine's own is kept; it arrives stopped if
/// either the engine's record or the store says it is.
#[tokio::test]
async fn a_move_between_engines_takes_the_stores_folder_and_stops_it_if_either_says_so() {
    let s = st("eng-move-state");
    let dir = s.dir.join("payload");
    let ih = |h: &str| typhon_engine::torrent::hex_decode(h).unwrap();
    let landed = |h: &str| {
        let t = s.engines.get("hoard").unwrap().manager.get(&ih(h)).unwrap();
        let paused = t.is_paused.load(std::sync::atomic::Ordering::Relaxed);
        let folder = t.save_path.read().clone();
        (folder, paused)
    };
    let resume = |h: &str| {
        s.engines
            .get("race")
            .unwrap()
            .manager
            .get(&ih(h))
            .unwrap()
            .is_paused
            .store(false, std::sync::atomic::Ordering::Relaxed)
    };
    let store_says = |h: &str, folder: &str, paused: bool| {
        let store = s.store.lock().unwrap();
        store.set_paused(h, "race", paused).unwrap();
        store.execute_batch_for_tests(&format!(
            "UPDATE torrents SET save_path = '{folder}' WHERE info_hash = '{h}';"
        ));
    };
    let elsewhere = s.dir.join("elsewhere");

    // The store names another folder and says stopped; the engine runs.
    let a = seed(&s, "race", "st1.bin", &dir);
    resume(&a);
    store_says(&a, elsewhere.to_str().unwrap(), true);
    let (code, v) = to_engine(&s, &a, "", r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        landed(&a),
        (elsewhere.clone(), true),
        "the store's folder, stopped by the store"
    );

    // The store has no folder and says running; the engine is stopped.
    let b = seed(&s, "race", "st2.bin", &dir);
    store_says(&b, "", false);
    let (code, v) = to_engine(&s, &b, "", r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(
        landed(&b),
        (dir.clone(), true),
        "its own folder, stopped by the engine"
    );

    // Neither says stopped: it arrives running.
    let c = seed(&s, "race", "st3.bin", &dir);
    resume(&c);
    store_says(&c, dir.to_str().unwrap(), false);
    let (code, v) = to_engine(&s, &c, "", r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(landed(&c), (dir.clone(), false));
}

/// Everything that is not a move is refused with its reason, and nothing
/// moves: no engine named, the one it is in, one that is not here, a torrent
/// nobody holds, the copy that is not in the engine named as the source, and
/// a torrent whose metainfo the store has lost.
#[tokio::test]
async fn a_move_between_engines_that_cannot_happen_is_refused_and_moves_nothing() {
    let s = st("eng-move-bad");
    let dir = s.dir.join("payload");
    let hash = seed(&s, "race", "bad.bin", &dir);
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();

    let (code, _) = to_engine(&s, &"0".repeat(40), "", r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    let (code, _) = to_engine(&s, &hash, "engine=hoard", r#"{"engine":"race"}"#).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "the copy named is not there");
    for (body, error) in [
        ("{}", "engine is required"),
        ("not json", "engine is required"),
        (r#"{"engine":"race"}"#, "already in race"),
        (
            r#"{"engine":"nowhere"}"#,
            "no engine named nowhere on this node",
        ),
    ] {
        let (code, v) = to_engine(&s, &hash, "", body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}: {v}");
        assert_eq!(v["error"], error, "{body}");
    }
    let (code, v) = to_engine(&s, &hash, "engine=race", r#"{"engine":"race"}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "already in race");

    assert!(s.engines.get("race").unwrap().manager.get(&ih).is_some());
    assert!(s.engines.get("hoard").unwrap().manager.get(&ih).is_none());
    assert_eq!(sessions(&s, &hash), ["race"]);

    // The store loses the metainfo: there is nothing to re-add it with.
    s.store
        .lock()
        .unwrap()
        .execute_batch_for_tests(&format!("DELETE FROM torrents WHERE info_hash = '{hash}';"));
    let (code, v) = to_engine(&s, &hash, "", r#"{"engine":"hoard"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT, "{v}");
    assert_eq!(
        v["error"],
        "no metainfo in the store; cannot re-add it elsewhere"
    );

    assert!(s.engines.get("race").unwrap().manager.get(&ih).is_some());
    assert!(s.engines.get("hoard").unwrap().manager.get(&ih).is_none());
}

/// ⭐ The store refuses to record the move AFTER the target took the torrent:
/// the torrent is put back where it was, so the engines and the store agree,
/// and the caller is told.
#[tokio::test]
async fn a_store_that_refuses_the_move_has_the_torrent_put_back() {
    let s = st("eng-move-undo");
    let dir = s.dir.join("payload");
    let hash = seed(&s, "race", "undo.bin", &dir);
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    s.store.lock().unwrap().execute_batch_for_tests(
        "CREATE TRIGGER refuse_session BEFORE UPDATE OF session ON torrents \
         BEGIN SELECT RAISE(ABORT, 'database disk image is malformed'); END;",
    );

    let (code, v) = to_engine(&s, &hash, "", r#"{"engine":"hoard"}"#).await;

    assert!(code.is_server_error(), "{code}: {v}");
    assert!(
        s.engines.get("race").unwrap().manager.get(&ih).is_some(),
        "back in the source"
    );
    assert!(
        s.engines.get("hoard").unwrap().manager.get(&ih).is_none(),
        "and gone from the target"
    );
    assert_eq!(sessions(&s, &hash), ["race"]);
    assert_eq!(
        std::fs::read(dir.join("undo.bin")).unwrap(),
        vec![5u8; SIZE as usize],
        "the data was never touched"
    );
}

async fn shim_delete(s: &TestState, form: &str) -> (StatusCode, String) {
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/torrents/delete")
        .header("X-API-Key", KEY)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_string()))
        .unwrap();
    let resp = router(s.state.clone()).oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, String::from_utf8_lossy(&bytes).into_owned())
}

/// ⭐ The shim's delete takes the files only when asked to: `deleteFiles` true
/// or 1 takes the payload, anything else keeps it, and the torrent is gone
/// from the engine and the store either way. A hash it never had is a plain 200.
#[tokio::test]
async fn the_shims_delete_takes_the_files_only_when_asked() {
    let s = st("eng-shim-delete");
    let dir = s.dir.join("payload");
    let kept = seed(&s, "race", "kept.bin", &dir);
    let gone = seed(&s, "race", "gone.bin", &dir);
    let gone_one = seed(&s, "hoard", "gone1.bin", &dir);
    let also_kept = seed(&s, "hoard", "kept2.bin", &dir);
    let held = |h: &str| find_torrent(&s.state, h).is_some();

    let (code, body) = shim_delete(&s, &format!("hashes={kept}&deleteFiles=false")).await;
    assert_eq!((code, body.as_str()), (StatusCode::OK, ""));
    assert!(!held(&kept) && sessions(&s, &kept).is_empty(), "removed");
    assert!(dir.join("kept.bin").exists(), "but its files stay");

    let (code, _) = shim_delete(&s, &format!("hashes={gone}&deleteFiles=true")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!held(&gone) && sessions(&s, &gone).is_empty());
    assert!(!dir.join("gone.bin").exists(), "true takes the payload");

    let (code, _) = shim_delete(&s, &format!("hashes={gone_one}&deleteFiles=1")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!dir.join("gone1.bin").exists(), "1 takes it too");

    let (code, _) = shim_delete(&s, &format!("hashes={also_kept}&deleteFiles=yes")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(!held(&also_kept));
    assert!(
        dir.join("kept2.bin").exists(),
        "a word that is not true keeps the files"
    );

    let (code, body) =
        shim_delete(&s, &format!("hashes={}&deleteFiles=true", "e".repeat(40))).await;
    assert_eq!(
        (code, body.as_str()),
        (StatusCode::OK, ""),
        "a hash it never had"
    );
}
