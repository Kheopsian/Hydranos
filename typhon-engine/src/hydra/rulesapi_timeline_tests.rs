//! `GET /api/workflows/for/:info_hash`: which delay rules aim at ONE torrent,
//! since when, and when they will act.

use super::*;
use crate::api::testing::{body_json, keyed, state_from, TestState};
use crate::rules::{Action, Cond, Node, Op, Trigger};
use axum::extract::{Path, RawQuery, State};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const DAY: i64 = 86400;

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn cond(field: &str, op: Op, value: &str, hold: Option<&str>, tag: Option<&str>) -> Node {
    Node::Cond(Cond {
        field: field.into(),
        op,
        value: value.into(),
        hold: hold.map(String::from),
        tag: tag.map(String::from),
    })
}

fn paused_for(hold: &str) -> Node {
    cond("user_paused", Op::Eq, "true", Some(hold), None)
}

fn workflow(id: &str, when: Node) -> Workflow {
    Workflow {
        id: id.into(),
        name: format!("rule {id}"),
        enabled: true,
        position: 0,
        trigger: Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then: vec![Action::AddTags {
            tags: vec!["stale".into()],
        }],
        cap: rules::DEFAULT_CAP,
    }
}

fn store_wf(s: &TestState, w: &Workflow) {
    s.state
        .store
        .lock()
        .unwrap()
        .put_workflow(&crate::store::StoredWorkflow {
            id: w.id.clone(),
            name: w.name.clone(),
            body: serde_json::to_string(w).unwrap(),
            enabled: w.enabled,
            position: 0,
            interval_secs: 900,
            last_run: 0,
        })
        .unwrap();
}

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

/// A row of the record: the `for` condition of `w` has been true since `ago`.
fn held_since(s: &TestState, w: &Workflow, hash: &str, ago: i64) {
    let key = rules::timed_conditions(&w.when).unwrap()[0].key.clone();
    s.state
        .store
        .lock()
        .unwrap()
        .rule_since_write(&w.id, &[(&key, hash)], &[], crate::store::now_secs() - ago)
        .unwrap();
}

fn wear(s: &TestState, hash: &str, tag: &str, ago: i64) {
    let store = s.state.store.lock().unwrap();
    store.set_tags(hash, &[tag.to_string()]).unwrap();
    store.put_tag_since_for_tests(hash, tag, Some(crate::store::now_secs() - ago));
}

