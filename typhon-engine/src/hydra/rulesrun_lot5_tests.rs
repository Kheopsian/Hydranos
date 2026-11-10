//! What a pass reads about a torrent before it judges it: which facts it pays
//! for only when a workflow asks, what the free space of a folder is, and how
//! the link scanner's answer reaches a single torrent's facts.

use super::*;
use crate::api::testing::{state_from, TestState};

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!(
        "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
        name.len(),
        "L".repeat(20)
    );
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// A paused torrent of the hoard engine, filed under a real folder; its hash.
fn add(s: &TestState, name: &str) -> String {
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(name), vec![2u8; 16384]).unwrap();
    crate::api::add_torrent_bytes(
        &s.state,
        &torrent_bytes(name),
        "",
        dir.to_str().unwrap(),
        "",
        true,
        true,
        "hoard",
    )
    .unwrap_or_else(|e| panic!("add {name}: {e}"))
    .0
}

fn cond(field: &str) -> rules::Node {
    rules::Node::Cond(rules::Cond {
        field: field.into(),
        op: rules::Op::Gt,
        value: "0".into(),
        hold: None,
        tag: None,
    })
}

fn workflow(when: rules::Node, then: Vec<Action>) -> Workflow {
    Workflow {
        id: "w".into(),
        name: "w".into(),
        enabled: true,
        position: 0,
        trigger: rules::Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then,
        cap: rules::DEFAULT_CAP,
    }
}

/// A pass pays for the free space of every folder only when a condition reads
/// it, and for every torrent's tracker list only when an action needs it.
#[test]
fn a_workflow_asks_only_for_the_facts_it_reads() {
    let plain = Want::of(&workflow(cond("ratio"), vec![Action::Pause]));
    assert_eq!((plain.free_space, plain.trackers), (false, false));

    let space = Want::of(&workflow(
        rules::Node::All {
            of: vec![
                cond("ratio"),
                rules::Node::Not {
                    of: Box::new(cond("free_space")),
                },
            ],
        },
        vec![Action::Pause],
    ));
    assert_eq!((space.free_space, space.trackers), (true, false));

    let trackers = Want::of(&workflow(
        cond("ratio"),
        vec![
            Action::Pause,
            Action::AddTrackers {
                urls: vec!["udp://t.example:80".into()],
                list_url: String::new(),
            },
        ],
    ));
    assert_eq!((trackers.free_space, trackers.trackers), (false, true));
}

/// What the filesystem has left under a folder is a real figure, and a folder
/// that is nowhere is "unknown", which no condition matches.
#[test]
fn the_free_space_of_a_folder_is_real_and_of_no_folder_is_unknown() {
    let s = state_from("lot5-free", "");
    let free = free_space_at(s.dir.to_str().unwrap());
    assert!(free > 1_000_000.0 && free < 1e18, "{free}");
    assert!(free_space_at("/no/such/folder/anywhere").is_nan());
}

/// ⭐ A pass reads a folder's free space only when it is asked to and the
/// torrent has a folder: with the want off, or no folder stored, the fact is
/// unknown; with both, it is the folder's.
#[test]
fn a_pass_reads_the_free_space_only_when_asked_and_only_of_a_folder() {
    let s = state_from("lot5-gather", "");
    let hash = add(&s, "gather.bin");
    let stored = s.state.store.workflow_facts("hoard").unwrap();
    assert!(!stored[&hash].save_path.is_empty());
    let links = Default::default();
    let want = |free_space| Want {
        free_space,
        trackers: false,
    };
    let read = |stored: &std::collections::HashMap<String, crate::store::WorkflowFacts>,
                w: Want|
     -> f64 { gather(&s.engines, stored, "hoard", &links, w)[0].free_space };

    assert!(read(&stored, want(false)).is_nan(), "not asked for");
    let asked = read(&stored, want(true));
    assert!(asked > 1_000_000.0 && asked < 1e18, "asked for: {asked}");
    let mut homeless = stored.clone();
    homeless.get_mut(&hash).unwrap().save_path.clear();
    assert!(
        read(&homeless, want(true)).is_nan(),
        "no folder stored: unknown, not the root's"
    );
    assert!(
        read(&Default::default(), want(true)).is_nan(),
        "no row at all"
    );
    assert!(gather(&s.engines, &stored, "no-engine", &links, want(true)).is_empty());
}

