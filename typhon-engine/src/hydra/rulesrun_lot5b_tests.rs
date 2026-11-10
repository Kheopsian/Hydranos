//! The delay machinery of a workflow read one question at a time: which
//! conditions wait, for what, until when, and what "ready" means. The pass
//! tests of `rulesapi` exercise it whole; each of these pins one answer.

use super::*;
use crate::rules::{Cond, Node, Op, SinceRows, Timing, Trigger};

const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

fn cond(field: &str, op: Op, value: &str, hold: Option<&str>, tag: Option<&str>) -> Cond {
    Cond {
        field: field.into(),
        op,
        value: value.into(),
        hold: hold.map(String::from),
        tag: tag.map(String::from),
    }
}

fn paused_for(hold: &str) -> Cond {
    cond("user_paused", Op::Eq, "true", Some(hold), None)
}

fn workflow(when: Node, then: Vec<Action>) -> Workflow {
    Workflow {
        id: "w".into(),
        name: "wf".into(),
        enabled: true,
        position: 0,
        trigger: Trigger::Schedule,
        interval_secs: rules::DEFAULT_INTERVAL_SECS,
        when,
        then,
        cap: rules::DEFAULT_CAP,
    }
}

fn tag_x() -> Vec<Action> {
    vec![Action::AddTags {
        tags: vec!["x".into()],
    }]
}

fn rows_of(c: &Cond, hash: &str, since: i64) -> SinceRows {
    let mut rows = SinceRows::new();
    rows.entry(rules::condition_key(c))
        .or_default()
        .insert(hash.into(), since);
    rows
}

fn pass(w: &Workflow, rows: SinceRows) -> TimedPass {
    TimedPass {
        timed: rules::timed_conditions(&w.when).unwrap(),
        timing: Arc::new(Timing::new(NOW, rows)),
        written: 0,
    }
}

fn paused(hash: &str) -> Facts {
    Facts {
        info_hash: hash.into(),
        name: format!("torrent {hash}"),
        user_paused: true,
        ..Default::default()
    }
}

// ----------------------------------------------------------------- the record

/// ⭐ The record of a workflow is read whole however it is cut into slices: a
/// slice that is exactly full is not the last one, and one that is not full
/// is.
#[test]
fn the_record_is_read_whole_whatever_the_slice() {
    let s = crate::api::testing::state_from("lot5b-since", "");
    let put: Vec<(String, String)> = (0..5)
        .map(|n| (if n < 3 { "ka" } else { "kb" }.to_string(), format!("h{n}")))
        .collect();
    {
        let store = s.state.store.lock().unwrap();
        let refs: Vec<(&str, &str)> = put.iter().map(|(k, h)| (k.as_str(), h.as_str())).collect();
        store.rule_since_write("w", &refs, &[], 77).unwrap();
        store
            .rule_since_write("other", &[("ka", "zz")], &[], 5)
            .unwrap();
    }
    let mut want = SinceRows::new();
    for (k, h) in &put {
        want.entry(k.clone()).or_default().insert(h.clone(), 77);
    }
    for slice in [0, 1, 2, 4, 5, 6, 20_000] {
        let (rows, _) = read_since(&s.state.store, "w", slice).unwrap();
        assert_eq!(rows, want, "slice {slice}");
    }
    let (none, _) = read_since(&s.state.store, "nobody", 2).unwrap();
    assert!(none.is_empty());
}

/// What a torrent has been counting is what the pass shows for it, and a
/// torrent with no date has nothing to show.
#[test]
fn a_torrent_shows_the_conditions_it_has_a_date_for() {
    let c = paused_for("3d");
    let w = workflow(Node::Cond(c.clone()), tag_x());
    let tp = pass(&w, rows_of(&c, "h1", NOW - DAY));

    let held = tp.held_for("h1");
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].field, "user_paused");
    assert_eq!(held[0].for_secs, 3 * DAY);
    assert_eq!(held[0].since, Some(NOW - DAY));
    assert_eq!(held[0].due, NOW + 2 * DAY);
    assert!(tp.held_for("h2").is_empty(), "no row, nothing to show");
}

// ------------------------------------------------------------- the questions

