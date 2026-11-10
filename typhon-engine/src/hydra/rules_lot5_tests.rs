//! What the mutation pass over the parts of `rules.rs` no earlier lot read
//! found nobody was checking: sizes in every unit, the words an error says,
//! what a workflow written without a delay or a cap gets, which actions and
//! conditions ask a pass for more than the torrent's own facts.

use super::*;

fn cond(field: &str, op: Op, value: &str, tag: Option<&str>) -> Node {
    Node::Cond(Cond {
        field: field.into(),
        op,
        value: value.into(),
        hold: None,
        tag: tag.map(str::to_string),
    })
}

/// ⭐ A size is its number times its unit, and each unit is its own number:
/// the decimal and the binary spelling of a unit differ by 2.4 % at a
/// kilobyte and 10 % at a terabyte, and a rule is written against one of them.
#[test]
fn a_size_is_its_number_times_its_own_unit() {
    for (text, bytes) in [
        ("7b", 7.0),
        ("3kb", 3_000.0),
        ("3kib", 3_072.0),
        ("2mb", 2_000_000.0),
        ("2mib", 2_097_152.0),
        ("5gb", 5_000_000_000.0),
        ("5gib", 5_368_709_120.0),
        ("4tb", 4_000_000_000_000.0),
        ("4tib", 4_398_046_511_104.0),
    ] {
        assert_eq!(parse_size(text), Some(bytes), "{text}");
        assert_eq!(
            parse_size(&text.to_uppercase()),
            Some(bytes),
            "{text}, any case"
        );
    }
    assert_eq!(
        parse_size("1tib"),
        Some(1_099_511_627_776.0),
        "2^40 exactly"
    );
    assert_eq!(parse_size("1gib"), Some(1_073_741_824.0), "2^30 exactly");
    assert_eq!(parse_size("1mib"), Some(1_048_576.0), "2^20 exactly");
    assert_eq!(
        parse_size(" 1.5 GB "),
        Some(1_500_000_000.0),
        "spaces, a fraction"
    );
    assert_eq!(parse_size("12"), Some(12.0), "no unit: bytes");
    assert_eq!(parse_size("0.5kib"), Some(512.0));
}

/// What is not a size is not one: no number, no digits before the unit.
#[test]
fn what_is_not_a_size_is_none() {
    for text in ["", "   ", "gb", "kib", "abc", "1.2.3gb", "12 parsecs", "b"] {
        assert_eq!(parse_size(text), None, "{text:?}");
    }
}

/// A workflow saved without a delay or a cap runs every fifteen minutes and
/// touches at most 500 torrents, as a scheduled one: the numbers are the
/// documented constants, not whatever a default happens to be.
#[test]
fn a_workflow_without_an_interval_or_a_cap_gets_the_documented_ones() {
    let w: Workflow = serde_json::from_value(serde_json::json!({
        "name": "bare",
        "when": {"kind": "cond", "field": "ratio", "op": "gt", "value": "1"},
        "then": [{"type": "pause"}],
    }))
    .unwrap();
    assert_eq!(w.interval_secs, 900);
    assert_eq!(w.cap, 500);
    assert_eq!(w.trigger, Trigger::Schedule);
    assert!(!w.enabled, "a new workflow is off");
    assert_eq!((DEFAULT_INTERVAL_SECS, DEFAULT_CAP), (900, 500));
    assert!(MIN_INTERVAL_SECS < DEFAULT_INTERVAL_SECS);
}

/// Only adding trackers asks a pass to copy each torrent's tracker list.
#[test]
fn only_adding_trackers_needs_the_tracker_list() {
    let all = [
        (Action::Pause, false),
        (Action::Resume, false),
        (Action::SetCategory { to: "x".into() }, false),
        (
            Action::AddTags {
                tags: vec!["t".into()],
            },
            false,
        ),
        (
            Action::RemoveTags {
                tags: vec!["t".into()],
            },
            false,
        ),
        (Action::Delete { with_files: true }, false),
        (
            Action::Webhook {
                url: "https://h.example/x".into(),
            },
            false,
        ),
        (
            Action::SetLocation {
                to: "/data".into(),
                allow_breaking_hardlinks: false,
            },
            false,
        ),
        (
            Action::AddTrackers {
                urls: vec![],
                list_url: String::new(),
            },
            true,
        ),
    ];
    for (action, expected) in all {
        assert_eq!(action.needs_trackers(), expected, "{action:?}");
    }
}

