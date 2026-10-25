//! "For at least": a condition that must have held for a duration. The record
//! (`rule_since`), the gate, the safe default, and the cost at scale.

use super::*;
use crate::api::testing::{body_json, keyed, state_from, TestState};
use crate::rules::{Action, Cond, Facts, Node, Op, Timing, Trigger};
use axum::extract::{Path, RawQuery, State};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const DAY: i64 = 86400;

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn cond(field: &str, op: Op, value: &str, hold: Option<&str>) -> Node {
    Node::Cond(Cond {
        field: field.into(),
        op,
        value: value.into(),
        hold: hold.map(String::from),
    })
}

fn paused_for(hold: Option<&str>) -> Node {
    cond("user_paused", Op::Eq, "true", hold)
}

fn workflow(id: &str, when: Node, then: Vec<Action>) -> Workflow {
    Workflow {
        id: id.into(),
        name: id.into(),
        enabled: true,
        position: 0,
        trigger: Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then,
        cap: rules::DEFAULT_CAP,
    }
}

fn tag(t: &str) -> Vec<Action> {
    vec![Action::AddTags {
        tags: vec![t.into()],
    }]
}

fn fact(hash: &str, state: &str) -> Facts {
    Facts {
        info_hash: hash.into(),
        state: state.into(),
        user_paused: state == "stopped",
        ..Default::default()
    }
}

// ---------------------------------------------------------------- the format

