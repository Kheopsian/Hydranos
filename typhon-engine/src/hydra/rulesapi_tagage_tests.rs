//! `tag_age`: "tag X was set more than 7 days ago".

use super::*;
use crate::api::testing::{state_from, TestState};
use crate::rules::{Action, Cond, Facts, Node, Op, Trigger};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const DAY: i64 = 86400;

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn age_cond(tag: &str, op: Op, value: &str) -> Node {
    Node::Cond(Cond {
        field: "tag_age".into(),
        op,
        value: value.into(),
        hold: None,
        tag: Some(tag.into()),
    })
}

fn workflow(id: &str, when: Node, mark: &str) -> Workflow {
    Workflow {
        id: id.into(),
        name: id.into(),
        enabled: true,
        position: 0,
        trigger: Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then: vec![Action::AddTags {
            tags: vec![mark.into()],
        }],
        cap: rules::DEFAULT_CAP,
    }
}

fn aged(tag: &str, secs: f64) -> Facts {
    Facts {
        tags: vec![tag.into()],
        tag_ages: vec![(tag.into(), secs)],
        ..Default::default()
    }
}

#[test]
fn the_age_is_compared_to_the_duration_for_that_tag() {
    let m = rules::compile(&age_cond("keep", Op::Gt, "7d")).unwrap();
    assert!(m(&aged("keep", 8.0 * DAY as f64)));
    assert!(!m(&aged("keep", 6.0 * DAY as f64)));
    // Another tag's age is not this tag's.
    assert!(!m(&aged("other", 30.0 * DAY as f64)));
}

/// Not wearing the tag is no age at all: false for every operator, `ne` and
/// "newer than" included, never an infinite age.
#[test]
fn a_torrent_without_the_tag_never_matches() {
    let none = Facts::default();
    for op in [Op::Gt, Op::Ge, Op::Lt, Op::Le, Op::Eq, Op::Ne] {
        assert!(
            !rules::compile(&age_cond("keep", op, "7d")).unwrap()(&none),
            "{op:?}"
        );
    }
}

#[test]
fn the_tag_is_required_part_of_the_key_and_refused_on_an_event() {
    let no_tag = Node::Cond(Cond {
        field: "tag_age".into(),
        op: Op::Gt,
        value: "7d".into(),
        hold: None,
        tag: None,
    });
    assert!(rules::compile(&no_tag).is_err(), "which tag?");
    let key = |t: &str| {
        let Node::Cond(c) = age_cond(t, Op::Gt, "7d") else {
            unreachable!()
        };
        rules::condition_key(&c)
    };
    assert_ne!(key("a"), key("b"), "two tags, two conditions");
    let mut w = workflow("w", age_cond("keep", Op::Gt, "7d"), "x");
    w.trigger = Trigger::Added;
    assert_eq!(
        rules::compile_workflow(&w).err(),
        Some(rules::CompileError::TagAgeOnEvent)
    );
    let json = r#"{"kind":"cond","field":"tag_age","op":"gt","value":"7d","tag":"keep"}"#;
    let Node::Cond(c) = serde_json::from_str::<Node>(json).unwrap() else {
        panic!()
    };
    assert_eq!(c.tag.as_deref(), Some("keep"));
}

// ---------------------------------------------------------------- end to end

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xEFu8; 20];
    piece[0] = name.as_bytes()[0];
    piece[1] = name.len() as u8;
    info.extend_from_slice(&piece);
    info.push(b'e');
    let mut out = Vec::new();
    out.extend_from_slice(b"d4:info");
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn add(s: &TestState, name: &str) -> String {
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
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

/// Wear `tag`, dated `ago` seconds back (None: a row-less tag).
fn wear(s: &TestState, hash: &str, tag: &str, ago: Option<i64>) {
    let store = s.state.store.lock().unwrap();
    store.set_tags(hash, &[tag.to_string()]).unwrap();
    store.put_tag_since_for_tests(hash, tag, ago.map(|a| crate::store::now_secs() - a));
}

fn tags(s: &TestState, hash: &str) -> Vec<String> {
    s.state.store.lock().unwrap().tags_of(hash)
}

fn marked(s: &TestState, hash: &str, mark: &str) -> bool {
    tags(s, hash).iter().any(|t| t == mark)
}

/// 8 days old satisfies "older than 7d", 6 days does not; a torrent without the
/// tag does not either.
#[tokio::test]
async fn a_rule_reads_the_dates_the_store_keeps() {
    let s = st("tagage-dates");
    let (a, b, c) = (
        add(&s, "aaa.bin"),
        add(&s, "bbbb.bin"),
        add(&s, "ccccc.bin"),
    );
    wear(&s, &a, "keep", Some(8 * DAY));
    wear(&s, &b, "keep", Some(6 * DAY));
    wear(&s, &c, "other", Some(30 * DAY));
    let w = workflow("wf-age", age_cond("keep", Op::Gt, "7d"), "stale");
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 1, "{r}");
    assert!(marked(&s, &a, "stale"));
    assert!(!marked(&s, &b, "stale"), "6 days is not more than 7");
    assert!(!marked(&s, &c, "stale"), "it does not wear the tag");
}

/// A tag worn before dates were kept is dated at the migration.
#[tokio::test]
async fn an_old_tag_without_a_row_is_dated_at_the_migration() {
    let s = st("tagage-migration");
    let a = add(&s, "aaa.bin");
    wear(&s, &a, "keep", None);
    let at = |ago: i64| {
        s.state
            .store
            .lock()
            .unwrap()
            .put_meta(
                crate::store::TAG_SINCE_MIGRATED_KEY,
                &(crate::store::now_secs() - ago).to_string(),
            )
            .unwrap()
    };
    let w = workflow("wf-mig", age_cond("keep", Op::Gt, "7d"), "stale");
    at(3 * DAY);
    run_one(&s.state, &w, false);
    assert!(!marked(&s, &a, "stale"), "migrated 3 days ago: 3 days old");
    at(10 * DAY);
    run_one(&s.state, &w, false);
    assert!(marked(&s, &a, "stale"), "migrated 10 days ago: 10 days old");
}

/// A pass that has no `tag_age` condition never reads the dates; one that has
/// reads them only for the torrents wearing the tag.
#[tokio::test]
async fn the_dates_are_read_only_when_a_rule_asks() {
    let s = st("tagage-cost");
    let a = add(&s, "aaa.bin");
    let b = add(&s, "bbbb.bin");
    wear(&s, &a, "keep", Some(8 * DAY));
    wear(&s, &b, "elsewhere", Some(8 * DAY));
    let calls = || crate::store::TAG_SINCE_BULK_CALLS.with(|c| c.get());

    let plain = workflow(
        "wf-plain",
        Node::Cond(Cond {
            field: "ratio".into(),
            op: Op::Ge,
            value: "0".into(),
            hold: None,
            tag: None,
        }),
        "x",
    );
    let before = calls();
    run_one(&s.state, &plain, false);
    assert_eq!(calls(), before, "no tag_age, no read of tag_since");

    let asks = workflow("wf-asks", age_cond("keep", Op::Gt, "7d"), "stale");
    run_one(&s.state, &asks, false);
    assert_eq!(
        calls(),
        before + 1,
        "one read, for the torrents that wear it"
    );
}
