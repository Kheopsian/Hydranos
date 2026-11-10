//! The buttons and the timer of the workflows, through the real engines: a
//! dry run that changes nothing and says what it would do, "run now" and what
//! it refuses, the activity log it leaves, and one tick of the timer.

use super::*;
use crate::api::testing::{body_json, keyed, state_from, TestState};
use crate::rules::{Action, Cond, Node, Op, Trigger};
use axum::extract::{Path, RawQuery, State};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const DAY: i64 = 86400;

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    let mut piece = [0xEEu8; 20];
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

/// A paused torrent of the hoard engine; its name is its file's.
fn add(s: &TestState, name: &str) -> String {
    let dir = s.dir.join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let hash = crate::api::add_torrent_bytes(
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
    .0;
    s.state
        .store
        .lock()
        .unwrap()
        .set_paused(&hash, "hoard", true)
        .unwrap();
    hash
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

fn paused(hold: Option<&str>) -> Node {
    Node::Cond(Cond {
        field: "user_paused".into(),
        op: Op::Eq,
        value: "true".into(),
        hold: hold.map(String::from),
        tag: None,
    })
}

fn workflow(id: &str, when: Node, then: Vec<Action>) -> Workflow {
    Workflow {
        id: id.into(),
        name: format!("wf {id}"),
        enabled: true,
        position: 0,
        trigger: Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then,
        cap: rules::DEFAULT_CAP,
    }
}

fn delete() -> Vec<Action> {
    vec![Action::Delete { with_files: false }]
}

fn stored(w: &Workflow, last_run: i64) -> crate::store::StoredWorkflow {
    crate::store::StoredWorkflow {
        id: w.id.clone(),
        name: w.name.clone(),
        body: serde_json::to_string(w).unwrap(),
        enabled: w.enabled,
        position: 0,
        interval_secs: w.interval_secs,
        last_run,
    }
}

/// Written, and marked as last run at `last_run` when that is not 0 (the
/// store takes the date from the run, not from the row it is written with).
fn save(s: &TestState, w: &Workflow, last_run: i64) {
    let store = s.state.store.lock().unwrap();
    store.put_workflow(&stored(w, last_run)).unwrap();
    if last_run != 0 {
        store.mark_workflow_run(&w.id, last_run).unwrap();
    }
}

fn activity_rows(s: &TestState) -> Vec<crate::store::ActivityEntry> {
    s.state
        .store
        .lock()
        .unwrap()
        .workflow_activity(100)
        .unwrap()
}

fn last_run(s: &TestState, id: &str) -> i64 {
    s.state
        .store
        .lock()
        .unwrap()
        .workflow(id)
        .unwrap()
        .unwrap()
        .last_run
}

async fn run_now_of(s: &TestState, id: &str, query: &str) -> (StatusCode, serde_json::Value) {
    let resp = run_now(
        State(s.state.clone()),
        Path(id.to_string()),
        RawQuery(Some(query.to_string())),
        keyed(KEY),
    )
    .await;
    let code = resp.status();
    (code, body_json(resp).await)
}

/// ⭐ A dry run says what a real one would do and does none of it: nothing is
/// deleted, nothing is logged as applied, the workflow is not marked as run.
#[tokio::test]
async fn a_dry_run_reports_what_it_would_do_and_does_none_of_it() {
    let s = st("lot6-dry");
    let (a, b) = (add(&s, "aaa.bin"), add(&s, "bbb.bin"));
    let w = workflow("dry", paused(None), delete());
    save(&s, &w, 0);

    let (code, v) = run_now_of(&s, "dry", "dry=1").await;

    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["matched"], 2, "{v}");
    assert_eq!(v["would_apply"], 2);
    assert_eq!(v["skipped"], 0);
    assert_eq!(v["rechecked"], 0);
    assert_eq!(v["no_longer_true"], 0);
    assert!(v.get("applied").is_none(), "a dry run applies nothing: {v}");
    assert!(held(&s, &a) && held(&s, &b), "nothing was deleted");
    assert_eq!(last_run(&s, "dry"), 0, "and the workflow did not run");
    assert!(activity_rows(&s).is_empty(), "nor was anything logged");
}

/// A dry run that finds nothing leaves its own line in the activity: the
/// operator pressing it sees that it ran.
#[tokio::test]
async fn a_dry_run_that_matches_nothing_says_so_in_the_activity() {
    let s = st("lot6-dry-none");
    add(&s, "ccc.bin");
    let never = Node::Cond(Cond {
        field: "category".into(),
        op: Op::Eq,
        value: "no-such-category".into(),
        hold: None,
        tag: None,
    });
    let w = workflow("none", never, delete());
    save(&s, &w, 0);

    let before = crate::store::now_secs();
    let v = run_one(&s.state, &w, true);

    assert_eq!(v["would_apply"], 0, "{v}");
    assert_eq!(v["matched"], 0);
    let rows = activity_rows(&s);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].at >= before && rows[0].at <= crate::store::now_secs(),
        "dated now, not at the start of time: {}",
        rows[0].at
    );
    assert_eq!(
        (
            rows[0].workflow_id.as_str(),
            rows[0].workflow_name.as_str(),
            rows[0].action.as_str(),
            rows[0].outcome.as_str()
        ),
        ("none", "wf none", "dry_run", "dry_run_no_match")
    );
}