/// The hardlink scan is one `stat` per file of the catalogue: only a tree
/// that reads one of its four facts asks for it, however deep.
#[test]
fn only_a_tree_that_reads_a_link_fact_needs_the_link_scan() {
    assert!(!needs_link_scan(&cond("ratio", Op::Gt, "1", None)));
    assert!(!needs_link_scan(&Node::All {
        of: vec![
            cond("ratio", Op::Gt, "1", None),
            Node::Not {
                of: Box::new(cond("category", Op::Eq, "x", None))
            },
        ]
    }));
    for field in LINK_FIELDS {
        assert!(needs_link_scan(&cond(field, Op::Gt, "0", None)), "{field}");
        assert!(
            needs_link_scan(&Node::All {
                of: vec![
                    cond("ratio", Op::Gt, "1", None),
                    Node::Any {
                        of: vec![Node::Not {
                            of: Box::new(cond(field, Op::Gt, "0", None))
                        }]
                    },
                ]
            }),
            "{field}, nested"
        );
    }
    assert_eq!(
        LINK_FIELDS,
        [
            "link_count",
            "external_links",
            "freeable_bytes",
            "data_missing"
        ]
    );
}

/// The tags a tree's `tag_age` conditions name: each once, trimmed, in the
/// order met, and neither another field's tag argument nor an empty one.
#[test]
fn the_tags_a_tree_ages_are_distinct_trimmed_and_only_from_tag_age() {
    let tree = Node::All {
        of: vec![
            cond("tag_age", Op::Gt, "1d", Some("keep")),
            Node::Not {
                of: Box::new(cond("tag_age", Op::Lt, "2d", Some(" seed "))),
            },
            Node::Any {
                of: vec![
                    cond("tag_age", Op::Gt, "3d", Some("keep")),
                    cond("tag_age", Op::Gt, "3d", Some("   ")),
                    cond("tag_age", Op::Gt, "3d", None),
                    cond("seeding_time", Op::Gt, "3d", Some("not-a-tag-age")),
                    cond("tag_age", Op::Gt, "4d", Some("seed")),
                    cond("tag_age", Op::Gt, "4d", Some("other")),
                ],
            },
        ],
    };
    assert_eq!(tag_ages_used(&tree), ["keep", "seed", "other"]);
    assert!(tag_ages_used(&cond("ratio", Op::Gt, "1", None)).is_empty());
}

/// Every field's kind has its own word in the API, and a field nobody knows
/// is "unknown".
#[test]
fn each_kind_of_field_has_its_own_word() {
    assert_eq!(kind_name("name"), "text");
    assert_eq!(kind_name("ratio"), "number");
    assert_eq!(kind_name("seeding_time"), "duration");
    assert_eq!(kind_name("free_space"), "size");
    assert_eq!(kind_name("progress"), "percent");
    assert_eq!(kind_name("data_missing"), "bool");
    assert_eq!(kind_name("tags"), "tags");
    assert_eq!(kind_name("nonsense"), "unknown");
}

/// ⭐ What a refusal says is what the operator reads in the editor: every
/// reason in its own words, and only a value that is not a duration gets the
/// reminder of what a duration is.
#[test]
fn each_refusal_says_its_own_reason() {
    let bad = |field: &str, value: &str| CompileError::BadValue {
        field: field.into(),
        value: value.into(),
    };
    let reminder = ": a duration is a number of seconds, or a number followed by s, m, h or d, and is never negative";
    assert_eq!(
        bad("ratio", "x").to_string(),
        "\"x\" is not a valid value for \"ratio\""
    );
    for field in [
        "for",
        "tag_age",
        "seeding_time",
        "added_age",
        "completed_age",
    ] {
        assert_eq!(
            bad(field, "soon").to_string(),
            format!("\"soon\" is not a valid value for {field:?}{reminder}"),
            "{field}"
        );
    }
    assert_eq!(
        bad("free_space", "lots").to_string(),
        "\"lots\" is not a valid value for \"free_space\"",
        "a size is not a duration"
    );
    for (error, words) in [
        (CompileError::UnknownField("zzz".into()), "unknown field \"zzz\""),
        (CompileError::BadRegex("oops".into()), "invalid regex: oops"),
        (
            CompileError::Empty,
            "a workflow with no condition would match everything",
        ),
        (
            CompileError::DeleteNotAlone,
            "delete cannot be combined with another action",
        ),
        (
            CompileError::NoActions,
            "a workflow with no action would do nothing",
        ),
        (
            CompileError::BadWebhookUrl("ftp://x".into()),
            "\"ftp://x\" is not an http:// or https:// URL",
        ),
        (
            CompileError::BadLocation("rel/x".into()),
            "\"rel/x\" is not an absolute folder (no `..`)",
        ),
        (
            CompileError::BadTracker("x".into()),
            "\"x\" is not an http://, https:// or udp:// announce URL",
        ),
        (
            CompileError::NoTrackers,
            "add trackers needs at least one tracker or the URL of a list",
        ),
        (
            CompileError::LinkFieldOnEvent("link_count".into()),
            "\"link_count\" cannot be used when a download completes: the hardlink index has not measured the torrent yet",
        ),
    ] {
        assert_eq!(error.to_string(), words);
    }
    assert!(CompileError::WebhookAlone
        .to_string()
        .starts_with("on a timer, a webhook needs another action"));
    assert!(CompileError::TagAgeOnEvent
        .to_string()
        .starts_with("\"tag age\" needs a schedule"));
    assert!(CompileError::ForOnEvent
        .to_string()
        .starts_with("\"for\" needs a schedule"));
}