/// `"for"` is the key; a condition without it serialises exactly as before.
#[test]
fn the_for_key_round_trips_and_is_absent_when_unset() {
    let with: Node = serde_json::from_str(
        r#"{"kind":"cond","field":"user_paused","op":"eq","value":"true","for":"7d"}"#,
    )
    .unwrap();
    let Node::Cond(c) = &with else { panic!() };
    assert_eq!(c.hold.as_deref(), Some("7d"));
    assert_eq!(rules::hold_secs(c).unwrap(), Some(7 * DAY));
    assert!(serde_json::to_string(&with)
        .unwrap()
        .contains(r#""for":"7d""#));

    let without: Node =
        serde_json::from_str(r#"{"kind":"cond","field":"user_paused","op":"eq","value":"true"}"#)
            .unwrap();
    assert!(!serde_json::to_string(&without).unwrap().contains("for"));
}

#[test]
fn a_bad_for_is_refused_and_an_event_takes_none() {
    let bad = workflow("w", paused_for(Some("soon")), tag("x"));
    assert!(matches!(
        rules::compile_workflow(&bad).err(),
        Some(rules::CompileError::BadValue { .. })
    ));
    let mut event = workflow("w", paused_for(Some("7d")), tag("x"));
    event.trigger = Trigger::Completed;
    assert_eq!(
        rules::compile_workflow(&event).err(),
        Some(rules::CompileError::ForOnEvent)
    );
}

// ------------------------------------------------------------------ the gate

fn rows_of(w: &Workflow, hash: &str, since: i64) -> rules::SinceRows {
    let t = rules::timed_conditions(&w.when).unwrap();
    let mut r = rules::SinceRows::new();
    r.entry(t[0].key.clone())
        .or_default()
        .insert(hash.into(), since);
    r
}

fn judged(w: &Workflow, f: &Facts, now: i64, rows: rules::SinceRows) -> bool {
    let timing = Arc::new(Timing::new(now, rows));
    rules::compile_workflow_in(w, Some(&timing)).unwrap()(f)
}

/// True for less than N: nothing. True for N: it acts. A torrent with no
/// record has only just become true.
#[test]
fn the_action_waits_for_the_duration() {
    let w = workflow("w", paused_for(Some("7d")), tag("x"));
    let f = fact("h1", "stopped");
    let now = 10_000_000;
    assert!(!judged(&w, &f, now, rows_of(&w, "h1", now - 7 * DAY + 1)));
    assert!(judged(&w, &f, now, rows_of(&w, "h1", now - 7 * DAY)));
    assert!(!judged(&w, &f, now, Default::default()), "no row: from now");
    // Long held but false now: the condition itself still has to hold.
    assert!(!judged(
        &w,
        &fact("h1", "seeding"),
        now,
        rows_of(&w, "h1", now - 9 * DAY)
    ));
    // Compiled with no record at all, a timed condition never holds.
    assert!(!rules::compile_workflow(&w).unwrap()(&f));
}

/// A workflow saved before `for` existed is judged exactly as it was.
#[test]
fn a_workflow_without_for_behaves_exactly_as_before() {
    let w = workflow("w", paused_for(None), tag("x"));
    assert!(rules::timed_conditions(&w.when).unwrap().is_empty());
    assert!(rules::compile_workflow(&w).unwrap()(&fact("h", "stopped")));
    assert!(!rules::compile_workflow(&w).unwrap()(&fact("h", "seeding")));
    let old: Workflow = serde_json::from_str(
        r#"{"name":"old","when":{"kind":"all","of":[{"kind":"cond","field":"user_paused","op":"eq","value":"true"}]},"then":[{"type":"pause"}]}"#,
    )
    .unwrap();
    assert!(rules::compile_workflow(&old).unwrap()(&fact(
        "h", "stopped"
    )));
    assert!(rules::timed_conditions(&old.when).unwrap().is_empty());
}

/// The key follows the condition's content: reordering keeps the date,
/// editing the value starts again, changing only the duration keeps it.
#[test]
fn the_key_follows_the_content_not_the_position() {
    let a = || paused_for(Some("7d"));
    let b = || cond("ratio", Op::Ge, "2", None);
    let one = workflow("w", Node::All { of: vec![a(), b()] }, tag("x"));
    let two = workflow("w", Node::All { of: vec![b(), a()] }, tag("x"));
    let k1 = rules::timed_conditions(&one.when).unwrap();
    let k2 = rules::timed_conditions(&two.when).unwrap();
    assert_eq!(k1[0].key, k2[0].key);
    // The date written under the first order is honoured under the second.
    let f = fact("h", "stopped");
    let rows = rows_of(&one, "h", 1_000_000 - 8 * DAY);
    let f = Facts { ratio: 3.0, ..f };
    assert!(judged(&two, &f, 1_000_000, rows));

    let other = rules::timed_conditions(&paused_for(None)).unwrap();
    assert!(other.is_empty());
    let value_changed = cond("user_paused", Op::Eq, "false", Some("7d"));
    assert_ne!(
        rules::timed_conditions(&value_changed).unwrap()[0].key,
        k1[0].key
    );
    let longer = paused_for(Some("30d"));
    assert_eq!(rules::timed_conditions(&longer).unwrap()[0].key, k1[0].key);
}

/// Every `for` condition is judged on its own: one skipped by a false
/// sibling keeps losing and gaining its row as it should.
#[test]
fn a_condition_hidden_by_a_false_sibling_still_keeps_its_record() {
    let w = workflow(
        "w",
        Node::All {
            of: vec![cond("ratio", Op::Ge, "2", None), paused_for(Some("7d"))],
        },
        tag("x"),
    );
    let timed = rules::timed_conditions(&w.when).unwrap();
    let facts = vec![fact("h", "stopped")]; // ratio 0: the sibling is false
    let d = rules::since_delta(&timed, &facts, &Default::default()).unwrap();
    assert_eq!(d.put.len(), 1, "true on its own, whatever the AND says");
}

// ----------------------------------------------------------- the store, small

fn store_lock() -> Arc<crate::store::StoreLock> {
    let s = crate::store::Store::open_in_memory().unwrap();
    s.ensure_schema().unwrap();
    Arc::new(crate::store::StoreLock::new(s))
}

fn count(store: &Arc<crate::store::StoreLock>, wf: &str) -> i64 {
    store.lock().unwrap().rule_since_count(wf).unwrap()
}

fn hashes(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{i:040x}")).collect()
}

/// A pass writes the transitions and nothing else: the second, with nothing
/// changed, writes no row at all.
#[test]
fn a_pass_with_nothing_changed_writes_nothing() {
    let store = store_lock();
    let w = workflow("w", cond("state", Op::Eq, "seeding", Some("7d")), tag("x"));
    let hs = hashes(300);
    let mut facts: Vec<Facts> = hs.iter().map(|h| fact(h, "seeding")).collect();

    let first = rulesrun::track_since(&store, &w, &facts, 1000, true)
        .unwrap()
        .expect("timed");
    assert_eq!(first.written, 300);
    assert_eq!(count(&store, "w"), 300);

    let second = rulesrun::track_since(&store, &w, &facts, 2000, true)
        .unwrap()
        .expect("timed");
    assert_eq!(second.written, 0, "nothing changed, nothing written");

    // 1% flips: exactly those are written, in both directions.
    for f in facts.iter_mut().take(3) {
        f.state = "stopped".into();
    }
    let third = rulesrun::track_since(&store, &w, &facts, 3000, true)
        .unwrap()
        .unwrap();
    assert_eq!(third.written, 3);
    assert_eq!(count(&store, "w"), 297);
    for f in facts.iter_mut().take(3) {
        f.state = "seeding".into();
    }
    let fourth = rulesrun::track_since(&store, &w, &facts, 4000, true)
        .unwrap()
        .unwrap();
    assert_eq!(fourth.written, 3);
    assert_eq!(count(&store, "w"), 300);
}

/// A preview reads the record and writes nothing.
#[test]
fn a_dry_decision_writes_nothing() {
    let store = store_lock();
    let w = workflow("w", cond("state", Op::Eq, "seeding", Some("7d")), tag("x"));
    let facts = vec![fact("a", "seeding")];
    rulesrun::track_since(&store, &w, &facts, 1000, false).unwrap();
    assert_eq!(count(&store, "w"), 0);
}

/// A workflow with no `for` and no leftovers touches nothing.
#[test]
fn a_workflow_without_for_leaves_the_table_alone() {
    let store = store_lock();
    let w = workflow("w", cond("state", Op::Eq, "seeding", None), tag("x"));
    let facts = vec![fact("a", "seeding")];
    assert!(rulesrun::track_since(&store, &w, &facts, 1000, true)
        .unwrap()
        .is_none());
    assert_eq!(count(&store, "w"), 0);
}

/// The record survives a restart: closing and reopening the file keeps the
/// date, and the pass after it still counts from it.
#[test]
fn the_record_survives_a_restart() {
    let s = st("since-persist");
    let path = s.dir.join("since.db");
    let w = workflow("w", paused_for(Some("7d")), tag("x"));
    let facts = vec![fact("a", "stopped")];
    {
        let store = crate::store::Store::open(&path, false).unwrap();
        store.ensure_schema().unwrap();
        let lock = Arc::new(crate::store::StoreLock::new(store));
        rulesrun::track_since(&lock, &w, &facts, 5_000, true).unwrap();
    }
    let store = crate::store::Store::open(&path, false).unwrap();
    store.ensure_schema().unwrap();
    let lock = Arc::new(crate::store::StoreLock::new(store));
    let tp = rulesrun::track_since(&lock, &w, &facts, 5_000 + 8 * DAY, true)
        .unwrap()
        .unwrap();
    assert_eq!(tp.written, 0, "the row was there, nothing to add");
    let (matches, _) = rulesrun::evaluate_with(&w, &[fact("a", "stopped")], Some(&tp)).unwrap();
    assert_eq!(matches.len(), 1, "held since before the restart");
}

/// A torrent removed from the store takes its rows with it, but only with
/// its last copy.
#[test]
fn removing_a_torrent_removes_its_rows() {
    let store = store_lock();
    {
        let s = store.lock().unwrap();
        s.rule_since_write("w", &[("k", "aa"), ("k", "bb")], &[], 1)
            .unwrap();
        s.rule_since_write("other", &[("k", "aa")], &[], 1).unwrap();
    }
    store.lock().unwrap().delete_torrent("aa").unwrap();
    assert_eq!(count(&store, "w"), 1, "only bb is left");
    assert_eq!(count(&store, "other"), 0, "whichever workflow held it");
}

// ------------------------------------------------------- the pass, end to end

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

fn set_paused(s: &TestState, hash: &str, paused: bool) {
    s.state
        .store
        .lock()
        .unwrap()
        .set_paused(hash, "hoard", paused)
        .unwrap();
}

fn held(s: &TestState, hash: &str) -> bool {
    s.state
        .engines
        .get("hoard")
        .unwrap()
        .manager
        .get(&crate::store::hex20(hash).unwrap())
        .is_some()
}

fn tags(s: &TestState, hash: &str) -> Vec<String> {
    s.state.store.lock().unwrap().tags_of(hash)
}

/// Pretend the condition has been true since `ago` seconds: the one thing a
/// test cannot wait for.
fn backdate(s: &TestState, w: &Workflow, hash: &str, ago: i64) {
    let key = rules::timed_conditions(&w.when).unwrap()[0].key.clone();
    let store = s.state.store.lock().unwrap();
    store
        .rule_since_write(&w.id, &[], &[(&key, hash)], 0)
        .unwrap();
    store
        .rule_since_write(&w.id, &[(&key, hash)], &[], crate::store::now_secs() - ago)
        .unwrap();
}

fn rows(s: &TestState, w: &Workflow) -> i64 {
    s.state
        .store
        .lock()
        .unwrap()
        .rule_since_count(&w.id)
        .unwrap()
}

/// ⭐ The safe default. A torrent that has been paused for a month, before
/// this existed, is NOT deleted by the first pass of "paused for 7 days":
/// there is no row, so it counts from now.
#[tokio::test]
async fn a_torrent_paused_long_before_the_update_survives_the_first_pass() {
    let s = st("since-first");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow(
        "wf-first",
        paused_for(Some("7d")),
        vec![Action::Delete { with_files: false }],
    );

    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 0, "{r}");
    assert!(held(&s, &h), "not deleted on the first pass");
    assert_eq!(r["since_rows_written"], 1);

    // Six days on: still not. Eight: gone, and its row with it.
    backdate(&s, &w, &h, 6 * DAY);
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 0, "{r}");
    assert!(held(&s, &h));
    backdate(&s, &w, &h, 8 * DAY);
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 1, "{r}");
    assert!(!held(&s, &h), "deleted once it has held 7 days");
    assert_eq!(rows(&s, &w), 0, "a deleted torrent leaves no row");
}