/// A dry run of a workflow with a delay says how many are still counting.
#[tokio::test]
async fn a_dry_run_of_a_delayed_workflow_counts_those_still_waiting() {
    let s = st("lot6-dry-wait");
    add(&s, "ddd.bin");
    add(&s, "eee.bin");
    let w = workflow("wait", paused(Some("7d")), delete());

    let v = run_one(&s.state, &w, true);

    assert_eq!(v["would_apply"], 0, "{v}");
    assert_eq!(v["waiting"], 2, "both have just become true: {v}");
}

/// Run now, for real: it deletes, says how many, logs each, and marks the
/// workflow as run. The activity then names the torrent and what was done.
#[tokio::test]
async fn run_now_applies_logs_and_marks_the_run() {
    let s = st("lot6-run");
    let (a, b) = (add(&s, "fff.bin"), add(&s, "ggg.bin"));
    let w = workflow("go", paused(None), delete());
    save(&s, &w, 0);

    let (code, v) = run_now_of(&s, "go", "").await;

    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["matched"], 2);
    assert_eq!(v["applied"], 2);
    assert_eq!(v["failed"], 0);
    assert!(v.get("dry_run").is_none());
    assert!(!held(&s, &a) && !held(&s, &b), "deleted");
    assert!(last_run(&s, "go") > 0, "marked as run");
    let rows = activity_rows(&s);
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(
            (
                r.action.as_str(),
                r.outcome.as_str(),
                r.workflow_name.as_str()
            ),
            ("delete", "applied", "wf go")
        );
    }
    let mut hashes: Vec<&str> = rows.iter().map(|r| r.info_hash.as_str()).collect();
    hashes.sort();
    let mut want = [a.as_str(), b.as_str()];
    want.sort();
    assert_eq!(hashes, want);

    // The activity route serves the same rows, newest first, in its words.
    let resp = activity(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    let v = body_json(resp).await;
    let list = v["activity"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["workflow_name"], "wf go");
    assert_eq!(list[0]["action"], "delete");
    assert_eq!(list[0]["outcome"], "applied");
    assert_eq!(list[0]["detail"], "");
    assert!(list[0]["torrent_name"].as_str().unwrap().ends_with(".bin"));
    assert!(list[0]["at"].as_i64().unwrap() > 0);
}