/// `has_delay`: a `for`, or an "older than" on a tag; not "newer than", which
/// only gets less true, and not a `for` that is not a duration.
#[test]
fn only_a_wait_for_time_is_a_delay() {
    let plain = |c: Cond| workflow(Node::Cond(c), tag_x());
    assert!(!has_delay(&plain(cond("ratio", Op::Gt, "1", None, None))));
    assert!(has_delay(&plain(paused_for("1d"))));
    assert!(has_delay(&plain(cond(
        "tag_age",
        Op::Gt,
        "1d",
        None,
        Some("k")
    ))));
    assert!(has_delay(&plain(cond(
        "tag_age",
        Op::Ge,
        "1d",
        None,
        Some("k")
    ))));
    assert!(!has_delay(&plain(cond(
        "tag_age",
        Op::Lt,
        "1d",
        None,
        Some("k")
    ))));
    assert!(!has_delay(&plain(paused_for("soon"))), "not a duration");
    assert!(has_delay(&workflow(
        Node::All {
            of: vec![
                Node::Cond(cond("ratio", Op::Gt, "1", None, None)),
                Node::Not {
                    of: Box::new(Node::Cond(paused_for("1d")))
                },
            ]
        },
        tag_x()
    )));
}

/// An "older than" tag condition that carries a `for` is an aged hold; the
/// same on another field, on "newer than", or with no `for` is not.
#[test]
fn only_an_older_than_on_a_tag_with_a_for_is_an_aged_hold() {
    assert!(is_aged_hold(&cond(
        "tag_age",
        Op::Gt,
        "1d",
        Some("1d"),
        Some("k")
    )));
    assert!(is_aged_hold(&cond(
        "tag_age",
        Op::Ge,
        "1d",
        Some("1d"),
        Some("k")
    )));
    assert!(!is_aged_hold(&cond(
        "tag_age",
        Op::Lt,
        "1d",
        Some("1d"),
        Some("k")
    )));
    assert!(!is_aged_hold(&cond(
        "tag_age",
        Op::Gt,
        "1d",
        None,
        Some("k")
    )));
    assert!(!is_aged_hold(&cond("ratio", Op::Gt, "1", Some("1d"), None)));
    assert!(!is_aged_hold(&cond(
        "seeding_time",
        Op::Ge,
        "1d",
        Some("1d"),
        None
    )));
}

/// "Now" is the rule holding at the pass's clock; "waiting on time alone" is
/// the rule that would hold if the delay were over.
#[test]
fn a_rule_holds_now_only_once_its_delay_has_run() {
    let c = paused_for("1d");
    let w = workflow(Node::Cond(c.clone()), tag_x());
    let f = paused("h1");

    let old = pass(&w, rows_of(&c, "h1", NOW - 2 * DAY));
    let gates = TimedGates::new(&w, &old).unwrap();
    assert!(gates.holds_now(&f), "paused for two days: due");
    assert!(!gates.waits_on_time_alone(&f), "nothing left to wait for");

    let fresh = pass(&w, rows_of(&c, "h1", NOW - 3600));
    let gates = TimedGates::new(&w, &fresh).unwrap();
    assert!(!gates.holds_now(&f), "paused an hour ago: not yet");
    assert!(gates.waits_on_time_alone(&f), "only time is missing");

    let not_paused = Facts {
        user_paused: false,
        ..paused("h1")
    };
    assert!(!gates.holds_now(&not_paused));
    assert!(!gates.waits_on_time_alone(&not_paused), "not only time");
}

/// ⭐ A torrent waits only for the delays whose condition is true of it and
/// that have not run out: a `for` on something false is not a wait, and one
/// that has run its course is not either. Each wait carries its own date, and
/// the rule's date is when the last of them comes due.
#[test]
fn a_torrent_waits_only_for_the_delays_that_are_true_and_not_yet_due() {
    let waiting = paused_for("1d");
    let false_one = cond("ratio", Op::Gt, "5", Some("1d"), None);
    let done = cond("category", Op::Eq, "books", Some("1d"), None);
    let w = workflow(
        Node::All {
            of: vec![
                Node::Cond(waiting.clone()),
                Node::Cond(false_one),
                Node::Cond(done.clone()),
            ],
        },
        tag_x(),
    );
    let f = Facts {
        category: "books".into(),
        ratio: 1.0,
        ..paused("h1")
    };
    let tp = pass(&w, rows_of(&done, "h1", NOW - 2 * DAY));
    let gates = TimedGates::new(&w, &tp).unwrap();

    let (list, fires_at) = gates.waiting(&tp, &f);

    assert_eq!(list.len(), 1, "{list:?}");
    assert_eq!(list[0].field, "user_paused");
    assert_eq!(list[0].for_secs, DAY);
    assert_eq!(list[0].since, None, "true now for the first time");
    assert_eq!(list[0].due, NOW + DAY);
    assert_eq!(fires_at, None, "the ratio is not time's to mend");
}