/// The same for one torrent, as an event reads it.
#[test]
fn one_torrents_facts_read_the_free_space_only_when_asked_and_only_of_a_folder() {
    let s = state_from("lot5-gather-one", "");
    let hash = add(&s, "one.bin");
    let one = |want: Want| -> Option<Facts> {
        let store = s.state.store.read().unwrap();
        gather_one_with(&s.engines, &store, "hoard", &hash, want)
    };
    let no = Want {
        free_space: false,
        trackers: false,
    };
    let yes = Want {
        free_space: true,
        trackers: false,
    };

    assert!(one(no).unwrap().free_space.is_nan(), "not asked for");
    let asked = one(yes).unwrap().free_space;
    assert!(asked > 1_000_000.0 && asked < 1e18, "{asked}");
    {
        let store = s.state.store.lock().unwrap();
        store.execute_batch_for_tests(&format!(
            "UPDATE torrents SET save_path = '' WHERE info_hash = '{hash}';"
        ));
    }
    assert!(
        one(yes).unwrap().free_space.is_nan(),
        "no folder stored: unknown"
    );
    assert!(
        gather_one_with(
            &s.engines,
            &s.state.store.read().unwrap(),
            "hoard",
            &"9".repeat(40),
            yes
        )
        .is_none(),
        "a torrent nobody holds"
    );
}

/// ⭐ What the scanner measured reaches one torrent's facts only when its row
/// describes the torrent as it is now: the same folder and the same number of
/// files. A row for other files, or none, leaves every link fact unknown.
#[test]
fn the_scanners_answer_reaches_one_torrent_only_when_its_row_is_current() {
    let s = state_from("lot5-links", "");
    let hash = add(&s, "links.bin");
    let raw = crate::store::hex20(&hash).unwrap();
    let folder = s.dir.join("data").to_string_lossy().into_owned();
    let facts = || -> Facts {
        let store = s.state.store.read().unwrap();
        gather_one_with(
            &s.engines,
            &store,
            "hoard",
            &hash,
            Want {
                free_space: false,
                trackers: false,
            },
        )
        .unwrap()
    };
    let enrich = |f: &mut Facts| {
        let store = s.state.store.read().unwrap();
        link_facts_one(&s.engines, &store, "hoard", f);
    };
    let row = |save_path: &str, files: i64, missing: i64| crate::store::LinkRow {
        info_hash: hash.clone(),
        session: "hoard".into(),
        save_path: save_path.into(),
        measured_at: 1,
        files,
        missing,
        stats: Vec::new(),
    };
    let put = |r: crate::store::LinkRow| s.state.store.lock().unwrap().put_link_rows(&[r]).unwrap();
    let mut summary = crate::linkindex::Summary::default();
    summary.insert(
        raw,
        crate::linkindex::Cached {
            external_links: 2,
            link_count: 3,
            measured_at: 1,
        },
    );
    s.engines.publish_link_summary(summary);
    let unknown = |f: &Facts| f.link_count.is_nan() && f.external_links.is_nan() && !f.data_missing;

    // Never measured: nothing is read from the summary.
    let mut f = facts();
    enrich(&mut f);
    assert!(unknown(&f), "no row");

    // Measured under another folder, or with another number of files: stale.
    for stale in [row("/elsewhere", 1, 0), row(&folder, 5, 0)] {
        put(stale);
        let mut f = facts();
        enrich(&mut f);
        assert!(unknown(&f), "a row that describes other files");
    }

    // Current, every file readable: the summary's numbers, and nothing missing.
    put(row(&folder, 1, 0));
    let mut f = facts();
    enrich(&mut f);
    assert_eq!((f.link_count, f.external_links), (3.0, 2.0));
    assert!(!f.data_missing, "0 of 1 missing");

    // Current, the one file unreadable: its data is missing.
    put(row(&folder, 1, 1));
    let mut f = facts();
    enrich(&mut f);
    assert_eq!((f.link_count, f.external_links), (3.0, 2.0));
    assert!(f.data_missing, "1 of 1 missing");

    // A hash that is not a hash, or a torrent the engine does not hold.
    let mut bogus = facts();
    bogus.info_hash = "not a hash".into();
    enrich(&mut bogus);
    assert!(unknown(&bogus));
    let mut gone = facts();
    gone.info_hash = "8".repeat(40);
    enrich(&mut gone);
    assert!(unknown(&gone));
}

/// How many threads stat at once: 16 unless told otherwise, what a person
/// says when it is a whole number above 0, never more than 256.
#[test]
fn the_number_of_scan_threads_is_16_unless_told_otherwise_and_never_above_256() {
    const VAR: &str = "HYDRANOS_LINK_SCAN_THREADS";
    let with = |v: Option<&str>| -> usize {
        match v {
            Some(v) => std::env::set_var(VAR, v),
            None => std::env::remove_var(VAR),
        }
        scan_threads()
    };
    let (none, four, zero, junk, huge, edge) = (
        with(None),
        with(Some("4")),
        with(Some("0")),
        with(Some("many")),
        with(Some("100000")),
        with(Some("256")),
    );
    std::env::remove_var(VAR);
    assert_eq!(none, 16);
    assert_eq!(four, 4);
    assert_eq!(zero, 16, "0 is not a number of threads");
    assert_eq!(junk, 16);
    assert_eq!(huge, 256);
    assert_eq!(edge, 256);
}