/// "Run now" refuses what it cannot run, and says why: no such workflow, a
/// body that does not parse, a workflow that waits for an event, no key.
#[tokio::test]
async fn run_now_refuses_what_it_cannot_run() {
    let s = st("lot6-refuse");
    let a = add(&s, "hhh.bin");

    let (code, v) = run_now_of(&s, "ghost", "").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "no such workflow");

    let mut broken = stored(&workflow("broken", paused(None), delete()), 0);
    broken.body = "this is not a workflow".into();
    s.state.store.lock().unwrap().put_workflow(&broken).unwrap();
    let (code, v) = run_now_of(&s, "broken", "").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("expected"), "{v}");

    for (trigger, words) in [
        (
            Trigger::Added,
            "this workflow runs when a torrent is added; use Preview to see what it would do to the last day's arrivals",
        ),
        (
            Trigger::Completed,
            "this workflow runs when a download completes; use Preview to see which downloads it would act on",
        ),
    ] {
        let mut w = workflow("event", paused(None), delete());
        w.trigger = trigger;
        save(&s, &w, 0);
        let (code, v) = run_now_of(&s, "event", "").await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"], words);
    }

    let resp = run_now(
        State(s.state.clone()),
        Path("event".into()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(held(&s, &a), "none of these deleted anything");
    assert!(activity_rows(&s).is_empty());
}

/// ⭐ One tick runs the workflows that are due, and only those: an enabled
/// scheduled one past its interval runs and is marked; one run a moment ago,
/// a disabled one, an event one and one whose body does not parse do not.
/// Activity older than a week goes, a day's stays.
#[tokio::test]
async fn a_tick_runs_what_is_due_and_nothing_else() {
    let s = st("lot6-tick");
    let now = crate::store::now_secs();
    let (due, recent, off, event) = (
        add(&s, "iii.bin"),
        add(&s, "jjj.bin"),
        add(&s, "kkk.bin"),
        add(&s, "lll.bin"),
    );
    let tag_one = |id: &str, tag: &str| {
        workflow(
            id,
            paused(None),
            vec![Action::AddTags {
                tags: vec![tag.into()],
            }],
        )
    };
    // Only one torrent may match each workflow: they differ by name.
    let by_name = |id: &str, name: &str, tag: &str| {
        let mut w = tag_one(id, tag);
        w.when = Node::Cond(Cond {
            field: "name".into(),
            op: Op::Eq,
            value: name.into(),
            hold: None,
            tag: None,
        });
        w
    };
    let w_due = by_name("due", "iii.bin", "ran-due");
    let w_recent = by_name("recent", "jjj.bin", "ran-recent");
    let mut w_off = by_name("off", "kkk.bin", "ran-off");
    w_off.enabled = false;
    let mut w_event = by_name("event", "lll.bin", "ran-event");
    w_event.trigger = Trigger::Completed;
    save(&s, &w_due, now - 901);
    save(&s, &w_recent, now - 899);
    save(&s, &w_off, 0);
    save(&s, &w_event, 0);
    let mut broken = stored(&by_name("broken", "iii.bin", "ran-broken"), 0);
    broken.body = "nope".into();
    s.state.store.lock().unwrap().put_workflow(&broken).unwrap();
    {
        let store = s.state.store.lock().unwrap();
        for (at, hash) in [(now - 8 * DAY, "old"), (now - DAY, "recent")] {
            store
                .log_workflow_activity(&crate::store::ActivityEntry {
                    at,
                    workflow_id: "x".into(),
                    workflow_name: "x".into(),
                    info_hash: hash.into(),
                    action: "pause".into(),
                    outcome: "applied".into(),
                    ..Default::default()
                })
                .unwrap();
        }
    }

    tick(&s.state, now).await;

    let tags = |h: &str| s.state.store.lock().unwrap().tags_of(h);
    assert_eq!(tags(&due), ["ran-due"], "due: ran");
    assert!(tags(&recent).is_empty(), "run 899 s ago: not due");
    assert!(tags(&off).is_empty(), "disabled");
    assert!(tags(&event).is_empty(), "an event is never on the clock");
    assert!(last_run(&s, "due") >= now, "marked as run now");
    assert_eq!(last_run(&s, "recent"), now - 899);
    assert_eq!(last_run(&s, "event"), 0, "an event workflow is not marked");
    assert_eq!(last_run(&s, "off"), 0);
    let hashes: Vec<String> = activity_rows(&s).into_iter().map(|r| r.info_hash).collect();
    assert!(hashes.contains(&"recent".to_string()), "a day old stays");
    assert!(!hashes.contains(&"old".to_string()), "a week old goes");
    assert!(hashes.contains(&due), "and the run itself is logged");
}

/// The link index's page: the keys the page reads, and the refresh period the
/// scanner works to. (The counts are the scanner's own, shared by the whole
/// process, so their values are not read here.)
#[tokio::test]
async fn the_link_status_has_every_figure_the_page_reads() {
    let s = st("lot6-links");
    let resp = links_status(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let keys = |o: &serde_json::Value| -> Vec<String> {
        let mut k: Vec<String> = o.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        [
            "catalogue",
            "counted_at",
            "data_missing",
            "files",
            "measured",
            "oldest_measurement",
            "orphans",
            "partly_missing",
            "queued",
            "refresh_secs",
            "sweep"
        ]
    );
    assert_eq!(
        keys(&v["sweep"]),
        [
            "cycles",
            "done",
            "files_per_sec",
            "last_batch_at",
            "last_cycle_secs",
            "started",
            "total"
        ]
    );
    assert_eq!(keys(&v["orphans"]), ["cycles", "dropped"]);
    assert_eq!(v["refresh_secs"], crate::linkscan::REFRESH_SECS);
    let resp = links_status(
        State(s.state.clone()),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