/// The rule's date: when every delay it waits for has come due.
#[test]
fn the_rule_fires_when_the_last_of_its_delays_comes_due() {
    let a = paused_for("1d");
    let b = cond("category", Op::Eq, "books", Some("2d"), None);
    let w = workflow(
        Node::All {
            of: vec![Node::Cond(a.clone()), Node::Cond(b.clone())],
        },
        tag_x(),
    );
    let f = Facts {
        category: "books".into(),
        ..paused("h1")
    };
    let mut rows = rows_of(&a, "h1", NOW - 3600);
    rows.extend(rows_of(&b, "h1", NOW - 3600));
    let tp = pass(&w, rows);
    let gates = TimedGates::new(&w, &tp).unwrap();
    let (list, fires_at) = gates.waiting(&tp, &f);
    assert_eq!(list.len(), 2);
    assert_eq!(fires_at, Some(NOW - 3600 + 2 * DAY));
    assert_eq!(tp.timing.elapsed(), 0, "the clock is put back");
}

/// ⭐ The preview counts the torrents the rule would act on but for time, and
/// not those it has nothing left to do for: a tag already worn, or a webhook
/// that is told again whatever the torrent looks like.
#[test]
fn the_preview_skips_what_there_is_nothing_left_to_do_for() {
    let c = paused_for("1d");
    let w = workflow(Node::Cond(c.clone()), tag_x());
    let mut rows = rows_of(&c, "h1", NOW - 3600);
    rows.entry(rules::condition_key(&c))
        .or_default()
        .insert("h2".into(), NOW - 3600);
    let tp = pass(&w, rows);
    let (a, b) = (
        paused("h1"),
        Facts {
            tags: vec!["x".into()],
            ..paused("h2")
        },
    );

    let (total, out) = pending(&w, &[a.clone(), b.clone()], &tp, 10).unwrap();
    assert_eq!(total, 1, "h2 already wears the tag");
    assert_eq!(out[0].info_hash, "h1");
    assert_eq!(out[0].waiting.len(), 1);

    let with_webhook = workflow(
        Node::Cond(c),
        vec![
            Action::AddTags {
                tags: vec!["x".into()],
            },
            Action::Webhook {
                url: "https://hook.example/x".into(),
            },
        ],
    );
    let (total, out) = pending(&with_webhook, &[a.clone(), b], &tp, 10).unwrap();
    assert_eq!(total, 1, "a webhook does not keep a torrent in the count");
    assert_eq!(out[0].info_hash, "h1");

    let (total, out) = pending(&w, &[a], &tp, 0).unwrap();
    assert_eq!(
        (total, out.len()),
        (1, 0),
        "counted, not listed, past the sample"
    );
}

/// The distinct leaves of a tree: a condition twice with the same delay is
/// one, with another delay or none it is another, and a different condition is
/// its own.
#[test]
fn a_tree_names_each_distinct_condition_once() {
    let a1 = paused_for("1d");
    let a2 = paused_for("2d");
    let a0 = cond("user_paused", Op::Eq, "true", None, None);
    let b = cond("ratio", Op::Gt, "1", None, None);
    let tree = Node::All {
        of: vec![
            Node::Cond(a1.clone()),
            Node::Any {
                of: vec![
                    Node::Cond(a1.clone()),
                    Node::Cond(a2.clone()),
                    Node::Not {
                        of: Box::new(Node::Cond(b.clone())),
                    },
                ],
            },
            Node::Cond(a0.clone()),
            Node::Cond(b.clone()),
        ],
    };
    let mut out = Vec::new();
    leaves(&tree, &mut out);
    let seen: Vec<(String, Option<String>)> = out
        .iter()
        .map(|c| (c.field.clone(), c.hold.clone()))
        .collect();
    assert_eq!(
        seen,
        [
            ("user_paused".to_string(), Some("1d".to_string())),
            ("user_paused".to_string(), Some("2d".to_string())),
            ("ratio".to_string(), None),
            ("user_paused".to_string(), None),
        ]
    );
}

// --------------------------------------------------------- one torrent's view

/// A torrent wearing `keep` for `age_days`, not paused.
fn aged(age_days: f64) -> Facts {
    Facts {
        tag_ages: vec![("keep".into(), age_days * DAY as f64)],
        user_paused: false,
        ..paused("h1")
    }
}

