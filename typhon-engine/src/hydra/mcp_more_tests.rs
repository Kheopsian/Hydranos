//! Two tools of the agent interface that act on a node's torrents and had no
//! test that reads the result back: `purge_race` (destructive, so absent
//! unless the operator allows it) and `add_torrent` (what it refuses, and what
//! a magnet leaves behind).

use super::*;
use crate::api::testing::{body_json, keyed, put_category, state_from, TestState};

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str, extra: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"))
}

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        "P".repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

fn add(s: &TestState, engine: &str, name: &str) -> String {
    let dir = s.dir.join("payload");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), vec![1u8; 16384]).unwrap();
    api::add_torrent_bytes(
        &s.state,
        &torrent_bytes(name),
        "",
        dir.to_str().unwrap(),
        "",
        true,
        true,
        engine,
    )
    .unwrap_or_else(|e| panic!("add {name}: {e}"))
    .0
}

async fn tool(s: &TestState, name: &str, args: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                      "params": {"name": name, "arguments": args}});
    let resp = post_mcp(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        axum::body::Bytes::from(body.to_string()),
    )
    .await;
    let v = body_json(resp).await;
    assert!(v.get("error").is_none(), "{name} was a protocol error: {v}");
    v["result"].clone()
}

fn failure(r: &Value) -> String {
    assert_eq!(r["isError"], true, "{r}");
    r["content"][0]["text"].as_str().unwrap().to_string()
}

/// ⭐ Purging the race copy goes through the tool only when the operator
/// allowed destructive tools; then it takes the race copy of each hash, says
/// which hashes it could not (a hoard torrent is not a race one, an unknown
/// hash is nowhere), and leaves the hoard torrent alone.
#[tokio::test]
async fn purge_race_takes_race_copies_only_and_only_when_allowed() {
    let closed = st("mcp-purge-closed", "");
    let kept = add(&closed, "race", "kept.bin");
    let r = tool(&closed, "purge_race", json!({"info_hashes": [kept]})).await;
    assert!(failure(&r).contains("allow_destructive"));
    assert!(api::find_torrent(&closed.state, &kept).is_some());

    let s = st("mcp-purge", "\n[mcp]\nallow_destructive = true\n");
    let race = add(&s, "race", "gone.bin");
    let hoard = add(&s, "hoard", "stays.bin");
    let unknown = "7".repeat(40);

    let r = tool(
        &s,
        "purge_race",
        json!({"info_hashes": [race, hoard, unknown]}),
    )
    .await;

    assert_eq!(r["isError"], false, "{r}");
    let out = &r["structuredContent"];
    assert_eq!(
        (out["ok"].clone(), out["failed"].clone()),
        (1.into(), 2.into())
    );
    let results = out["results"].as_array().unwrap();
    assert_eq!(results[0]["info_hash"], race);
    assert_eq!(
        (results[0]["ok"].clone(), results[0]["status"].clone()),
        (true.into(), 200.into())
    );
    assert_eq!(results[1]["status"], 404);
    assert_eq!(results[1]["answer"]["error"], "torrent not in race");
    assert_eq!(results[2]["status"], 404);
    assert!(
        api::find_torrent(&s.state, &race).is_none(),
        "the race copy is gone"
    );
    assert!(s.dir.join("payload/stays.bin").exists());
    assert!(
        !s.dir.join("payload/gone.bin").exists(),
        "with its files: a purge frees the slot"
    );
    assert!(
        api::find_torrent(&s.state, &hoard).is_some(),
        "the hoard one stays"
    );
}

/// ⭐ An add that is asked wrongly says how, and adds nothing: no category,
/// one the node does not know (with the ones it does), more than one source,
/// no source, a file it may not read, an engine it does not run.
#[tokio::test]
async fn an_add_asked_wrongly_says_how_and_adds_nothing() {
    let s = st("mcp-add-bad", "");
    put_category(
        &s.state,
        "books",
        "hoard",
        s.dir.join("books").to_str().unwrap(),
    );
    let nothing_added = || {
        s.engines.get("race").unwrap().manager.all().len()
            + s.engines.get("hoard").unwrap().manager.all().len()
            + s.store.lock().unwrap().magnets().unwrap().len()
    };
    let magnet = format!("magnet:?xt=urn:btih:{}", "3".repeat(40));

    for (args, error) in [
        (
            json!({"torrent_url": "http://x.example/a.torrent"}),
            "category is required".to_string(),
        ),
        (
            json!({"category": "films", "magnet": magnet}),
            "unknown category `films`; this node has: books".to_string(),
        ),
        (
            json!({"category": "books", "magnet": magnet, "torrent_path": "/a.torrent"}),
            "give one of torrent_path, torrent_url or magnet".to_string(),
        ),
        (
            json!({"category": "books", "magnet": magnet, "torrent_url": "http://x.example/a.torrent"}),
            "give one of torrent_path, torrent_url or magnet".to_string(),
        ),
        (
            json!({"category": "books", "torrent_path": "/a.torrent", "torrent_url": "http://x.example/a.torrent"}),
            "give torrent_path or torrent_url, not both".to_string(),
        ),
        (
            json!({"category": "books"}),
            "torrent_path, torrent_url or magnet is required".to_string(),
        ),
        (
            json!({"category": "books", "torrent_path": "/etc/hostname"}),
            "/etc/hostname: not a .torrent file this node can read".to_string(),
        ),
        (
            json!({"category": "books", "torrent_url": "ftp://x.example/a.torrent"}),
            "torrent_url must be an http(s) URL".to_string(),
        ),
    ] {
        let r = tool(&s, "add_torrent", args.clone()).await;
        assert_eq!(failure(&r), error, "{args}");
    }
    assert_eq!(nothing_added(), 0);
}

/// A magnet is queued for its metadata, labelled as asked, and the answer
/// says it is resolving, not that the torrent is there.
#[tokio::test]
async fn a_magnet_is_queued_with_its_category_tags_and_pause() {
    let s = st("mcp-add-magnet", "");
    put_category(
        &s.state,
        "books",
        "hoard",
        s.dir.join("books").to_str().unwrap(),
    );
    let hash = "4".repeat(40);

    let r = tool(
        &s,
        "add_torrent",
        json!({"category": "books", "magnet": format!("magnet:?xt=urn:btih:{hash}"),
               "tags": [" fresh ", "keep"], "paused": true}),
    )
    .await;

    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(
        r["structuredContent"],
        json!({"info_hash": hash, "status": "resolving"})
    );
    let waiting = s.store.lock().unwrap().magnets().unwrap();
    assert_eq!(waiting.len(), 1);
    let m = &waiting[0];
    assert_eq!(m.info_hash, hash);
    assert_eq!(m.category, "books");
    assert_eq!(m.tags, "fresh,keep");
    assert!(m.paused);
    assert_eq!(m.state, "resolving");
}
