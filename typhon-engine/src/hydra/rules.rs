//! Workflows: conditions in, actions out.
//!
//! An automation people otherwise write as a cron script -- "file it under
//! `done` when it finishes", "seed for two days then stop". The shape is the
//! one qui settled on and it is the right one: a tree of AND/OR groups rather
//! than a flat list, because the flat list forces you to write the same rule
//! three times the first moment you want "tracker A or tracker B".
//!
//! This module imports NOTHING from the rest of Hydra. That is structural, not
//! stylistic: a condition that needed a live engine to evaluate could not be
//! unit tested, so the type system is what stops one being added. Everything
//! arrives as `Facts`, a flat struct someone else fills in.
//!
//! Conditions compile ONCE into closures. A pass looks at three hundred
//! thousand torrents; parsing "500GiB" or building a regex per torrent per rule
//! is the difference between a background task and a stall.

use serde::{Deserialize, Serialize};

/// Everything a condition may look at, flattened.
///
/// Sourced from the row builder and the store, so a field here exists because
/// Hydra already knows it -- not because a comparable product has it.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub info_hash: String,
    pub name: String,
    pub category: String,
    pub tags: Vec<String>,
    pub state: String,
    pub engine: String,
    pub save_path: String,
    pub tracker_host: String,
    pub tracker_error: bool,
    pub tracker_error_msg: String,
    pub torrent_error: bool,
    pub user_paused: bool,
    pub multi_file: bool,

    pub progress: f64,
    pub ratio: f64,
    pub total_size: f64,
    pub total_uploaded: f64,
    pub total_downloaded: f64,
    pub upload_rate: f64,
    pub download_rate: f64,
    pub num_peers: f64,
    pub num_seeds: f64,
    pub swarm_seeds: f64,
    pub swarm_leechers: f64,

    /// Seconds. See `NEVER` for the ones that may not have happened.
    pub seeding_time: f64,
    pub added_age: f64,
    pub completed_age: f64,

    /// Bytes free on the filesystem holding this torrent's data.
    pub free_space: f64,
    /// Highest link count across the torrent's files: >1 means hardlinked.
    ///
    /// ⚠️ On its own this number decides nothing. It counts an inode's names
    /// without saying whose they are, so two cross-seeds of each other report
    /// the same `2` as a file the media library is using. `external_links` is
    /// the one that separates them.
    pub link_count: f64,
    /// Names held by someone OUTSIDE this catalogue. Zero means every name is
    /// ours, so removing our torrents orphans the bytes and nothing else
    /// notices. `NEVER` until a scan has actually measured it.
    pub external_links: f64,
    /// Bytes that removing this torrent would really give back: only the files
    /// no other name shares. `NEVER` until measured.
    pub freeable_bytes: f64,
    /// Not one of the torrent's files could be read. A seeding torrent in this
    /// state announces data it cannot serve.
    pub data_missing: bool,
}

/// The value of a duration that has not happened yet.
///
/// ⚠️ This is NaN on purpose and the choice is not cosmetic. A torrent that
/// never completed has no completion age, and EVERY finite sentinel is wrong:
/// -1, 0 and the epoch all satisfy `completed_age < 1d`, so a rule meaning
/// "finished in the last day" would match the entire never-finished catalogue.
/// NaN compares false to every ordering operator, which is exactly the wanted
/// answer. `!=` is handled explicitly below, because NaN != x is true and that
/// would leak the same bug back through the other door.
pub const NEVER: f64 = f64::NAN;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    Matches,
    HasTag,
    NotHasTag,
}

/// One comparison. `field` is checked against the known set when compiled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cond {
    pub field: String,
    pub op: Op,
    #[serde(default)]
    pub value: String,
}

/// A condition tree.
///
/// `All` and `Any` nest, so "(tracker is A or tracker is B) and ratio >= 2"
/// is expressible without repeating the ratio.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    All { of: Vec<Node> },
    Any { of: Vec<Node> },
    Not { of: Box<Node> },
    Cond(Cond),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Pause,
    Resume,
    /// Changing a category MOVES DATA in Hydra. Carried out as a job, never
    /// inline in a pass: a rule matching a thousand torrents would otherwise
    /// hold the pass open for terabytes of copying.
    SetCategory { to: String },
    AddTags { tags: Vec<String> },
    RemoveTags { tags: Vec<String> },
    /// The one that cannot be undone. Kept apart everywhere: it may not share a
    /// workflow with another action, and it ends processing for that torrent.
    Delete {
        #[serde(default)]
        with_files: bool,
    },
}