/// False in between, and the count starts again.
#[tokio::test]
async fn the_count_restarts_when_the_condition_breaks() {
    let s = st("since-restart");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-restart", paused_for(Some("7d")), tag("stale"));
    run_one(&s.state, &w, false);
    backdate(&s, &w, &h, 8 * DAY);

    set_paused(&s, &h, false); // resumed before the next pass
    run_one(&s.state, &w, false);
    assert_eq!(rows(&s, &w), 0, "the break erased the date");
    assert!(tags(&s, &h).is_empty());

    set_paused(&s, &h, true);
    run_one(&s.state, &w, false);
    assert!(tags(&s, &h).is_empty(), "paused again: counting from now");
    assert_eq!(rows(&s, &w), 1);
}

/// With the date old enough, the action runs and the tag converges.
#[tokio::test]
async fn once_held_long_enough_the_action_runs() {
    let s = st("since-fires");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-fires", paused_for(Some("3d")), tag("stale"));
    run_one(&s.state, &w, false);
    assert!(tags(&s, &h).is_empty());
    backdate(&s, &w, &h, 3 * DAY);
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 1, "{r}");
    assert_eq!(tags(&s, &h), vec!["stale".to_string()]);
    // The next pass changes nothing, and writes no row.
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["since_rows_written"], 0, "{r}");
}

