//! `move_data` and `graduate` where they stop: the refusals and the failures
//! halfway, which must leave the torrent served from where it was and every
//! original in place. Across filesystems the move copies, and `/dev/shm` is
//! the other filesystem the build container has.

use super::*;
use crate::api::testing::{state_from, TestState};
use std::path::{Path, PathBuf};
use typhon_engine::torrent::TorrentManager;

/// A torrent `pair/<name>` per file, each of the given length.
fn torrent(files: &[(&str, usize)]) -> Vec<u8> {
    let list: String = files
        .iter()
        .map(|(p, l)| format!("d6:lengthi{l}e4:pathl{}:{p}ee", p.len()))
        .collect();
    let pieces = files.iter().map(|(_, l)| l).sum::<usize>().div_ceil(16384);
    let info = format!(
        "d5:filesl{list}e4:name4:pair12:piece lengthi16384e6:pieces{}:{}e",
        20 * pieces,
        "M".repeat(20 * pieces)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// The other filesystem, removed when the test ends.
struct Shm(PathBuf);
impl Drop for Shm {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Puts the copy hooks back to a normal disk, whatever the test did.
struct Disk;
impl Drop for Disk {
    fn drop(&mut self) {
        crate::jobs::COPY_FULL_AFTER.with(|c| c.set(None));
        crate::jobs::FREE_SPACE.with(|c| c.set(None));
    }
}

struct Moving {
    s: TestState,
    ih: typhon_engine::torrent::meta::InfoHash,
    hash: String,
    /// Where the torrent is served from: the files are in `src/pair`.
    src: PathBuf,
    dst: PathBuf,
    job: crate::store::Job,
    _shm: Option<Shm>,
}

impl Moving {
    fn engine(&self) -> Arc<TorrentManager> {
        self.s.state.engines.get("race").unwrap().manager.clone()
    }

    /// The torrent is still served from the folder it started in.
    fn still_served_from_the_source(&self) {
        let t = self.engine().get(&self.ih).expect("still in its engine");
        assert_eq!(*t.save_path.read(), self.src);
    }
}

/// File n holds the byte n + 1. `across`: the files start on /dev/shm, so the
/// move to the temp dir is a copy. When /dev/shm is not another filesystem
/// here the test fails (`jobs::no_second_filesystem`); `None` only when
/// `HYDRANOS_TEST_ALLOW_NO_SECOND_FS` lets it skip.
fn moving(
    tag: &str,
    files: &[(&str, usize)],
    across: bool,
    allow: bool,
    kind: &str,
) -> Option<Moving> {
    let s = state_from(tag, "");
    let dst = s.dir.join("elsewhere");
    let (root, shm) = if across {
        let shm_dir = Path::new("/dev/shm");
        if !shm_dir.exists() || crate::jobs::same_filesystem(shm_dir, &s.dir) {
            crate::jobs::no_second_filesystem();
            return None;
        }
        let r = shm_dir.join(format!("hydra-mvf-{}-{tag}", std::process::id()));
        (r.clone(), Some(Shm(r)))
    } else {
        (s.dir.join("race-data"), None)
    };
    std::fs::create_dir_all(root.join("pair")).unwrap();
    for (n, (name, len)) in files.iter().enumerate() {
        std::fs::write(root.join("pair").join(name), vec![n as u8 + 1; *len]).unwrap();
    }
    let blob = torrent(files);
    let race = s.state.engines.get("race").unwrap();
    let (ih, _) = race
        .manager
        .add_torrent_bytes(&blob, &root.display().to_string(), true, false)
        .expect("added");
    let hash = typhon_engine::torrent::hex_encode(&ih);
    let params = if kind == "move_data" {
        serde_json::json!({
            "engine": "race", "category": "", "save_path": dst.display().to_string(),
            "allow_breaking_hardlinks": allow,
        })
    } else {
        serde_json::json!({
            "from_engine": "race", "to_engine": "hoard", "to_category": "",
            "save_path": dst.display().to_string(),
        })
    }
    .to_string();
    let job = {
        let store = s.state.store.lock().unwrap();
        store
            .insert_torrent(
                &hash,
                "race",
                &blob,
                &root.display().to_string(),
                "",
                0.0,
                true,
                "",
            )
            .unwrap();
        store.create_job(kind, &hash, &params, 1).unwrap();
        store.claim_next_job().expect("claimed")
    };
    Some(Moving {
        s,
        ih,
        hash,
        src: root,
        dst,
        job,
        _shm: shm,
    })
}

/// A job's `params` cannot be rewritten: the record of where the torrent was
/// cannot be written down.
fn refuse_job_params(m: &Moving) {
    m.s.state.store.lock().unwrap().execute_batch_for_tests(
        "CREATE TRIGGER refuse_params BEFORE UPDATE OF params ON jobs \
         BEGIN SELECT RAISE(ABORT, 'database disk image is malformed'); END;",
    );
}

fn bytes_of(p: PathBuf, n: u8, len: usize) -> bool {
    std::fs::read(p).map(|b| b == vec![n; len]).unwrap_or(false)
}

/// ⭐ The answer given when a move was asked is stale by the time the job
/// runs: a file that gained a second name in between would have its link
/// broken by the copy. The job asks again and stops before the first byte.
#[test]
fn a_name_gained_after_the_request_stops_the_job_before_a_byte_is_copied() {
    let Some(m) = moving("mvf-late-link", &[("a", 16384)], true, false, "move_data") else {
        return;
    };
    std::fs::hard_link(m.src.join("pair/a"), m.src.join("pair/library-copy")).unwrap();

    let err = move_data(&m.s.state, &m.job).expect_err("the link would be broken");
    assert!(
        err.starts_with("1 file(s) are hardlinked elsewhere"),
        "{err}"
    );
    assert!(
        err.contains("allow_breaking_hardlinks was not given"),
        "{err}"
    );
    assert!(!m.dst.exists(), "nothing was copied");
    assert!(bytes_of(m.src.join("pair/a"), 1, 16384));
    m.still_served_from_the_source();
}

/// With the operator's consent the copy breaks the link: the torrent's name
/// moves, the other name keeps the bytes, and the job counts them.
#[test]
fn with_consent_the_copy_breaks_the_link_and_the_other_name_keeps_its_bytes() {
    let Some(m) = moving("mvf-consent", &[("a", 16384)], true, true, "move_data") else {
        return;
    };
    std::fs::hard_link(m.src.join("pair/a"), m.src.join("pair/library-copy")).unwrap();

    move_data(&m.s.state, &m.job).expect("moved with consent");
    assert!(bytes_of(m.dst.join("pair/a"), 1, 16384));
    assert!(
        !m.src.join("pair/a").exists(),
        "the torrent's own name left the source"
    );
    assert!(
        bytes_of(m.src.join("pair/library-copy"), 1, 16384),
        "the other name still reads the bytes"
    );
    let t = m.engine().get(&m.ih).expect("still in race");
    assert_eq!(*t.save_path.read(), m.dst);
    assert_eq!(
        m.s.state
            .store
            .lock()
            .unwrap()
            .save_path_of(&m.hash)
            .as_deref(),
        Some(m.dst.display().to_string().as_str())
    );
    let jobs = m.s.state.store.lock().unwrap().list_jobs(10).unwrap();
    assert_eq!(jobs[0].progress_bytes, 16384, "the bytes copied");
}

/// ⭐ The copy of the second file fails: the first copy is removed too, no
/// partial file is left, every original is where it was, and the torrent
/// never left its engine.
#[test]
fn a_copy_that_fails_on_the_second_file_removes_the_first_copy_and_moves_nothing() {
    let Some(m) = moving(
        "mvf-copy-fails",
        &[("a", 8000), ("b", 16384)],
        true,
        false,
        "move_data",
    ) else {
        return;
    };
    let _disk = Disk;
    // `a` (8000 bytes) fits, `b` (16384) does not.
    crate::jobs::COPY_FULL_AFTER.with(|c| c.set(Some(9000)));

    let err = move_data(&m.s.state, &m.job).expect_err("the disk filled up");
    assert!(err.starts_with("copying "), "{err}");
    assert!(err.contains("pair/b"), "the file that failed: {err}");
    assert!(
        err.ends_with("the copies were removed, nothing moved"),
        "{err}"
    );
    assert!(!m.dst.join("pair/a").exists(), "the first copy was removed");
    assert!(
        !m.dst.join("pair/b").exists(),
        "and no partial file is left"
    );
    assert!(bytes_of(m.src.join("pair/a"), 1, 8000));
    assert!(bytes_of(m.src.join("pair/b"), 2, 16384));
    m.still_served_from_the_source();
}

/// Room for the bytes to copy is asked before the first one is: one byte
/// short is refused with both numbers, exactly enough goes through.
#[test]
fn the_target_needs_room_for_exactly_the_bytes_to_copy() {
    let Some(m) = moving(
        "mvf-room",
        &[("a", 8000), ("b", 16384)],
        true,
        false,
        "move_data",
    ) else {
        return;
    };
    let _disk = Disk;

    crate::jobs::FREE_SPACE.with(|c| c.set(Some(24383)));
    let err = move_data(&m.s.state, &m.job).expect_err("one byte short");
    assert_eq!(
        err,
        "24384 bytes to copy, 24383 free on the target; nothing was moved"
    );
    assert!(!m.dst.exists(), "nothing was copied");
    m.still_served_from_the_source();

    crate::jobs::FREE_SPACE.with(|c| c.set(Some(24384)));
    move_data(&m.s.state, &m.job).expect("exactly enough room");
    assert!(bytes_of(m.dst.join("pair/a"), 1, 8000));
    assert!(bytes_of(m.dst.join("pair/b"), 2, 16384));
    assert!(!m.src.join("pair/a").exists() && !m.src.join("pair/b").exists());
}

/// ⭐ The record of where the torrent was is written down BEFORE it leaves
/// its engine. A store that refuses it stops the move there: the copies are
/// removed and the torrent is still served from where it was.
#[test]
fn a_store_that_refuses_the_record_stops_the_move_with_the_copies_removed() {
    let Some(m) = moving("mvf-no-record", &[("a", 16384)], true, false, "move_data") else {
        return;
    };
    refuse_job_params(&m);

    let err = move_data(&m.s.state, &m.job).expect_err("the record cannot be written");
    assert!(
        err.starts_with("cannot write down the torrent's record: "),
        "{err}"
    );
    assert!(
        err.ends_with("the copies were removed, nothing moved"),
        "{err}"
    );
    assert!(!m.dst.join("pair/a").exists(), "the copy was removed");
    assert!(bytes_of(m.src.join("pair/a"), 1, 16384));
    m.still_served_from_the_source();
}

/// The same refusal on a graduation stops it before the torrent leaves its
/// engine: nothing copied, nothing renamed.
#[test]
fn a_graduation_whose_record_cannot_be_written_moves_nothing() {
    let Some(m) = moving(
        "mvf-grad-no-record",
        &[("a", 16384)],
        false,
        false,
        "graduate",
    ) else {
        return;
    };
    refuse_job_params(&m);

    let err = graduate(&m.s.state, &m.job).expect_err("the record cannot be written");
    assert!(
        err.starts_with("cannot write down the torrent's record: "),
        "{err}"
    );
    assert!(err.ends_with("nothing was moved"), "{err}");
    assert!(!m.dst.exists());
    assert!(bytes_of(m.src.join("pair/a"), 1, 16384));
    m.still_served_from_the_source();
    assert!(
        m.s.state
            .engines
            .get("hoard")
            .unwrap()
            .manager
            .get(&m.ih)
            .is_none(),
        "the target never held it"
    );
}

/// A graduation run again after the torrent left its engine can only finish
/// from the record the first run carried; with none, it says so instead of
/// guessing where the files went.
#[test]
fn a_graduation_run_again_without_a_record_cannot_find_its_torrent() {
    let Some(m) = moving(
        "mvf-grad-no-torrent",
        &[("a", 16384)],
        false,
        false,
        "graduate",
    ) else {
        return;
    };
    m.engine().remove_torrent(&m.ih, true).unwrap();

    let err = graduate(&m.s.state, &m.job).expect_err("nothing to graduate");
    assert_eq!(err, "the source engine no longer holds it");
    assert!(
        bytes_of(m.src.join("pair/a"), 1, 16384),
        "the data is untouched"
    );
    assert!(!m.dst.exists());
}

/// ⭐ A metainfo in the store that is not this torrent's is refused when the
/// torrent is handed back at the new place. The renames are undone whatever
/// else fails, and the answer says the undo is incomplete rather than
/// claiming the torrent is where it was.
#[test]
fn a_metainfo_the_engine_refuses_puts_the_files_back_and_says_the_undo_is_incomplete() {
    let Some(m) = moving("mvf-wrong-meta", &[("a", 16384)], false, false, "move_data") else {
        return;
    };
    let other: String = torrent(&[("z", 16384)])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    m.s.state
        .store
        .lock()
        .unwrap()
        .execute_batch_for_tests(&format!(
            "UPDATE torrents SET torrent = x'{other}' WHERE info_hash = '{}';",
            m.hash
        ));

    let err = move_data(&m.s.state, &m.job).expect_err("the engine refused the metainfo");
    assert!(
        err.starts_with(&format!("the engine refused it at {}: ", m.dst.display())),
        "{err}"
    );
    assert!(
        err.contains("ROLLBACK INCOMPLETE: could not re-add it at "),
        "{err}"
    );
    assert!(
        bytes_of(m.src.join("pair/a"), 1, 16384),
        "the rename was undone: {err}"
    );
    assert!(!m.dst.join("pair/a").exists());
}

/// A move with no engine or no folder is refused before anything is looked
/// up, and says which of the two it lacks as one sentence.
#[test]
fn a_move_with_no_engine_or_no_folder_is_refused_before_anything_is_looked_at() {
    let Some(m) = moving("mvf-no-params", &[("a", 16384)], false, false, "move_data") else {
        return;
    };
    let ask = |engine: &str, save_path: &str| {
        let mut j = m.job.clone();
        j.params = serde_json::json!({"engine": engine, "save_path": save_path}).to_string();
        move_data(&m.s.state, &j)
    };
    for (engine, folder) in [("race", ""), ("", "/somewhere"), ("", "")] {
        assert_eq!(
            ask(engine, folder),
            Err("a move needs an engine and a save path".to_string()),
            "engine {engine:?}, folder {folder:?}"
        );
    }
    assert_eq!(
        ask("ghost", "/somewhere"),
        Err("the engine is gone".to_string()),
        "both given: it goes on to look the engine up"
    );
    assert!(bytes_of(m.src.join("pair/a"), 1, 16384));
    m.still_served_from_the_source();
}

/// A move on one filesystem renames, and the job counts every file it
/// renamed as moved: the progress ends at the size of the torrent.
#[test]
fn a_rename_move_reports_every_renamed_file_as_progress() {
    let Some(m) = moving(
        "mvf-progress",
        &[("a", 8000), ("b", 16384)],
        false,
        false,
        "move_data",
    ) else {
        return;
    };

    move_data(&m.s.state, &m.job).expect("moved");

    assert!(bytes_of(m.dst.join("pair/a"), 1, 8000));
    assert!(bytes_of(m.dst.join("pair/b"), 2, 16384));
    let jobs = m.s.state.store.lock().unwrap().list_jobs(10).unwrap();
    assert_eq!(jobs[0].progress_bytes, 8000 + 16384);
}

/// ⭐ A name of the same file already at the destination is not "moved onto":
/// renaming one name onto another name of the same file does nothing, and the
/// move does not go on to delete the name it did not move. The old name is a
/// link somebody else may be reading.
#[test]
fn a_move_onto_another_name_of_the_same_file_deletes_neither_name() {
    let Some(m) = moving("mvf-same-file", &[("a", 16384)], false, false, "move_data") else {
        return;
    };
    std::fs::create_dir_all(m.dst.join("pair")).unwrap();
    std::fs::hard_link(m.src.join("pair/a"), m.dst.join("pair/a")).unwrap();

    move_data(&m.s.state, &m.job).expect("moved");

    assert!(bytes_of(m.dst.join("pair/a"), 1, 16384), "the new place");
    assert!(
        bytes_of(m.src.join("pair/a"), 1, 16384),
        "the other name of the file is still there"
    );
    let t = m.engine().get(&m.ih).expect("still in race");
    assert_eq!(*t.save_path.read(), m.dst);
}