impl Action {
    pub fn is_delete(&self) -> bool {
        matches!(self, Action::Delete { .. })
    }
}

/// How many torrents one pass of one workflow may touch.
pub const DEFAULT_CAP: usize = 500;
/// The floor qui uses too. A rule that runs every second is a rule that has
/// stopped being an automation and started being a load generator.
pub const MIN_INTERVAL_SECS: i64 = 60;
pub const DEFAULT_INTERVAL_SECS: i64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    #[serde(default)]
    pub id: String,
    pub name: String,
    /// A new workflow is off. It is the only default that cannot cause damage
    /// while its author is still typing.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub position: i64,
    #[serde(default = "default_interval")]
    pub interval_secs: i64,
    pub when: Node,
    pub then: Vec<Action>,
    #[serde(default = "default_cap")]
    pub cap: usize,
}

fn default_interval() -> i64 {
    DEFAULT_INTERVAL_SECS
}
fn default_cap() -> usize {
    DEFAULT_CAP
}

/// A compiled condition: no parsing, no allocation, one call per torrent.
pub type Matcher = Box<dyn Fn(&Facts) -> bool + Send + Sync>;

#[derive(Debug, PartialEq)]
pub enum CompileError {
    UnknownField(String),
    BadValue { field: String, value: String },
    BadRegex(String),
    Empty,
    /// Delete refuses company. Combining it with a tag action would raise the
    /// question of whether the tag was written before the files went away.
    DeleteNotAlone,
    NoActions,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::UnknownField(x) => write!(f, "unknown field {x:?}"),
            CompileError::BadValue { field, value } => {
                write!(f, "{value:?} is not a valid value for {field:?}")
            }
            CompileError::BadRegex(e) => write!(f, "invalid regex: {e}"),
            CompileError::Empty => write!(f, "a workflow with no condition would match everything"),
            CompileError::DeleteNotAlone => {
                write!(f, "delete cannot be combined with another action")
            }
            CompileError::NoActions => write!(f, "a workflow with no action would do nothing"),
        }
    }
}

/// What kind of value a field holds, which decides the operators it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    /// Seconds, entered as `2d`, `36h`, `90m` or a bare count.
    Duration,
    /// Bytes, entered as `500GB` or `500GiB` -- which differ, and the
    /// difference is 7% at that size, so both spellings are honoured exactly.
    Size,
    Bool,
    /// A set, compared with has_tag rather than equals.
    Tags,
    /// 0-100, entered as a percentage.
    Percent,
}

/// The states a torrent row can report, for the editor's dropdown.
///
/// Listed rather than derived: they come out of `derive_state_static`, which
/// is a match on strings the engine writes, so there is nothing to enumerate
/// at runtime. Kept beside FIELDS so the two are read together.
pub const STATES: &[&str] = &[
    "seeding",
    "downloading",
    "stopped",
    "queued",
    "checking_files",
    "error",
];

/// Every field a condition may name, with the kind that decides its operators.
///
/// The single source of truth: `/api/workflows/fields` serves this, the
/// compiler validates against it, and the UI builds its dropdowns from it. A
/// field added here appears in all three at once, which is the only way they
/// stay in agreement.
pub const FIELDS: &[(&str, Kind)] = &[
    ("name", Kind::Text),
    ("info_hash", Kind::Text),
    ("category", Kind::Text),
    ("tags", Kind::Tags),
    ("state", Kind::Text),
    ("engine", Kind::Text),
    ("save_path", Kind::Text),
    ("tracker_host", Kind::Text),
    ("tracker_error", Kind::Bool),
    ("tracker_error_msg", Kind::Text),
    ("torrent_error", Kind::Bool),
    ("user_paused", Kind::Bool),
    ("multi_file", Kind::Bool),
    ("progress", Kind::Percent),
    ("ratio", Kind::Number),
    ("total_size", Kind::Size),
    ("total_uploaded", Kind::Size),
    ("total_downloaded", Kind::Size),
    ("upload_rate", Kind::Size),
    ("download_rate", Kind::Size),
    ("num_peers", Kind::Number),
    ("num_seeds", Kind::Number),
    ("swarm_seeds", Kind::Number),
    ("swarm_leechers", Kind::Number),
    // ⚠️ `seeding_time` is deliberately NOT here. The column exists in the
    // store and 4.x never writes it -- the 3.x seedtime counter was not
    // ported -- so it reads zero for every torrent, always. Offering it would
    // hand the operator a condition that silently never fires, which is worse
    // than not having it: `seeding_time >= 2d` looks right and does nothing.
    // Until something writes it, "seed for two days then stop" is
    // `completed_age >= 2d`, which is a real measurement.
    ("added_age", Kind::Duration),
    ("completed_age", Kind::Duration),
    ("free_space", Kind::Size),
    ("link_count", Kind::Number),
    ("external_links", Kind::Number),
    ("freeable_bytes", Kind::Size),
    ("data_missing", Kind::Bool),
];