/// A workflow without `for` acts at once and writes no row.
#[tokio::test]
async fn a_workflow_without_for_acts_at_once_and_writes_no_row() {
    let s = st("since-none");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-none", paused_for(None), tag("stale"));
    let r = run_one(&s.state, &w, false);
    assert_eq!(r["applied"], 1, "{r}");
    assert!(r.get("since_rows_written").is_none());
    assert_eq!(tags(&s, &h), vec!["stale".to_string()]);
    assert_eq!(rows(&s, &w), 0);
}

/// Deleting a workflow clears its rows, however many.
#[tokio::test]
async fn deleting_a_workflow_deletes_its_rows() {
    let s = st("since-rmwf");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-rm", paused_for(Some("7d")), tag("stale"));
    {
        let store = s.state.store.lock().unwrap();
        store
            .put_workflow(&crate::store::StoredWorkflow {
                id: w.id.clone(),
                name: w.name.clone(),
                body: serde_json::to_string(&w).unwrap(),
                enabled: true,
                position: 0,
                interval_secs: 900,
                last_run: 0,
            })
            .unwrap();
    }
    run_one(&s.state, &w, false);
    assert_eq!(rows(&s, &w), 1);
    let r = remove(
        State(s.state.clone()),
        Path(w.id.clone()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(r.status().is_success());
    assert_eq!(rows(&s, &w), 0);
}

/// A preview says since when, and when the rule will fire, and writes nothing.
#[tokio::test]
async fn the_preview_shows_since_when_and_when_it_fires() {
    let s = st("since-preview");
    let a = add(&s, "aaa.bin");
    let b = add(&s, "bbbb.bin");
    set_paused(&s, &a, true);
    set_paused(&s, &b, true);
    let w = workflow("wf-prev", paused_for(Some("7d")), tag("stale"));
    run_one(&s.state, &w, false);
    backdate(&s, &w, &a, 2 * DAY); // a: since 2 days ago
    s.state
        .store
        .lock()
        .unwrap()
        .rule_since_write(
            &w.id,
            &[],
            &[(&rules::timed_conditions(&w.when).unwrap()[0].key, &b)],
            0,
        )
        .unwrap(); // b: no record, "from now"
    let before = rows(&s, &w);

    let body = serde_json::to_string(&w).unwrap();
    let r = preview(State(s.state.clone()), RawQuery(None), keyed(KEY), body).await;
    let v = body_json(r).await;
    assert_eq!(v["would_apply"], 0, "{v}");
    assert_eq!(v["waiting"], 2, "{v}");
    let now = crate::store::now_secs();
    let sample = v["waiting_sample"].as_array().unwrap();
    let of = |h: &str| sample.iter().find(|x| x["info_hash"] == h).unwrap();
    let wa = &of(&a)["waiting"][0];
    assert!(wa["since"].as_i64().unwrap() <= now - 2 * DAY + 5);
    let due_a = of(&a)["fires_at"].as_i64().unwrap();
    assert!((due_a - (now + 5 * DAY)).abs() < 10, "{due_a} vs {now}");
    assert!(of(&b)["waiting"][0]["since"].is_null(), "from now on");
    let due_b = of(&b)["fires_at"].as_i64().unwrap();
    assert!((due_b - (now + 7 * DAY)).abs() < 10);
    assert_eq!(rows(&s, &w), before, "a preview writes nothing");
}

/// A workflow switched off forgets: switching it on again starts from zero.
#[tokio::test]
async fn switching_a_workflow_off_clears_what_it_knew() {
    let s = st("since-off");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let mut w = workflow("wf-off", paused_for(Some("7d")), tag("stale"));
    run_one(&s.state, &w, false);
    assert_eq!(rows(&s, &w), 1);
    w.enabled = false;
    let r = save(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::to_string(&w).unwrap(),
    )
    .await;
    assert!(r.status().is_success());
    assert_eq!(rows(&s, &w), 0);
}

// ----------------------------------------------------------------- the scale

/// ⭐ The cost at a million torrents, for `state = seeding for 7d`. Not run by
/// default: `cargo test --release since_at_a_million -- --ignored --nocapture`.
/// The store is a real file (WAL) in `SINCE_BENCH_DIR`, or the temp dir.
#[test]
#[ignore]
fn since_at_a_million_torrents() {
    use std::time::Instant;
    let n: usize = std::env::var("SINCE_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let dir = std::env::var("SINCE_BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let path = dir.join(format!("since-bench-{}.db", std::process::id()));
    let store = crate::store::Store::open(&path, false).unwrap();
    store.ensure_schema().unwrap();
    // The reader is a connection of its own, as in production.
    let reader = crate::store::Store::open(&path, true).ok();
    let lock = Arc::new(crate::store::StoreLock::with_reader(store, reader));

    let w = workflow(
        "bench",
        cond("state", Op::Eq, "seeding", Some("7d")),
        tag("x"),
    );
    let mut facts: Vec<Facts> = hashes(n).iter().map(|h| fact(h, "seeding")).collect();
    let pass = |facts: &[Facts], at: i64, label: &str| {
        let t = Instant::now();
        let tp = rulesrun::track_since(&lock, &w, facts, at, true)
            .unwrap()
            .unwrap();
        let track = t.elapsed();
        let t = Instant::now();
        let (m, _) = rulesrun::evaluate_with(&w, facts, Some(&tp)).unwrap();
        println!(
            "{label}: track {:.2?} (rows written {}), evaluate {:.2?} ({} due), rows in table {}",
            track,
            tp.written,
            t.elapsed(),
            m.len(),
            lock.lock().unwrap().rule_since_count("bench").unwrap()
        );
    };
    pass(&facts, 1_000, "1st pass, all seeding   ");
    pass(&facts, 2_000, "2nd pass, no change     ");
    let one_percent = n / 100;
    for f in facts.iter_mut().take(one_percent) {
        f.state = "stopped".into();
    }
    pass(&facts, 3_000, "3rd pass, 1% turn false ");
    for f in facts.iter_mut().take(one_percent) {
        f.state = "seeding".into();
    }
    pass(&facts, 4_000, "4th pass, 1% turn true  ");
    pass(&facts, 1_000 + 8 * DAY, "5th pass, 8 days later   ");
    let t = Instant::now();
    let mut purged = 0;
    loop {
        let n = lock
            .lock()
            .unwrap()
            .rule_since_purge_workflow("bench", rulesrun::SINCE_BATCH as i64)
            .unwrap();
        if n == 0 {
            break;
        }
        purged += n;
    }
    println!(
        "purge of the workflow: {purged} rows in {:.2?}",
        t.elapsed()
    );
    let _ = std::fs::remove_file(&path);
}
