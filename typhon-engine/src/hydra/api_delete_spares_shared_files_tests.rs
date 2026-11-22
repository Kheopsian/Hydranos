use super::testing::*;
use super::*;

/// One file, `book.epub`, 16 KiB. `private` changes the info dict, so the
/// two torrents have two info hashes and read the same path -- what an
/// upload seeded from its source's files is.
fn torrent_bytes(private: bool) -> Vec<u8> {
    let name = "book.epub";
    format!(
        "d8:announce19:https://t.example/a4:infod6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}{}ee",
        name.len(),
        "A".repeat(20),
        if private { "7:privatei1e" } else { "" }
    )
    .into_bytes()
}

/// 2026-10-02: a bulk delete "with files" took the source torrents' files,
/// which the cross-seed uploads made from them also read, and 1 376 uploads went
/// on announcing empty folders as complete.
#[test]
fn deleting_a_torrent_with_its_files_keeps_the_ones_another_torrent_reads() {
    let s = state("delete-spares");
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("book.epub");
    std::fs::write(&file, vec![7u8; 16384]).unwrap();
    let root = dir.to_string_lossy().to_string();
    let (source, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes(false),
        "",
        &root,
        "",
        true,
        true,
        "hoard",
    )
    .expect("source added");
    let (upload, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes(true),
        "",
        &root,
        "",
        true,
        true,
        "hoard",
    )
    .expect("upload added");
    assert_ne!(source, upload);
    let sessions = vec!["hoard".to_string()];

    remove_one_torrent(&s.state, &source, &sessions, "", true).expect("source removed");
    assert!(
        file.exists(),
        "the upload still reads book.epub: it must stay"
    );

    remove_one_torrent(&s.state, &upload, &sessions, "", true).expect("upload removed");
    assert!(
        !file.exists(),
        "nothing reads it any more: deleting with files deletes it"
    );
}

/// `torrent_bytes(false)` seeded by `engine` from `dir`, next to whatever
/// copy another engine already holds -- what POST /copy does.
fn seed_copy(s: &TestState, engine: &str, dir: &std::path::Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("book.epub"), vec![7u8; 16384]).unwrap();
    let blob = torrent_bytes(false);
    let root = dir.to_string_lossy().to_string();
    let (ih, _) = s
        .state
        .engines
        .get(engine)
        .unwrap()
        .manager
        .add_torrent_bytes(&blob, &root, false, true)
        .expect("seeded");
    let hash = typhon_engine::torrent::hex_encode(&ih);
    s.state
        .store
        .lock()
        .unwrap()
        .insert_torrent(&hash, engine, &blob, &root, "", 0.0, false, "")
        .unwrap();
    hash
}

/// 2026-10-06: the *arr grabbed a race torrent into the hoard too, from a
/// payload of its own. The drain removed the race copy "with files", but
/// the store still listed the hoard copy, so the race files were kept for
/// a copy that never read them: 58 orphans on /race in a day.
#[test]
fn a_copy_with_its_own_payload_does_not_keep_this_one() {
    let s = state("delete-copy-elsewhere");
    let race = s.dir.join("race");
    let hoard = s.dir.join("hoard");
    let hash = seed_copy(&s, "race", &race);
    assert_eq!(seed_copy(&s, "hoard", &hoard), hash);

    remove_one_torrent(&s.state, &hash, &["race".to_string()], "race", true)
        .expect("race copy removed");
    assert!(
        !race.join("book.epub").exists(),
        "nothing reads the race payload any more"
    );
    assert!(
        hoard.join("book.epub").exists(),
        "the hoard copy's own payload stays"
    );
}

/// Two engines seeding ONE payload (POST /copy): the first removal keeps
/// the files the other copy reads, the last one takes them.
#[test]
fn a_copy_reading_the_same_files_keeps_them_until_it_goes() {
    let s = state("delete-copy-shared");
    let dir = s.dir.join("data");
    let hash = seed_copy(&s, "race", &dir);
    seed_copy(&s, "hoard", &dir);

    remove_one_torrent(&s.state, &hash, &["race".to_string()], "race", true)
        .expect("race copy removed");
    assert!(
        dir.join("book.epub").exists(),
        "the hoard copy still reads it"
    );
    remove_one_torrent(&s.state, &hash, &["hoard".to_string()], "hoard", true)
        .expect("hoard copy removed");
    assert!(!dir.join("book.epub").exists(), "the last reader is gone");
}

/// Removing every copy at once takes every copy's own payload, not only
/// the one the last engine happened to read.
#[test]
fn removing_everywhere_takes_each_copys_files() {
    let s = state("delete-everywhere");
    let race = s.dir.join("race");
    let hoard = s.dir.join("hoard");
    let hash = seed_copy(&s, "race", &race);
    seed_copy(&s, "hoard", &hoard);

    let _ = remove_torrent_everywhere(&s.state, &hash, true);
    assert!(!race.join("book.epub").exists());
    assert!(!hoard.join("book.epub").exists());
}

/// A move is refused when this torrent's copy in another engine reads the
/// same files, and allowed when that copy has a payload of its own.
#[test]
fn a_move_is_refused_only_when_another_copy_reads_the_same_path() {
    let s = state("move-copy");
    let shared = s.dir.join("shared");
    let hash = seed_copy(&s, "race", &shared);
    seed_copy(&s, "hoard", &shared);
    let race = find_copy(&s.state, "race", &hash).unwrap();
    let plan = crate::jobsrun::plan_move_checked(&s.state, &race, &s.dir.join("dst"));
    assert_eq!(
        plan.refusal().map(|r| r.0),
        Some("shared"),
        "the hoard copy reads these files"
    );
    assert_eq!(plan.shared_with, vec![hash.clone()]);

    let t = state("move-copy-elsewhere");
    let hash = seed_copy(&t, "race", &t.dir.join("race"));
    seed_copy(&t, "hoard", &t.dir.join("hoard"));
    let race = find_copy(&t.state, "race", &hash).unwrap();
    let plan = crate::jobsrun::plan_move_checked(&t.state, &race, &t.dir.join("dst"));
    assert!(
        plan.refusal().is_none(),
        "the hoard copy reads its own payload"
    );
}