/// Does this condition tree ask anything that needs the link scan?
///
/// The scan is one `stat` per file in the catalogue. Worth it when a rule uses
/// it, pure waste every fifteen minutes when none does -- and no workflow uses
/// it by default, so the common case must stay free.
pub fn needs_link_scan(n: &Node) -> bool {
    match n {
        Node::All { of } | Node::Any { of } => of.iter().any(needs_link_scan),
        Node::Not { of } => needs_link_scan(of),
        Node::Cond(c) => matches!(
            c.field.as_str(),
            "link_count" | "external_links" | "freeable_bytes" | "data_missing"
        ),
    }
}

pub fn kind_of(field: &str) -> Option<Kind> {
    FIELDS.iter().find(|(n, _)| *n == field).map(|(_, k)| *k)
}

/// `2d`, `36h`, `90m`, `45s`, or a bare number of seconds.
pub fn parse_duration(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let (digits, mult) = match t.chars().last()? {
        'd' | 'D' => (&t[..t.len() - 1], 86400.0),
        'h' | 'H' => (&t[..t.len() - 1], 3600.0),
        'm' | 'M' => (&t[..t.len() - 1], 60.0),
        's' | 'S' => (&t[..t.len() - 1], 1.0),
        _ => (t, 1.0),
    };
    digits.trim().parse::<f64>().ok().map(|n| n * mult)
}

/// `500GB` (decimal) and `500GiB` (binary) are different numbers.
///
/// Guessing which one somebody meant is how a "free space below 100GB" rule
/// fires 7% early or late forever without anybody noticing. Both are accepted
/// and neither is reinterpreted.
pub fn parse_size(text: &str) -> Option<f64> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    const UNITS: &[(&str, f64)] = &[
        ("kib", 1024.0),
        ("mib", 1024.0 * 1024.0),
        ("gib", 1024.0 * 1024.0 * 1024.0),
        ("tib", 1024.0 * 1024.0 * 1024.0 * 1024.0),
        ("kb", 1000.0),
        ("mb", 1e6),
        ("gb", 1e9),
        ("tb", 1e12),
        ("b", 1.0),
    ];
    for (suffix, mult) in UNITS {
        if let Some(head) = lower.strip_suffix(suffix) {
            return head.trim().parse::<f64>().ok().map(|n| n * mult);
        }
    }
    lower.parse::<f64>().ok()
}

fn number_for(field: &str, kind: Kind, value: &str) -> Option<f64> {
    match kind {
        Kind::Duration => parse_duration(value),
        Kind::Size => parse_size(value),
        Kind::Percent | Kind::Number => value.trim().parse::<f64>().ok(),
        _ => {
            let _ = field;
            None
        }
    }
}

fn text_of<'a>(f: &'a Facts, field: &str) -> Option<&'a str> {
    Some(match field {
        "name" => &f.name,
        "info_hash" => &f.info_hash,
        "category" => &f.category,
        "state" => &f.state,
        "engine" => &f.engine,
        "save_path" => &f.save_path,
        "tracker_host" => &f.tracker_host,
        "tracker_error_msg" => &f.tracker_error_msg,
        _ => return None,
    })
}