fn aged_hold() -> Cond {
    cond("tag_age", Op::Gt, "1d", Some("2d"), Some("keep"))
}

fn view_of(w: &Workflow, f: &Facts, rows: SinceRows) -> CondView {
    let t = timeline_of(w, f, rows, NOW).unwrap().expect("a timeline");
    assert_eq!(t.conditions.len(), 1, "{:?}", t.conditions);
    t.conditions.into_iter().next().unwrap()
}

/// ⭐ A tag's "older than" with a `for` holds when the tag is old enough AND
/// the `for` has run from the date the pass recorded: not before, and exactly
/// at the date; and never for a tag that is not old enough, whatever the
/// date.
#[test]
fn an_aged_hold_holds_when_the_tag_is_old_enough_and_the_for_has_run() {
    let c = aged_hold();
    let w = workflow(Node::Cond(c.clone()), tag_x());

    // Old enough (5 days against 1), the for recorded 3 days ago: due a day ago.
    let v = view_of(&w, &aged(5.0), rows_of(&c, "h1", NOW - 3 * DAY));
    assert!(v.holds && v.counting, "{v:?}");
    assert_eq!(v.since, Some(Some(NOW - 5 * DAY)));
    assert_eq!(v.due, Some(NOW - DAY));
    assert_eq!(
        v.for_secs,
        Some(4 * DAY),
        "from the tag's date to the due date"
    );

    // Recorded exactly `for` ago: due this very second, and that is held.
    let v = view_of(&w, &aged(5.0), rows_of(&c, "h1", NOW - 2 * DAY));
    assert_eq!(v.due, Some(NOW));
    assert!(v.holds, "due now is held");

    // One second short.
    let v = view_of(&w, &aged(5.0), rows_of(&c, "h1", NOW - 2 * DAY + 1));
    assert!(!v.holds, "one second to go");

    // Never recorded: the for starts now.
    let v = view_of(&w, &aged(5.0), SinceRows::new());
    assert!(!v.holds && v.counting);
    assert_eq!(v.due, Some(NOW + 2 * DAY));

    // Too young (half a day against 1): the date has passed, the tag has not.
    let v = view_of(&w, &aged(0.5), rows_of(&c, "h1", NOW - 3 * DAY));
    assert!(
        !v.holds,
        "the tag is not old enough, whatever the record says"
    );
    assert!(v.counting);
}

/// A delay belongs to scheduled workflows only: an event one has none, and one
/// without a delay says nothing about any torrent.
#[test]
fn an_event_workflow_or_one_without_a_delay_has_no_timeline() {
    let f = paused("h1");
    let mut event = workflow(Node::Cond(paused_for("1d")), tag_x());
    event.trigger = Trigger::Completed;
    assert!(timeline_of(&event, &f, SinceRows::new(), NOW)
        .unwrap()
        .is_none());
    let mut added = event.clone();
    added.trigger = Trigger::Added;
    assert!(timeline_of(&added, &f, SinceRows::new(), NOW)
        .unwrap()
        .is_none());

    let plain = workflow(Node::Cond(cond("ratio", Op::Gt, "1", None, None)), tag_x());
    assert!(timeline_of(&plain, &f, SinceRows::new(), NOW)
        .unwrap()
        .is_none());

    let scheduled = workflow(Node::Cond(paused_for("1d")), tag_x());
    assert!(timeline_of(&scheduled, &f, SinceRows::new(), NOW)
        .unwrap()
        .is_some());
}

/// A tag named on a field that is not `tag_age` is nobody's wait: the
/// timeline counts only what waits, and the stray tag is not one.
#[test]
fn a_stray_tag_on_another_field_is_not_a_wait() {
    let stray = cond("ratio", Op::Gt, "1", None, Some("keep"));
    let w = workflow(
        Node::All {
            of: vec![Node::Cond(stray), Node::Cond(paused_for("1d"))],
        },
        tag_x(),
    );
    let f = Facts {
        tag_ages: vec![("keep".into(), 3.0 * DAY as f64)],
        ratio: 2.0,
        ..paused("h1")
    };
    let t = timeline_of(&w, &f, SinceRows::new(), NOW).unwrap().unwrap();
    let by_field = |name: &str| t.conditions.iter().find(|v| v.field == name).unwrap();
    assert!(!by_field("ratio").counting, "{:?}", by_field("ratio"));
    assert_eq!(by_field("ratio").for_secs, None);
    assert_eq!(by_field("ratio").due, None);
    assert!(by_field("user_paused").counting);
}
