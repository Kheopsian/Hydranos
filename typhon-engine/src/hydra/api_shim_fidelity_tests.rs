use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        name.chars().next().unwrap().to_string().repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// One row per torrent even when two engines hold it, with the tracker
/// it announces to; `hashes=all` names every torrent; a deleted tag is
/// gone from the torrents too.
#[tokio::test]
async fn the_shim_answers_like_qbittorrent() {
    let s = state_from("shim-fid", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let a = torrent_bytes("alpha");
    let (ha, _) =
        add_torrent_bytes(&s.state, &a, "", "/tmp", "keep,drop", true, true, "race").unwrap();
    add_torrent_bytes(&s.state, &a, "", "/tmp", "keep,drop", true, true, "hoard").unwrap();
    assert!(s
        .state
        .store
        .lock()
        .unwrap()
        .tags_of(&ha)
        .contains(&"drop".to_string()));
    let (hb, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("bravo"),
        "",
        "/tmp",
        "",
        true,
        true,
        "hoard",
    )
    .unwrap();

    let r = qbit_torrents_info(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        String::new(),
    )
    .await;
    let rows = body_json(r).await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "alpha once, bravo once: {rows:?}");
    assert!(rows
        .iter()
        .all(|r| r["tracker"] == "https://tracker.example/announce"));

    let mut form = Fields::new();
    form.insert("hashes".into(), "all".into());
    let mut all = qbit_hashes(&s.state, &form);
    all.sort();
    let mut want = vec![ha.clone(), hb];
    want.sort();
    assert_eq!(all, want);

    let mut form = Fields::new();
    form.insert("tags".into(), "drop".into());
    qbit_delete_tags(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        Form(form),
    )
    .await;
    assert_eq!(
        s.state.store.lock().unwrap().tags_of(&ha),
        vec!["keep".to_string()]
    );
}
