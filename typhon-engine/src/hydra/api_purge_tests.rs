use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        "C".repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// The race purge drops the race copy from the engine and its row, and
/// nothing of a hoard copy of the same torrent. 4.3 deleted every row of
/// the hash and left both engines holding the torrent.
#[tokio::test]
async fn a_purge_takes_the_race_copy_and_only_it() {
    let s = state_from("purge-copy", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let dir = s.dir.join("payload");
    std::fs::create_dir_all(&dir).unwrap();
    let bytes = torrent_bytes("purged");
    let save = dir.display().to_string();
    let (hash, _) = add_torrent_bytes(&s.state, &bytes, "", &save, "", true, true, "race")
        .expect("race add");
    add_torrent_bytes(&s.state, &bytes, "", &save, "", true, true, "hoard").expect("hoard add");
    assert_eq!(
        s.state.store.lock().unwrap().sessions_of(&hash),
        vec!["hoard".to_string(), "race".to_string()]
    );

    let resp = purge_race_torrent(
        State(s.state.clone()),
        Path(hash.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        s.state.store.lock().unwrap().sessions_of(&hash),
        vec!["hoard".to_string()],
        "the hoard row stays"
    );
    assert!(
        find_copy(&s.state, "race", &hash).is_none(),
        "the race engine let go"
    );
    assert!(
        find_copy(&s.state, "hoard", &hash).is_some(),
        "the hoard engine still holds it"
    );
}