fn number_of(f: &Facts, field: &str) -> Option<f64> {
    Some(match field {
        "progress" => f.progress,
        "ratio" => f.ratio,
        "total_size" => f.total_size,
        "total_uploaded" => f.total_uploaded,
        "total_downloaded" => f.total_downloaded,
        "upload_rate" => f.upload_rate,
        "download_rate" => f.download_rate,
        "num_peers" => f.num_peers,
        "num_seeds" => f.num_seeds,
        "swarm_seeds" => f.swarm_seeds,
        "swarm_leechers" => f.swarm_leechers,
        "seeding_time" => f.seeding_time,
        "added_age" => f.added_age,
        "completed_age" => f.completed_age,
        "free_space" => f.free_space,
        "link_count" => f.link_count,
        "external_links" => f.external_links,
        "freeable_bytes" => f.freeable_bytes,
        _ => return None,
    })
}

fn bool_of(f: &Facts, field: &str) -> Option<bool> {
    Some(match field {
        "tracker_error" => f.tracker_error,
        "torrent_error" => f.torrent_error,
        "user_paused" => f.user_paused,
        "multi_file" => f.multi_file,
        _ => return None,
    })
}

/// Compare two numbers, with NaN meaning "this never happened".
///
/// Every ordering answers false against NaN, which is what makes a
/// never-completed torrent fail `completed_age < 1d` instead of matching it.
/// `Ne` is spelled out: leaving it to `!=` would answer true and hand the bug
/// straight back.
fn cmp_num(op: Op, left: f64, right: f64) -> bool {
    if left.is_nan() {
        return false;
    }
    match op {
        Op::Eq => left == right,
        Op::Ne => left != right,
        Op::Gt => left > right,
        Op::Ge => left >= right,
        Op::Lt => left < right,
        Op::Le => left <= right,
        _ => false,
    }
}

fn compile_cond(c: &Cond) -> Result<Matcher, CompileError> {
    let kind = kind_of(&c.field).ok_or_else(|| CompileError::UnknownField(c.field.clone()))?;
    let field = c.field.clone();
    let op = c.op;

    match kind {
        Kind::Number | Kind::Duration | Kind::Size | Kind::Percent => {
            let want = number_for(&field, kind, &c.value).ok_or_else(|| CompileError::BadValue {
                field: field.clone(),
                value: c.value.clone(),
            })?;
            Ok(Box::new(move |f: &Facts| {
                number_of(f, &field).is_some_and(|got| cmp_num(op, got, want))
            }))
        }
        Kind::Bool => {
            let want = matches!(
                c.value.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "1"
            );
            Ok(Box::new(move |f: &Facts| {
                bool_of(f, &field).is_some_and(|got| match op {
                    Op::Ne => got != want,
                    _ => got == want,
                })
            }))
        }
        Kind::Tags => {
            let want = c.value.trim().to_string();
            Ok(Box::new(move |f: &Facts| {
                let present = f.tags.iter().any(|t| t == &want);
                match op {
                    Op::NotHasTag | Op::Ne | Op::NotContains => !present,
                    _ => present,
                }
            }))
        }
        Kind::Text => {
            if op == Op::Matches {
                let re = regex::Regex::new(&c.value)
                    .map_err(|e| CompileError::BadRegex(e.to_string()))?;
                return Ok(Box::new(move |f: &Facts| {
                    text_of(f, &field).is_some_and(|got| re.is_match(got))
                }));
            }
            // Case-insensitive, like qui: a category typed `Done` and stored
            // `done` is the same category to everyone except a comparison.
            let want = c.value.to_lowercase();
            Ok(Box::new(move |f: &Facts| {
                let Some(got) = text_of(f, &field) else {
                    return false;
                };
                let got = got.to_lowercase();
                match op {
                    Op::Eq => got == want,
                    Op::Ne => got != want,
                    Op::Contains => got.contains(&want),
                    Op::NotContains => !got.contains(&want),
                    Op::StartsWith => got.starts_with(&want),
                    Op::EndsWith => got.ends_with(&want),
                    _ => false,
                }
            }))
        }
    }
}

/// Compile a condition tree into one closure.
pub fn compile(node: &Node) -> Result<Matcher, CompileError> {
    match node {
        Node::Cond(c) => compile_cond(c),
        Node::Not { of } => {
            let inner = compile(of)?;
            Ok(Box::new(move |f: &Facts| !inner(f)))
        }
        Node::All { of } => {
            if of.is_empty() {
                return Err(CompileError::Empty);
            }
            let parts: Vec<Matcher> = of.iter().map(compile).collect::<Result<_, _>>()?;
            Ok(Box::new(move |f: &Facts| parts.iter().all(|p| p(f))))
        }
        Node::Any { of } => {
            if of.is_empty() {
                return Err(CompileError::Empty);
            }
            let parts: Vec<Matcher> = of.iter().map(compile).collect::<Result<_, _>>()?;
            Ok(Box::new(move |f: &Facts| parts.iter().any(|p| p(f))))
        }
    }
}