async fn ask(s: &TestState, hash: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let r = for_torrent(
        State(s.state.clone()),
        Path(hash.to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    let status = r.status();
    (status, body_json(r).await)
}

fn near(got: &serde_json::Value, want: i64) -> bool {
    got.as_i64().is_some_and(|g| (g - want).abs() <= 5)
}

fn rows(s: &TestState, w: &Workflow) -> i64 {
    s.state
        .store
        .lock()
        .unwrap()
        .rule_since_count(&w.id)
        .unwrap()
}

/// Paused for 3 days under a rule "paused for 7 days": it acts in 4.
#[tokio::test]
async fn a_pause_held_three_days_acts_in_four() {
    let s = st("timeline-a");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-a", paused_for("7d"));
    store_wf(&s, &w);
    held_since(&s, &w, &h, 3 * DAY);
    let now = crate::store::now_secs();

    let (status, v) = ask(&s, &h).await;
    assert_eq!(status, 200, "{v}");
    assert!(near(&v["computed_at"], now), "{v}");
    let list = v["workflows"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    let e = &list[0];
    assert_eq!(e["id"], "wf-a", "{v}");
    assert_eq!(e["name"], "rule wf-a", "{v}");
    assert_eq!(e["then"][0]["type"], "add_tags", "{v}");
    assert!(near(&e["fires_at"], now - 3 * DAY + 7 * DAY), "{v}");
    assert_eq!(e["blocked_by"].as_array().unwrap().len(), 0, "{v}");
    let c = &e["conditions"][0];
    assert_eq!(c["field"], "user_paused", "{v}");
    assert_eq!(c["holds"], false, "3 of 7 days: not yet: {v}");
    assert_eq!(c["current"], true, "{v}");
    assert_eq!(c["for_secs"], 7 * DAY, "{v}");
    assert!(near(&c["since"], now - 3 * DAY), "{v}");
    assert!(near(&c["due"], now + 4 * DAY), "{v}");
}

/// Another condition is false: no date, and the one that is false is named
/// with the value it has now.
#[tokio::test]
async fn a_false_condition_blocks_the_rule_and_is_named() {
    let s = st("timeline-b");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow(
        "wf-b",
        Node::All {
            of: vec![paused_for("7d"), cond("ratio", Op::Ge, "1", None, None)],
        },
    );
    store_wf(&s, &w);
    held_since(&s, &w, &h, 3 * DAY);

    let (_, v) = ask(&s, &h).await;
    let e = &v["workflows"][0];
    assert!(e["fires_at"].is_null(), "{v}");
    let blocked = e["blocked_by"].as_array().unwrap();
    assert_eq!(blocked.len(), 1, "{v}");
    assert_eq!(blocked[0]["field"], "ratio", "{v}");
    assert_eq!(blocked[0]["current"], 0.0, "{v}");
    assert_eq!(blocked[0]["holds"], false, "{v}");
    assert_eq!(e["conditions"].as_array().unwrap().len(), 2, "{v}");
}

/// True now but the last pass has not seen it yet: no row, so the count starts
/// from this very moment (`since` null) and the date is now plus the delay.
/// Reading writes nothing.
#[tokio::test]
async fn a_condition_not_yet_recorded_counts_from_now_and_nothing_is_written() {
    let s = st("timeline-c");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    let w = workflow("wf-c", paused_for("7d"));
    store_wf(&s, &w);
    let now = crate::store::now_secs();

    let (_, v) = ask(&s, &h).await;
    let e = &v["workflows"][0];
    let c = &e["conditions"][0];
    assert!(c["since"].is_null(), "{v}");
    assert_eq!(c["holds"], false, "the count has not started: {v}");
    assert!(near(&c["due"], now + 7 * DAY), "{v}");
    assert!(near(&e["fires_at"], now + 7 * DAY), "{v}");
    assert_eq!(rows(&s, &w), 0, "a question is not a pass");
}

/// A rule on the age of a tag has no row at all: the date is the tag's.
#[tokio::test]
async fn a_tag_age_rule_alone_gives_the_day_the_tag_is_old_enough() {
    let s = st("timeline-d");
    let h = add(&s, "aaa.bin");
    wear(&s, &h, "keep", 3 * DAY);
    let w = workflow("wf-d", cond("tag_age", Op::Gt, "7d", None, Some("keep")));
    store_wf(&s, &w);
    let now = crate::store::now_secs();

    let (_, v) = ask(&s, &h).await;
    let e = &v["workflows"][0];
    assert!(near(&e["fires_at"], now - 3 * DAY + 7 * DAY), "{v}");
    let c = &e["conditions"][0];
    assert_eq!(c["field"], "tag_age", "{v}");
    assert_eq!(c["tag"], "keep", "{v}");
    assert_eq!(c["holds"], false, "{v}");
    assert_eq!(c["for_secs"], 7 * DAY, "{v}");
    assert!(near(&c["since"], now - 3 * DAY), "{v}");
    assert!(near(&c["due"], now + 4 * DAY), "{v}");
}

/// A `tag_age` that also carries a `for`: the rule is listed from the day the
/// tag is set, and acts when the tag is old enough AND the `for` has then run:
/// tag date + 3 days + 1 hour. The bar goes from the tag to that date.
#[tokio::test]
async fn a_tag_age_with_a_for_is_listed_and_dated_after_both_waits() {
    let s = st("timeline-e");
    let h = add(&s, "aaa.bin");
    wear(&s, &h, "noHL", 600);
    let w = workflow(
        "wf-e",
        Node::All {
            of: vec![cond("tag_age", Op::Ge, "3d", Some("1h"), Some("noHL"))],
        },
    );
    store_wf(&s, &w);
    let now = crate::store::now_secs();

    let (_, v) = ask(&s, &h).await;
    let list = v["workflows"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{v}");
    let e = &list[0];
    let due = now - 600 + 3 * DAY + 3600;
    assert!(near(&e["fires_at"], due), "{v}");
    let c = &e["conditions"][0];
    assert_eq!(c["holds"], false, "{v}");
    assert_eq!(c["tag"], "noHL", "{v}");
    assert!(near(&c["since"], now - 600), "{v}");
    assert!(near(&c["due"], due), "{v}");
    let span = c["for_secs"].as_i64().unwrap();
    assert!((span - (3 * DAY + 3600)).abs() <= 5, "{v}");
}

/// A torrent no delay rule aims at: an empty list, not an error.
#[tokio::test]
async fn a_torrent_no_rule_aims_at_has_an_empty_list() {
    let s = st("timeline-e");
    let h = add(&s, "aaa.bin"); // wears no tag
    set_paused(&s, &h, false); // an added torrent starts stopped
    store_wf(&s, &workflow("wf-e1", paused_for("7d")));
    store_wf(
        &s,
        &workflow("wf-e2", cond("tag_age", Op::Gt, "7d", None, Some("keep"))),
    );
    let (status, v) = ask(&s, &h).await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["workflows"].as_array().unwrap().len(), 0, "{v}");
    assert!(v["computed_at"].as_i64().unwrap() > 0, "{v}");
}

/// Off, on an event, without a delay, or with nothing left to do: not listed.
#[tokio::test]
async fn disabled_event_plain_and_already_done_rules_are_left_out() {
    let s = st("timeline-f");
    let h = add(&s, "aaa.bin");
    set_paused(&s, &h, true);
    wear(&s, &h, "stale", 9 * DAY);

    let mut off = workflow("wf-off", paused_for("7d"));
    off.enabled = false;
    store_wf(&s, &off);

    let mut event = workflow(
        "wf-event",
        cond("tag_age", Op::Gt, "7d", None, Some("stale")),
    );
    event.trigger = Trigger::Added;
    store_wf(&s, &event);

    store_wf(
        &s,
        &workflow("wf-plain", cond("user_paused", Op::Eq, "true", None, None)),
    );

    // Its action is already true of the torrent: it wears `stale`.
    store_wf(&s, &workflow("wf-done", paused_for("7d")));

    let mut live = workflow("wf-live", paused_for("2d"));
    live.then = vec![Action::AddTags {
        tags: vec!["other".into()],
    }];
    store_wf(&s, &live);

    let (_, v) = ask(&s, &h).await;
    let ids: Vec<&str> = v["workflows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["wf-live"], "{v}");
}

/// A torrent nobody holds, or not a hash at all: 404.
#[tokio::test]
async fn an_unknown_torrent_is_not_found() {
    let s = st("timeline-g");
    let (status, _) = ask(&s, &"ab".repeat(20)).await;
    assert_eq!(status, 404);
    let (status, _) = ask(&s, "not-a-hash").await;
    assert_eq!(status, 404);
}

/// The route is behind the same key as the rest.
#[tokio::test]
async fn the_route_refuses_a_caller_with_no_key() {
    let s = st("timeline-key");
    let r = for_torrent(
        State(s.state.clone()),
        Path("ab".repeat(20)),
        RawQuery(None),
        axum::http::HeaderMap::new(),
    )
    .await;
    assert_eq!(r.status(), 401);
}