/// Compile a whole workflow, refusing the shapes that are always mistakes.
pub fn compile_workflow(w: &Workflow) -> Result<Matcher, CompileError> {
    if w.then.is_empty() {
        return Err(CompileError::NoActions);
    }
    if w.then.iter().any(Action::is_delete) && w.then.len() > 1 {
        return Err(CompileError::DeleteNotAlone);
    }
    compile(&w.when)
}

/// Is this torrent already how the action would leave it?
///
/// The convergence check, and the reason a workflow settles instead of
/// rewriting the same tag every fifteen minutes. Without it `applied` counts
/// passes rather than changes, and the activity log becomes unreadable within
/// a day.
pub fn already_satisfied(action: &Action, f: &Facts) -> bool {
    match action {
        Action::Pause => f.user_paused,
        Action::Resume => !f.user_paused,
        Action::SetCategory { to } => f.category.eq_ignore_ascii_case(to),
        Action::AddTags { tags } => tags.iter().all(|t| f.tags.contains(t)),
        Action::RemoveTags { tags } => tags.iter().all(|t| !f.tags.contains(t)),
        // Deleting is never already done: the torrent is still here.
        Action::Delete { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            name: "Shirley.2024.MULTi.1080p".into(),
            category: "animes".into(),
            tags: vec!["keep".into()],
            tracker_host: "tracker.example.net".into(),
            progress: 100.0,
            ratio: 2.5,
            seeding_time: 3.0 * 86400.0,
            completed_age: 3.0 * 86400.0,
            total_size: 2.0 * 1024.0 * 1024.0 * 1024.0,
            ..Default::default()
        }
    }

    fn m(node: Node, f: &Facts) -> bool {
        compile(&node).expect("compiles")(f)
    }

    fn cond(field: &str, op: Op, value: &str) -> Node {
        Node::Cond(Cond {
            field: field.into(),
            op,
            value: value.into(),
        })
    }

    /// The two examples that motivated the feature, and its whole point.
    #[test]
    fn the_two_canonical_workflows_match() {
        let f = facts();
        // "file it under done once it is finished"
        let finished = Node::All {
            of: vec![
                cond("progress", Op::Ge, "100"),
                cond("category", Op::Eq, "animes"),
            ],
        };
        assert!(m(finished, &f));

        // "seed for two days, then stop" -- expressed on completed_age,
        // because nothing writes seeding_time in 4.x. See FIELDS.
        assert!(m(cond("completed_age", Op::Ge, "2d"), &f));
        assert!(!m(cond("completed_age", Op::Ge, "7d"), &f));
    }

    /// The reason a flat AND list was not enough.
    #[test]
    fn or_groups_nest_inside_and_groups() {
        let f = facts();
        let node = Node::All {
            of: vec![
                Node::Any {
                    of: vec![
                        cond("tracker_host", Op::Eq, "nope.example"),
                        cond("tracker_host", Op::EndsWith, "example.net"),
                    ],
                },
                cond("ratio", Op::Ge, "2"),
            ],
        };
        assert!(m(node, &f));

        // The OR still has to be satisfied by something.
        let node = Node::All {
            of: vec![
                Node::Any {
                    of: vec![
                        cond("tracker_host", Op::Eq, "nope.example"),
                        cond("tracker_host", Op::Eq, "also-nope.example"),
                    ],
                },
                cond("ratio", Op::Ge, "2"),
            ],
        };
        assert!(!m(node, &f));
    }

    /// ⭐ The bug the Go v1 found by breaking a test, kept dead here.
    ///
    /// A torrent that never completed must not satisfy "completed less than a
    /// day ago". Every finite sentinel does; NaN is the only value that does
    /// not, and `!=` has to be spelled out or it comes back.
    #[test]
    fn a_never_completed_torrent_matches_no_completion_rule() {
        let f = Facts {
            completed_age: NEVER,
            ..Default::default()
        };
        for op in [Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Eq, Op::Ne] {
            assert!(
                !m(cond("completed_age", op, "1d"), &f),
                "{op:?} must be false against a completion that never happened"
            );
        }
        // A torrent that DID complete still compares normally.
        let done = Facts {
            completed_age: 3600.0,
            ..Default::default()
        };
        assert!(m(cond("completed_age", Op::Lt, "1d"), &done));
    }

    #[test]
    fn durations_and_sizes_are_read_the_way_they_are_written() {
        assert_eq!(parse_duration("2d"), Some(172800.0));
        assert_eq!(parse_duration("36h"), Some(129600.0));
        assert_eq!(parse_duration("90"), Some(90.0));
        // GB and GiB differ by 7% at this size. Neither is reinterpreted.
        assert_eq!(parse_size("500GB"), Some(5e11));
        assert_eq!(parse_size("500GiB"), Some(536870912000.0));
        assert_ne!(parse_size("500GB"), parse_size("500GiB"));
    }

    /// An empty group matches everything, which as a workflow means "do this
    /// to the entire catalogue". Refused rather than obeyed.
    #[test]
    fn a_workflow_that_would_match_everything_is_refused() {
        assert_eq!(
            compile(&Node::All { of: vec![] }).err(),
            Some(CompileError::Empty)
        );
        assert_eq!(
            compile(&Node::Any { of: vec![] }).err(),
            Some(CompileError::Empty)
        );
    }

    #[test]
    fn unknown_fields_and_bad_values_are_refused_at_compile_time() {
        assert_eq!(
            compile(&cond("nonesuch", Op::Eq, "x")).err(),
            Some(CompileError::UnknownField("nonesuch".into()))
        );
        assert_eq!(
            compile(&cond("completed_age", Op::Ge, "two days")).err(),
            Some(CompileError::BadValue {
                field: "completed_age".into(),
                value: "two days".into()
            })
        );
        assert!(matches!(
            compile(&cond("name", Op::Matches, "([")).err(),
            Some(CompileError::BadRegex(_))
        ));
    }

    /// Delete is kept alone so nobody has to reason about ordering against a
    /// torrent that is about to stop existing.
    #[test]
    fn delete_refuses_company_and_an_empty_action_list_is_refused() {
        let base = Workflow {
            id: String::new(),
            name: "x".into(),
            enabled: false,
            position: 0,
            interval_secs: DEFAULT_INTERVAL_SECS,
            when: cond("ratio", Op::Ge, "2"),
            then: vec![Action::Delete { with_files: true }, Action::Pause],
            cap: DEFAULT_CAP,
        };
        assert_eq!(
            compile_workflow(&base).err(),
            Some(CompileError::DeleteNotAlone)
        );

        let alone = Workflow {
            then: vec![Action::Delete { with_files: true }],
            ..base.clone()
        };
        assert!(compile_workflow(&alone).is_ok());

        let none = Workflow {
            then: vec![],
            ..base
        };
        assert_eq!(compile_workflow(&none).err(), Some(CompileError::NoActions));
    }

    /// Without this a workflow rewrites the same tag every fifteen minutes and
    /// the applied count means nothing.
    #[test]
    fn convergence_stops_a_workflow_reapplying_itself() {
        let f = facts();
        assert!(already_satisfied(&Action::SetCategory { to: "animes".into() }, &f));
        assert!(!already_satisfied(&Action::SetCategory { to: "done".into() }, &f));
        assert!(already_satisfied(&Action::AddTags { tags: vec!["keep".into()] }, &f));
        assert!(!already_satisfied(&Action::AddTags { tags: vec!["new".into()] }, &f));
        assert!(!already_satisfied(&Action::Pause, &f));
        assert!(already_satisfied(&Action::Resume, &f));
        // A delete is never "already done".
        assert!(!already_satisfied(&Action::Delete { with_files: false }, &f));
    }

    #[test]
    fn tags_are_a_set_not_a_string() {
        let f = facts();
        assert!(m(cond("tags", Op::HasTag, "keep"), &f));
        assert!(!m(cond("tags", Op::HasTag, "drop"), &f));
        assert!(m(cond("tags", Op::NotHasTag, "drop"), &f));
    }

    #[test]
    fn text_compares_without_regard_to_case() {
        let f = facts();
        assert!(m(cond("category", Op::Eq, "ANIMES"), &f));
        assert!(m(cond("name", Op::Contains, "shirley"), &f));
        assert!(m(cond("name", Op::Matches, r"\.1080p"), &f));
    }
}
