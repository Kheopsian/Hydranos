//! Automatic download rules, as qBittorrent writes and reads them.
//!
//! The rule document IS qBittorrent's (`rss/rules`, `rss/setRule`): an *arr
//! or a script that already drives a qBittorrent can drive this one, and the
//! UI edits the same object. Both of qBittorrent's spellings of the add
//! options are read -- the top-level `assignedCategory` / `savePath` /
//! `addPaused` of 4.x and the `torrentParams` object of 4.6+/5.x -- and both
//! are written back.
//!
//! The matching follows qBittorrent's `AutoDownloadRule`:
//! - `mustContain` without regex: `|` separates alternatives (any one is
//!   enough), and inside one alternative every space-separated word must be
//!   in the title, in any order. `*` and `?` are wildcards; case is ignored.
//!   An empty alternative (`foo|`) matches everything, as in qBittorrent.
//! - `mustNotContain`: the same syntax; any alternative that matches rejects
//!   the article. Empty alternatives are ignored here (qBittorrent would
//!   reject everything on `foo|`, which nobody means).
//! - `useRegex`: each field is one case-insensitive regular expression.
//! - No `-word` exclusion: qBittorrent has none; `mustNotContain` is the way.
//! - `episodeFilter`: qBittorrent's `1x2;8-15;30-;` (season 1, episodes 2,
//!   8 to 15 and 30 onwards, later seasons included by the open range), and
//!   also `S01E01-S01E10;S02E05;S03E01-;`, ranges that may cross seasons.
//!   Titles are read as `S01E05` / `s1.e5` / `1x05`; a title with no episode
//!   number never matches a rule that has a filter (a season pack included).
//! - `smartFilter`: an episode taken once (`previouslyMatchedEpisodes`) is not
//!   taken again, unless the new title says REPACK or PROPER.
//! - `ignoreDays`: an article dated less than that many days after the rule's
//!   last match is not taken.
//!
//! Three keys are Hydranos's own, written only when set and ignored by a
//! qBittorrent client; a rule without them behaves as above:
//! - `conditions`: a nested AND/OR tree over the article's fields (title,
//!   category, author, description, link, size, age). A group is
//!   `{"match": "all"|"any", "rows": [...]}`, a row is
//!   `{"field", "op", "value", "value2"}`. Text ops are `contains` /
//!   `not_contains` (the `mustContain` syntax), `is` / `is_not` (whole text,
//!   case ignored) and `regex` / `not_regex`; `size` and `age` (hours) take
//!   `gt`, `lt` and `between` (inclusive). Sizes read `700 MB`, `1.5 GiB` or
//!   bytes; KB/MB/GB and KiB/MiB/GiB are all 1024-based, as the client shows
//!   sizes. At most 8 levels and 100 nodes; an empty group is dropped.
//! - `minSize` / `maxSize`: bytes, 0 = no limit, inclusive; the same as a
//!   `size between` row ANDed onto the tree.
//!
//! A feed that does not give a size leaves a size test *unknown*, not failed:
//! the logic is three-valued (AND/OR as in SQL), an unknown result is let
//! through, and a `.torrent` is judged again on its real size once fetched
//! (refused, and not fetched again, when it is out of range). A magnet has
//! no size before it is added, so it is taken.

use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};

/// One rule, in the fields this client acts on. The store keeps `to_json` of
/// it: fields qBittorrent has and Hydranos does not act on are not kept.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rule {
    pub name: String,
    pub enabled: bool,
    pub priority: i64,
    pub use_regex: bool,
    pub must_contain: String,
    pub must_not_contain: String,
    pub episode_filter: String,
    /// Feed URLs, as qBittorrent names them.
    pub affected_feeds: Vec<String>,
    pub ignore_days: i64,
    /// Unix seconds, 0 = never.
    pub last_match: i64,
    pub smart_filter: bool,
    pub previously_matched: Vec<String>,
    pub category: String,
    pub save_path: String,
    pub tags: Vec<String>,
    /// `None`: the client's default (not paused).
    pub add_paused: Option<bool>,
    /// Hydranos: the condition tree, `None` = no condition.
    pub conditions: Option<Cond>,
    /// Hydranos: bytes, 0 = no limit.
    pub min_size: i64,
    pub max_size: i64,
}

/// Deepest nesting and most nodes a condition tree may have: a rule is read
/// at every pass, and the editor draws all of it.
const MAX_DEPTH: usize = 8;
const MAX_NODES: usize = 100;

/// What a row looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Category,
    Author,
    Description,
    Link,
    /// Bytes.
    Size,
    /// Hours since the article's date.
    Age,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Field::Title => "title",
            Field::Category => "category",
            Field::Author => "author",
            Field::Description => "description",
            Field::Link => "link",
            Field::Size => "size",
            Field::Age => "age",
        }
    }
    fn parse(s: &str) -> Option<Field> {
        Some(match s {
            "title" => Field::Title,
            "category" => Field::Category,
            "author" => Field::Author,
            "description" => Field::Description,
            "link" => Field::Link,
            "size" => Field::Size,
            "age" => Field::Age,
            _ => return None,
        })
    }
    fn numeric(self) -> bool {
        matches!(self, Field::Size | Field::Age)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Contains,
    NotContains,
    Is,
    IsNot,
    Regex,
    NotRegex,
    Gt,
    Lt,
    Between,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Contains => "contains",
            Op::NotContains => "not_contains",
            Op::Is => "is",
            Op::IsNot => "is_not",
            Op::Regex => "regex",
            Op::NotRegex => "not_regex",
            Op::Gt => "gt",
            Op::Lt => "lt",
            Op::Between => "between",
        }
    }
    fn parse(s: &str) -> Option<Op> {
        Some(match s {
            "contains" => Op::Contains,
            "not_contains" => Op::NotContains,
            "is" => Op::Is,
            "is_not" => Op::IsNot,
            "regex" => Op::Regex,
            "not_regex" => Op::NotRegex,
            "gt" => Op::Gt,
            "lt" => Op::Lt,
            "between" => Op::Between,
            _ => return None,
        })
    }
    fn numeric(self) -> bool {
        matches!(self, Op::Gt | Op::Lt | Op::Between)
    }
}

/// One row of the tree.
#[derive(Debug, Clone, PartialEq)]
pub struct Test {
    pub field: Field,
    pub op: Op,
    /// As written (`1.5 GB`), so the editor gets back what it sent.
    pub value: String,
    /// Only `between` reads it.
    pub value2: String,
    /// `value` / `value2` in bytes or seconds, read once at load.
    n1: i64,
    n2: i64,
}

/// A node of the condition tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// `any`: one row is enough; otherwise all of them. Never empty.
    Group {
        any: bool,
        rows: Vec<Cond>,
    },
    Test(Test),
}

/// What a condition says of an article. `Unknown` is a size the feed did
/// not give: neither a pass nor a failure until the torrent is fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    Yes,
    No,
    Unknown,
}

impl Tri {
    fn and(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::No, _) | (_, Tri::No) => Tri::No,
            (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
            _ => Tri::Yes,
        }
    }
    fn or(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::Yes, _) | (_, Tri::Yes) => Tri::Yes,
            (Tri::Unknown, _) | (_, Tri::Unknown) => Tri::Unknown,
            _ => Tri::No,
        }
    }
}

/// The fields of one article a rule can test.
#[derive(Debug, Clone, Copy)]
pub struct ArticleView<'a> {
    pub title: &'a str,
    pub category: &'a str,
    pub author: &'a str,
    pub description: &'a str,
    pub link: &'a str,
    /// `None` when the feed gives none (a stored 0).
    pub size: Option<i64>,
    pub date: i64,
    /// Unix seconds, for `age`.
    pub now: i64,
}

impl<'a> ArticleView<'a> {
    pub fn from_row(a: &'a crate::store::RssArticleRow, now: i64) -> Self {
        ArticleView {
            title: &a.title,
            category: &a.category,
            author: &a.author,
            description: &a.description,
            link: &a.link,
            size: (a.size > 0).then_some(a.size),
            date: a.date,
            now,
        }
    }
}

/// What `decide` says for an article that passes.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// The smart filter's key, empty when the title has none.
    pub episode: String,
    /// The size was unknown and mattered: judge again on the fetched torrent.
    pub recheck: bool,
}

impl Test {
    fn eval(&self, v: &ArticleView) -> Tri {
        let yes = |b: bool| if b { Tri::Yes } else { Tri::No };
        let text = match self.field {
            Field::Title => v.title,
            Field::Category => v.category,
            Field::Author => v.author,
            Field::Description => v.description,
            Field::Link => v.link,
            Field::Size | Field::Age => {
                let x = if self.field == Field::Size {
                    match v.size {
                        Some(s) => s,
                        None => return Tri::Unknown,
                    }
                } else if v.date > 0 {
                    v.now - v.date
                } else {
                    return Tri::Unknown;
                };
                return yes(match self.op {
                    Op::Gt => x > self.n1,
                    Op::Lt => x < self.n1,
                    _ => x >= self.n1 && x <= self.n2,
                });
            }
        };
        yes(match self.op {
            Op::Contains => must_contain(false, &self.value, text),
            Op::NotContains => !must_not_contain(false, &self.value, text),
            Op::Is => text.trim().to_lowercase() == self.value.trim().to_lowercase(),
            Op::IsNot => text.trim().to_lowercase() != self.value.trim().to_lowercase(),
            Op::Regex => regex_ci(&self.value).is_ok_and(|r| r.is_match(text)),
            Op::NotRegex => regex_ci(&self.value).is_ok_and(|r| !r.is_match(text)),
            // Refused at load: a number op on a text field.
            Op::Gt | Op::Lt | Op::Between => false,
        })
    }
}

impl Cond {
    pub fn eval(&self, v: &ArticleView) -> Tri {
        match self {
            Cond::Test(t) => t.eval(v),
            Cond::Group { any, rows } => {
                let decided = if *any { Tri::Yes } else { Tri::No };
                let mut acc = if *any { Tri::No } else { Tri::Yes };
                for r in rows {
                    let x = r.eval(v);
                    acc = if *any { acc.or(x) } else { acc.and(x) };
                    // Nothing later can change a decided group.
                    if acc == decided {
                        break;
                    }
                }
                acc
            }
        }
    }

    fn uses_size(&self) -> bool {
        match self {
            Cond::Test(t) => t.field == Field::Size,
            Cond::Group { rows, .. } => rows.iter().any(Cond::uses_size),
        }
    }

    /// Depth and node count, and what a loaded row already guarantees for a
    /// rule built by hand: regexes that compile, a `between` that is ordered.
    fn validate(&self, depth: usize, nodes: &mut usize) -> Result<(), String> {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(format!("conditions: more than {MAX_NODES} rows and groups"));
        }
        match self {
            Cond::Group { rows, .. } => {
                if depth > MAX_DEPTH {
                    return Err(format!(
                        "conditions: groups nested more than {MAX_DEPTH} deep"
                    ));
                }
                rows.iter().try_for_each(|r| r.validate(depth + 1, nodes))
            }
            Cond::Test(t) => {
                if t.field.numeric() != t.op.numeric() {
                    return Err(format!(
                        "conditions: {} cannot be tested with {}",
                        t.field.name(),
                        t.op.name()
                    ));
                }
                if matches!(t.op, Op::Regex | Op::NotRegex) {
                    regex_ci(&t.value).map_err(|e| {
                        format!(
                            "conditions: {:?} is not a valid regular expression: {e}",
                            t.value
                        )
                    })?;
                }
                if t.op == Op::Between && t.n2 < t.n1 {
                    return Err(format!(
                        "conditions: {} between {:?} and {:?}: the second value is smaller",
                        t.field.name(),
                        t.value,
                        t.value2
                    ));
                }
                Ok(())
            }
        }
    }

    /// `None` for an empty group, which is dropped.
    fn from_json(v: &Value, depth: usize, nodes: &mut usize) -> Result<Option<Cond>, String> {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(format!("conditions: more than {MAX_NODES} rows and groups"));
        }
        let o = v
            .as_object()
            .ok_or_else(|| format!("conditions: a row must be an object, not {v}"))?;
        let text = |k: &str| -> Result<Option<String>, String> {
            match o.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(s)) => Ok(Some(s.clone())),
                Some(Value::Number(n)) => Ok(Some(n.to_string())),
                Some(other) => Err(format!(
                    "conditions: {k} must be text or a number, not {other}"
                )),
            }
        };
        if let Some(rows) = o.get("rows") {
            if depth > MAX_DEPTH {
                return Err(format!(
                    "conditions: groups nested more than {MAX_DEPTH} deep"
                ));
            }
            let any = match text("match")?.map(|m| m.to_ascii_lowercase()).as_deref() {
                None | Some("all") | Some("and") => false,
                Some("any") | Some("or") => true,
                Some(other) => {
                    return Err(format!(
                        "conditions: match must be all or any, not {other:?}"
                    ))
                }
            };
            let rows = rows
                .as_array()
                .ok_or_else(|| format!("conditions: rows must be a list, not {rows}"))?;
            let mut out = Vec::new();
            for r in rows {
                out.extend(Cond::from_json(r, depth + 1, nodes)?);
            }
            return Ok((!out.is_empty()).then_some(Cond::Group { any, rows: out }));
        }
        let f = text("field")?.unwrap_or_default();
        let field = Field::parse(&f).ok_or_else(|| {
            format!(
                "conditions: field {f:?} is not title, category, author, description, \
                 link, size or age"
            )
        })?;
        let p = text("op")?.unwrap_or_default();
        let op = Op::parse(&p).ok_or_else(|| format!("conditions: op {p:?} is not understood"))?;
        let value = text("value")?.unwrap_or_default();
        let value2 = text("value2")?.unwrap_or_default();
        let (mut n1, mut n2) = (0, 0);
        if field.numeric() && op.numeric() {
            let num = |s: &str| {
                if field == Field::Size {
                    parse_size(s)
                } else {
                    parse_hours(s)
                }
                .map_err(|e| format!("conditions: {} {e}", field.name()))
            };
            n1 = num(&value)?;
            if op == Op::Between {
                n2 = num(&value2)?;
            }
        }
        let t = Cond::Test(Test {
            field,
            op,
            value,
            value2,
            n1,
            n2,
        });
        t.validate(depth, &mut 0)?;
        Ok(Some(t))
    }

    fn to_json(&self) -> Value {
        match self {
            Cond::Group { any, rows } => json!({
                "match": if *any { "any" } else { "all" },
                "rows": rows.iter().map(Cond::to_json).collect::<Vec<_>>(),
            }),
            Cond::Test(t) if t.op == Op::Between => json!({
                "field": t.field.name(), "op": t.op.name(), "value": t.value, "value2": t.value2,
            }),
            Cond::Test(t) => json!({"field": t.field.name(), "op": t.op.name(), "value": t.value}),
        }
    }
}

/// `700 MB`, `1.5 GiB`, `2048`: bytes. 1024-based whatever the spelling.
pub fn parse_size(s: &str) -> Result<i64, String> {
    let t = s.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num
        .replace(',', ".")
        .parse()
        .map_err(|_| format!("size {s:?} is not a number with a unit, like 700 MB or 1.5 GiB"))?;
    let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1048576.0,
        "g" | "gb" | "gib" => 1073741824.0,
        "t" | "tb" | "tib" => 1099511627776.0,
        u => return Err(format!("size {s:?}: unit {u:?} is not B, KB, MB, GB or TB")),
    };
    let v = n * mult;
    if !v.is_finite() || v > 9.0e15 {
        return Err(format!("size {s:?} is out of range"));
    }
    Ok(v.round() as i64)
}

/// `12`, `12h`, `2d`: hours (a plain number is hours), as seconds.
fn parse_hours(s: &str) -> Result<i64, String> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mult) = match t.strip_suffix('d') {
        Some(n) => (n, 86400.0),
        None => (t.strip_suffix('h').unwrap_or(&t), 3600.0),
    };
    let n: f64 = num
        .trim()
        .replace(',', ".")
        .parse()
        .map_err(|_| format!("age {s:?} is not a number of hours (12, 12h or 2d)"))?;
    if !n.is_finite() || !(0.0..=1.0e9).contains(&n) {
        return Err(format!("age {s:?} is out of range"));
    }
    Ok((n * mult).round() as i64)
}

/// `12.3 GB`, `700 MB`, `1 GB`.
fn human(n: i64) -> String {
    let (div, unit) = match n {
        n if n >= 1 << 40 => (1099511627776.0, "TB"),
        n if n >= 1 << 30 => (1073741824.0, "GB"),
        n if n >= 1 << 20 => (1048576.0, "MB"),
        n if n >= 1 << 10 => (1024.0, "KB"),
        _ => (1.0, "B"),
    };
    let s = format!("{:.1}", n as f64 / div);
    format!("{} {unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// `minSize` / `maxSize`: bytes, or text like `1 GB`; absent = 0.
fn size_limit(obj: &serde_json::Map<String, Value>, k: &str) -> Result<i64, String> {
    let n = match obj.get(k) {
        None | Some(Value::Null) => return Ok(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) if s.trim().is_empty() => 0,
        Some(Value::String(s)) => parse_size(s).map_err(|e| format!("{k}: {e}"))?,
        Some(other) => return Err(format!("{k} must be a size, not {other}")),
    };
    if n < 0 {
        return Err(format!("{k} must not be negative"));
    }
    Ok(n)
}

impl Rule {
    /// From a qBittorrent rule document. Unknown fields are ignored; a field
    /// of the wrong type is a refusal, not a silent default.
    pub fn from_json(name: &str, v: &Value) -> Result<Rule, String> {
        let obj = v
            .as_object()
            .ok_or("the rule definition is not a JSON object")?;
        let s = |k: &str| -> Result<String, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(String::new()),
                Some(Value::String(s)) => Ok(s.clone()),
                Some(other) => Err(format!("{k} must be a string, not {other}")),
            }
        };
        let b = |k: &str, d: bool| -> Result<bool, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(d),
                Some(Value::Bool(b)) => Ok(*b),
                Some(other) => Err(format!("{k} must be true or false, not {other}")),
            }
        };
        let list = |k: &str| -> Result<Vec<String>, String> {
            match obj.get(k) {
                None | Some(Value::Null) => Ok(Vec::new()),
                Some(Value::Array(a)) => Ok(a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()),
                // qBittorrent's own UI once sent one string with the URLs on lines.
                Some(Value::String(s)) => Ok(s
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()),
                Some(other) => Err(format!("{k} must be a list, not {other}")),
            }
        };
        let tp = obj.get("torrentParams").and_then(Value::as_object);
        let tp_s = |k: &str| {
            tp.and_then(|t| t.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let tp_b = |k: &str| tp.and_then(|t| t.get(k)).and_then(Value::as_bool);
        let add_paused = match obj.get("addPaused") {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        };
        let r = Rule {
            name: name.to_string(),
            enabled: b("enabled", true)?,
            priority: obj.get("priority").and_then(Value::as_i64).unwrap_or(0),
            use_regex: b("useRegex", false)?,
            must_contain: s("mustContain")?,
            must_not_contain: s("mustNotContain")?,
            episode_filter: s("episodeFilter")?.trim().to_string(),
            affected_feeds: list("affectedFeeds")?,
            ignore_days: obj
                .get("ignoreDays")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0),
            last_match: match obj.get("lastMatch") {
                Some(Value::String(d)) => super::parse::parse_date(d).unwrap_or(0),
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
                _ => 0,
            },
            smart_filter: b("smartFilter", false)?,
            previously_matched: list("previouslyMatchedEpisodes")?,
            category: tp_s("category")
                .filter(|c| !c.is_empty())
                .unwrap_or(s("assignedCategory")?),
            save_path: tp_s("save_path")
                .filter(|c| !c.is_empty())
                .unwrap_or(s("savePath")?),
            tags: tp
                .and_then(|t| t.get("tags"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            add_paused: tp_b("stopped").or_else(|| tp_b("paused")).or(add_paused),
            conditions: match obj.get("conditions") {
                None | Some(Value::Null) => None,
                Some(c) => Cond::from_json(c, 1, &mut 0)?,
            },
            min_size: size_limit(obj, "minSize")?,
            max_size: size_limit(obj, "maxSize")?,
        };
        r.check()?;
        Ok(r)
    }

    /// The regular expressions compile and the episode filter reads: refused
    /// at save, rather than a rule that silently never matches.
    pub fn check(&self) -> Result<(), String> {
        if self.use_regex {
            for (k, e) in [
                ("mustContain", &self.must_contain),
                ("mustNotContain", &self.must_not_contain),
            ] {
                if !e.is_empty() {
                    regex_ci(e)
                        .map_err(|err| format!("{k} is not a valid regular expression: {err}"))?;
                }
            }
        }
        if !self.episode_filter.is_empty() {
            episode_ranges(&self.episode_filter)?;
        }
        if let Some(c) = &self.conditions {
            c.validate(1, &mut 0)?;
        }
        if self.min_size > 0 && self.max_size > 0 && self.max_size < self.min_size {
            return Err("maxSize is smaller than minSize".into());
        }
        Ok(())
    }

    /// qBittorrent's document, both spellings of the add options included.
    pub fn to_json(&self) -> Value {
        let mut doc = json!({
            "enabled": self.enabled,
            "priority": self.priority,
            "useRegex": self.use_regex,
            "mustContain": self.must_contain,
            "mustNotContain": self.must_not_contain,
            "episodeFilter": self.episode_filter,
            "affectedFeeds": self.affected_feeds,
            "ignoreDays": self.ignore_days,
            "lastMatch": if self.last_match > 0 { super::parse::rfc2822(self.last_match) } else { String::new() },
            "smartFilter": self.smart_filter,
            "previouslyMatchedEpisodes": self.previously_matched,
            "addPaused": self.add_paused,
            "assignedCategory": self.category,
            "savePath": self.save_path,
            "torrentContentLayout": Value::Null,
            "torrentParams": {
                "category": self.category,
                "save_path": self.save_path,
                "tags": self.tags,
                "stopped": self.add_paused,
                "skip_checking": false,
            },
        });
        // Hydranos's own keys: absent unless set, so a rule that does not use
        // them is the document qBittorrent writes.
        if let Some(c) = &self.conditions {
            doc["conditions"] = c.to_json();
        }
        if self.min_size > 0 {
            doc["minSize"] = json!(self.min_size);
        }
        if self.max_size > 0 {
            doc["maxSize"] = json!(self.max_size);
        }
        doc
    }

    /// Whether the rule judges the size, so an article without one is
    /// "unknown" for it.
    pub fn uses_size(&self) -> bool {
        self.min_size > 0
            || self.max_size > 0
            || self.conditions.as_ref().is_some_and(Cond::uses_size)
    }

    /// The tree and the size limits together; the limits are one more row.
    fn conditions_for(&self, v: &ArticleView) -> Tri {
        let mut r = self.conditions.as_ref().map_or(Tri::Yes, |c| c.eval(v));
        if r != Tri::No && (self.min_size > 0 || self.max_size > 0) {
            r = r.and(match v.size {
                None => Tri::Unknown,
                Some(s) if s >= self.min_size && (self.max_size <= 0 || s <= self.max_size) => {
                    Tri::Yes
                }
                Some(_) => Tri::No,
            });
        }
        r
    }

    /// The article's real size, known once its torrent is fetched: `Err`
    /// says why the rule does not take it.
    pub fn check_size(&self, v: &ArticleView, size: i64) -> Result<(), String> {
        let v = ArticleView {
            size: Some(size),
            ..*v
        };
        if self.conditions_for(&v) != Tri::No {
            return Ok(());
        }
        let range = match (self.min_size > 0, self.max_size > 0) {
            (true, true) => format!("{}\u{2013}{}", human(self.min_size), human(self.max_size)),
            (true, false) => format!("at least {}", human(self.min_size)),
            (false, true) => format!("at most {}", human(self.max_size)),
            (false, false) => String::new(),
        };
        Err(if range.is_empty() {
            format!("size {} does not fit the rule's conditions", human(size))
        } else {
            format!("size {} outside the rule ({range})", human(size))
        })
    }

    /// Whether the title passes the rule's text and episode tests.
    pub fn matches_title(&self, title: &str) -> bool {
        must_contain(self.use_regex, &self.must_contain, title)
            && !must_not_contain(self.use_regex, &self.must_not_contain, title)
            && (self.episode_filter.is_empty()
                || episode_ranges(&self.episode_filter).is_ok_and(|r| episode_in(&r, title)))
    }

    /// qBittorrent's `matches`: the title, `ignoreDays` and the smart filter,
    /// then Hydranos's conditions and size limits. What `matchingArticles`
    /// lists; `enabled` and the feeds are `decide`'s.
    ///
    /// `ignoreDays` is qBittorrent's test: an article dated less than that
    /// many days after the rule's last match is not taken, so a rule that
    /// matched stays quiet for the period, and older articles never wake it.
    pub fn matches_article(&self, v: &ArticleView) -> Result<Verdict, &'static str> {
        let title = v.title;
        if !self.matches_title(title) {
            return Err("title does not match");
        }
        if self.ignore_days > 0
            && self.last_match > 0
            && v.date < self.last_match + self.ignore_days * 86400
        {
            return Err("ignoreDays: dated less than that many days after the last match");
        }
        let key = episode_key(title).unwrap_or_default();
        if self.smart_filter
            && !key.is_empty()
            && self.previously_matched.contains(&key)
            && !is_repack(title)
        {
            return Err("smart filter: episode already taken");
        }
        let recheck = match self.conditions_for(v) {
            Tri::No => return Err("conditions: the article does not satisfy them"),
            Tri::Unknown => true,
            Tri::Yes => false,
        };
        Ok(Verdict {
            episode: key,
            recheck,
        })
    }

    /// The whole decision for one article of the feed at `feed_url`: why
    /// not, or the verdict (the episode key the smart filter records, and
    /// whether the size is yet to be checked).
    pub fn decide(&self, feed_url: &str, v: &ArticleView) -> Result<Verdict, &'static str> {
        if !self.enabled {
            return Err("rule disabled");
        }
        if !self.affected_feeds.iter().any(|u| u == feed_url) {
            return Err("feed not in the rule");
        }
        self.matches_article(v)
    }
}

/// Compiled once per expression: a pass offers every unread article of a
/// feed to every rule, and each word of a rule is an expression.
fn regex_ci(e: &str) -> Result<Regex, regex::Error> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Regex>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(r) = cache.lock().ok().and_then(|c| c.get(e).cloned()) {
        return Ok(r);
    }
    let r = RegexBuilder::new(e)
        .case_insensitive(true)
        .size_limit(1 << 20)
        .build()?;
    if let Ok(mut c) = cache.lock() {
        if c.len() >= 1024 {
            c.clear();
        }
        c.insert(e.to_string(), r.clone());
    }
    Ok(r)
}

/// A wildcard word (`*`, `?`) anywhere in the title, case ignored.
fn wildcard(word: &str) -> Option<Regex> {
    let mut p = String::new();
    for c in word.chars() {
        match c {
            '*' => p.push_str(".*"),
            '?' => p.push('.'),
            c => p.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex_ci(&p).ok()
}

/// One alternative: every word present.
fn alternative(expr: &str, title: &str) -> bool {
    expr.split_whitespace()
        .all(|w| wildcard(w).is_some_and(|r| r.is_match(title)))
}

fn must_contain(regex: bool, expr: &str, title: &str) -> bool {
    if expr.is_empty() {
        return true;
    }
    if regex {
        return regex_ci(expr).is_ok_and(|r| r.is_match(title));
    }
    expr.split('|').any(|alt| alternative(alt, title))
}

fn must_not_contain(regex: bool, expr: &str, title: &str) -> bool {
    if expr.trim().is_empty() {
        return false;
    }
    if regex {
        return regex_ci(expr).is_ok_and(|r| r.is_match(title));
    }
    expr.split('|')
        .filter(|alt| !alt.trim().is_empty())
        .any(|alt| alternative(alt, title))
}

/// An inclusive range of (season, episode); `to` None = open.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeRange {
    from: (u32, u32),
    to: Option<(u32, u32)>,
}

/// Read a filter, qBittorrent's `1x2;8-15;30-;` or `S01E01-S01E10;S02E05;`.
pub fn episode_ranges(filter: &str) -> Result<Vec<EpisodeRange>, String> {
    let f = filter.trim();
    let bad = || {
        format!(
            "episode filter {f:?} is not understood: write qBittorrent's 1x2;8-15;30-; \
             (season x episodes) or S01E01-S01E10;S02E05;"
        )
    };
    let lower = f.to_ascii_lowercase();
    let mut out = Vec::new();
    if lower.starts_with('s') {
        let ep = |t: &str| -> Option<(u32, u32)> {
            let t = t.trim().to_ascii_lowercase();
            let t = t.strip_prefix('s')?;
            let (s, e) = t.split_once('e')?;
            Some((s.parse().ok()?, e.parse().ok()?))
        };
        for tok in lower
            .split([';', ','])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let r = match tok.split_once('-') {
                Some((a, "")) => EpisodeRange {
                    from: ep(a).ok_or_else(bad)?,
                    to: None,
                },
                Some((a, b)) => EpisodeRange {
                    from: ep(a).ok_or_else(bad)?,
                    to: Some(ep(b).ok_or_else(bad)?),
                },
                None => {
                    let e = ep(tok).ok_or_else(bad)?;
                    EpisodeRange {
                        from: e,
                        to: Some(e),
                    }
                }
            };
            out.push(r);
        }
    } else {
        let (season, eps) = lower.split_once('x').ok_or_else(bad)?;
        let season: u32 = season.trim().parse().map_err(|_| bad())?;
        if season == 0 {
            return Err(bad());
        }
        for tok in eps.split(';').map(str::trim).filter(|t| !t.is_empty()) {
            let n = |t: &str| t.trim().parse::<u32>().map_err(|_| bad());
            let r = match tok.split_once('-') {
                Some((a, "")) => EpisodeRange {
                    from: (season, n(a)?),
                    to: None,
                },
                Some((a, b)) => {
                    let (a, b) = (n(a)?, n(b)?);
                    // qBittorrent ignores a reversed range rather than refusing it.
                    if a > b {
                        continue;
                    }
                    EpisodeRange {
                        from: (season, a),
                        to: Some((season, b)),
                    }
                }
                None => {
                    let e = n(tok)?;
                    EpisodeRange {
                        from: (season, e),
                        to: Some((season, e)),
                    }
                }
            };
            out.push(r);
        }
    }
    if out.is_empty() {
        return Err(bad());
    }
    Ok(out)
}

fn episode_in(ranges: &[EpisodeRange], title: &str) -> bool {
    let Some(ep) = episode_of(title) else {
        return false;
    };
    ranges
        .iter()
        .any(|r| ep >= r.from && r.to.is_none_or(|to| ep <= to))
}

/// The (season, episode) a title names: `S01E05`, `s1.e5`, `S01 E05`, `1x05`.
pub fn episode_of(title: &str) -> Option<(u32, u32)> {
    use std::sync::OnceLock;
    static SE: OnceLock<Regex> = OnceLock::new();
    static X: OnceLock<Regex> = OnceLock::new();
    let se = SE.get_or_init(|| regex_ci(r"\bs0*(\d{1,4})[ \-_.]?e0*(\d{1,4})(?:\D|$)").unwrap());
    let x = X.get_or_init(|| regex_ci(r"\b(\d{1,4})x0*(\d{1,4})(?:\D|$)").unwrap());
    let c = se.captures(title).or_else(|| x.captures(title))?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?))
}

/// The smart filter's key: `S01E05`, or the date of a daily show.
fn episode_key(title: &str) -> Option<String> {
    if let Some((s, e)) = episode_of(title) {
        return Some(format!("S{s:02}E{e:02}"));
    }
    use std::sync::OnceLock;
    static DATE: OnceLock<Regex> = OnceLock::new();
    let d = DATE.get_or_init(|| regex_ci(r"\b(\d{4})[.\-](\d{1,2})[.\-](\d{1,2})\b").unwrap());
    d.captures(title)
        .map(|c| format!("{}-{:0>2}-{:0>2}", &c[1], &c[2], &c[3]))
}

fn is_repack(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    t.contains("repack") || t.contains("proper")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(v: Value) -> Rule {
        Rule::from_json("r", &v).unwrap()
    }

    fn view(title: &str, date: i64) -> ArticleView<'_> {
        ArticleView {
            title,
            category: "",
            author: "",
            description: "",
            link: "",
            size: None,
            date,
            now: date,
        }
    }

    #[test]
    fn words_are_and_bars_are_or_and_case_is_ignored() {
        let r = rule(json!({"mustContain": "show 1080p|other 720p"}));
        assert!(r.matches_title("The.Show.S01E01.1080P.WEB"));
        assert!(r.matches_title("1080p of the SHOW"), "any order");
        assert!(r.matches_title("Other.Thing.720p"));
        assert!(
            !r.matches_title("The.Show.S01E01.720p"),
            "one word missing in each alternative"
        );
        let w = rule(json!({"mustContain": "sh?w*1080"}));
        assert!(w.matches_title("The.Show.S01E01.1080p"), "wildcards");
        assert!(!w.matches_title("The.Shoow.1080p"));
        // qBittorrent: an empty alternative matches everything.
        assert!(rule(json!({"mustContain": "nothing|"})).matches_title("whatever"));
    }

    #[test]
    fn must_not_contain_rejects_on_any_alternative() {
        let r = rule(json!({"mustContain": "show", "mustNotContain": "cam|hdts|"}));
        assert!(r.matches_title("Show.1080p"));
        assert!(!r.matches_title("Show.CAM"));
        assert!(!r.matches_title("Show.HDTS.x264"));
        let both = rule(json!({"mustNotContain": "french dub"}));
        assert!(
            both.matches_title("Show.French.Sub"),
            "only both words reject"
        );
        assert!(!both.matches_title("Show.Dub.FRENCH"));
        // No `-word` exclusion: the minus is a character like any other.
        assert!(!rule(json!({"mustContain": "-cam"})).matches_title("show cam"));
    }

    #[test]
    fn regex_rules_use_the_whole_field_as_one_expression() {
        let r = rule(
            json!({"useRegex": true, "mustContain": r"^show\.s\d+e\d+\.(1080|2160)p", "mustNotContain": "(?:cam|ts)$"}),
        );
        assert!(r.matches_title("Show.S01E02.2160p.WEB"));
        assert!(!r.matches_title("A.Show.S01E02.2160p"), "anchored");
        assert!(!r.matches_title("Show.S01E02.1080p.TS"));
        assert!(Rule::from_json(
            "bad",
            &json!({"useRegex": true, "mustContain": "(unclosed"})
        )
        .is_err());
    }

    #[test]
    fn the_episode_filter_takes_qbittorrent_syntax() {
        let r = rule(json!({"episodeFilter": "1x2;8-15;30-;"}));
        assert!(r.matches_title("Show.S01E02.1080p"));
        assert!(r.matches_title("Show 1x08"));
        assert!(r.matches_title("Show.s1.e15"));
        assert!(!r.matches_title("Show.S01E16"));
        assert!(!r.matches_title("Show.S01E03"));
        assert!(r.matches_title("Show.S01E31"), "open range");
        assert!(
            r.matches_title("Show.S02E01"),
            "an open range runs into later seasons"
        );
        assert!(!rule(json!({"episodeFilter": "2x1-5;"})).matches_title("Show.S01E03"));
        assert!(
            !r.matches_title("Show.Season.1.Complete"),
            "no episode number"
        );
        assert!(Rule::from_json("bad", &json!({"episodeFilter": "season one"})).is_err());
        assert!(
            Rule::from_json("bad", &json!({"episodeFilter": "0x1;"})).is_err(),
            "season 0"
        );
    }

    #[test]
    fn the_episode_filter_takes_sxxexx_ranges_across_seasons() {
        let r = rule(json!({"episodeFilter": "S01E01-S01E10"}));
        assert!(r.matches_title("Show.S01E10.720p"));
        assert!(!r.matches_title("Show.S01E11.720p"));
        let across = rule(json!({"episodeFilter": "S01E20-S02E02;S05E01-"}));
        assert!(across.matches_title("Show.S01E22"));
        assert!(across.matches_title("Show.S02E01"));
        assert!(!across.matches_title("Show.S02E03"));
        assert!(across.matches_title("Show.S07E01"));
    }

    #[test]
    fn ignore_days_and_the_smart_filter_hold_a_rule_back() {
        let now = 1_800_000_000;
        let mut r =
            rule(json!({"mustContain": "show", "affectedFeeds": ["http://f"], "ignoreDays": 2}));
        assert_eq!(
            r.decide("http://f", &view("Show.S01E01", now))
                .map(|v| v.episode),
            Ok("S01E01".to_string())
        );
        assert!(
            r.decide("http://other", &view("Show.S01E01", now)).is_err(),
            "feed not in the rule"
        );
        r.last_match = now - 86400;
        assert!(
            r.decide("http://f", &view("Show.S01E02", now)).is_err(),
            "matched a day ago"
        );
        r.last_match = now - 3 * 86400;
        assert!(r.decide("http://f", &view("Show.S01E02", now)).is_ok());

        let mut s = rule(
            json!({"mustContain": "show", "affectedFeeds": ["http://f"], "smartFilter": true}),
        );
        s.previously_matched.push("S01E01".into());
        assert!(s
            .decide("http://f", &view("Show.S01E01.720p", now))
            .is_err());
        assert!(
            s.decide("http://f", &view("Show.S01E01.REPACK", now))
                .is_ok(),
            "a repack is taken"
        );
        assert!(s.decide("http://f", &view("Show.S01E02", now)).is_ok());
        s.enabled = false;
        assert!(s.decide("http://f", &view("Show.S01E02", now)).is_err());
    }

    #[test]
    fn the_document_round_trips_in_both_spellings() {
        let old = rule(
            json!({"assignedCategory": "tv", "savePath": "/data/tv", "addPaused": true, "affectedFeeds": ["u"]}),
        );
        assert_eq!(
            (
                old.category.as_str(),
                old.save_path.as_str(),
                old.add_paused
            ),
            ("tv", "/data/tv", Some(true))
        );
        let new = rule(
            json!({"torrentParams": {"category": "tv", "save_path": "/data/tv", "stopped": false, "tags": ["a"]}}),
        );
        assert_eq!(
            (new.category.as_str(), new.add_paused, new.tags.clone()),
            ("tv", Some(false), vec!["a".to_string()])
        );
        let back = Rule::from_json("r", &old.to_json()).unwrap();
        assert_eq!(back, old);
        assert!(
            Rule::from_json("r", &json!({"enabled": "yes"})).is_err(),
            "a wrong type is refused"
        );
        assert!(Rule::from_json("r", &json!([1])).is_err());
    }

    const NOW: i64 = 1_800_000_000;
    const GB: i64 = 1 << 30;

    fn art<'a>(title: &'a str, category: &'a str, size: Option<i64>) -> ArticleView<'a> {
        ArticleView {
            title,
            category,
            author: "Ann Author",
            description: "A long <b>description</b>",
            link: "https://tracker.example/view/1",
            size,
            date: NOW - 2 * 3600,
            now: NOW,
        }
    }

    fn cond(v: Value) -> Cond {
        rule(json!({ "conditions": v })).conditions.unwrap()
    }

    fn row(field: &str, op: &str, value: &str) -> Value {
        json!({"field": field, "op": op, "value": value})
    }

    #[test]
    fn every_text_field_and_op_is_tested() {
        let a = art("The.Show.S01E01.1080p", "TV", Some(GB));
        let t = |f: &str, op: &str, v: &str| cond(row(f, op, v)).eval(&a);
        assert_eq!(t("title", "contains", "show 1080p|nothing"), Tri::Yes);
        assert_eq!(t("title", "contains", "720p"), Tri::No);
        assert_eq!(t("title", "not_contains", "720p|cam"), Tri::Yes);
        assert_eq!(t("title", "not_contains", "1080p"), Tri::No);
        assert_eq!(t("category", "is", "tv"), Tri::Yes, "case is ignored");
        assert_eq!(t("category", "is", "T"), Tri::No, "the whole text");
        assert_eq!(t("category", "is_not", "Movies"), Tri::Yes);
        assert_eq!(t("category", "is_not", "TV"), Tri::No);
        assert_eq!(t("author", "contains", "author ann"), Tri::Yes);
        assert_eq!(t("description", "regex", r"<b>desc\w+</b>"), Tri::Yes);
        assert_eq!(t("description", "not_regex", "zzz"), Tri::Yes);
        assert_eq!(t("link", "contains", "tracker.example/view/*"), Tri::Yes);
        assert_eq!(t("link", "not_regex", r"/view/\d"), Tri::No);
    }

    #[test]
    fn size_and_age_take_numbers() {
        let a = art("t", "", Some(2 * GB));
        let t = |f: &str, op: &str, v: &str, v2: &str| {
            cond(json!({"field": f, "op": op, "value": v, "value2": v2})).eval(&a)
        };
        assert_eq!(t("size", "gt", "1 GB", ""), Tri::Yes);
        assert_eq!(t("size", "gt", "2 GiB", ""), Tri::No, "gt is strict");
        assert_eq!(t("size", "lt", "3 GB", ""), Tri::Yes);
        assert_eq!(
            t("size", "between", "2 GB", "2048 MB"),
            Tri::Yes,
            "inclusive"
        );
        assert_eq!(t("size", "between", "3 GB", "4 GB"), Tri::No);
        // Dated two hours before `now`.
        assert_eq!(t("age", "lt", "3", ""), Tri::Yes);
        assert_eq!(t("age", "gt", "2h", ""), Tri::No);
        assert_eq!(t("age", "between", "1", "1d"), Tri::Yes);
    }

    #[test]
    fn groups_nest_and_combine() {
        let tree = cond(json!({"match": "all", "rows": [
            row("category", "is", "TV"),
            {"match": "any", "rows": [row("title", "contains", "2160p"), row("title", "contains", "1080p")]},
        ]}));
        assert_eq!(tree.eval(&art("Show 1080p", "TV", None)), Tri::Yes);
        assert_eq!(tree.eval(&art("Show 720p", "TV", None)), Tri::No);
        assert_eq!(tree.eval(&art("Show 1080p", "Movies", None)), Tri::No);
    }

    #[test]
    fn an_unknown_size_is_three_valued() {
        let big = row("size", "gt", "1 GB");
        let tv = row("category", "is", "TV");
        let all = |a: &Value, b: &Value| cond(json!({"match": "all", "rows": [a, b]}));
        let any = |a: &Value, b: &Value| cond(json!({"match": "any", "rows": [a, b]}));
        let unknown = art("t", "TV", None);
        let movie = art("t", "Movies", None);
        assert_eq!(cond(big.clone()).eval(&unknown), Tri::Unknown);
        assert_eq!(all(&tv, &big).eval(&unknown), Tri::Unknown);
        assert_eq!(all(&tv, &big).eval(&movie), Tri::No, "a failure decides");
        assert_eq!(any(&tv, &big).eval(&unknown), Tri::Yes, "a pass decides");
        assert_eq!(any(&tv, &big).eval(&movie), Tri::Unknown);
        assert_eq!(
            any(&tv, &big).eval(&art("t", "Movies", Some(2 * GB))),
            Tri::Yes
        );
    }

    #[test]
    fn size_limits_are_a_size_test_and_decide_says_when_to_recheck() {
        let r = rule(json!({
            "affectedFeeds": ["f"], "minSize": "1 GB", "maxSize": 8 * GB,
        }));
        let ok = |size| r.decide("f", &art("t", "", size));
        assert_eq!(ok(Some(GB)).map(|v| v.recheck), Ok(false), "inclusive");
        assert_eq!(ok(Some(8 * GB)).map(|v| v.recheck), Ok(false));
        assert!(ok(Some(GB - 1)).is_err());
        assert!(ok(Some(8 * GB + 1)).is_err());
        assert_eq!(
            ok(None).map(|v| v.recheck),
            Ok(true),
            "unknown is let through"
        );
        let msg = r
            .check_size(&art("t", "", None), 12 * GB + 3 * GB / 10)
            .unwrap_err();
        assert_eq!(msg, "size 12.3 GB outside the rule (1 GB\u{2013}8 GB)");
        assert!(r.check_size(&art("t", "", None), 2 * GB).is_ok());
        let min = rule(json!({"minSize": GB}));
        assert!(min
            .check_size(&art("t", "", None), GB / 2)
            .unwrap_err()
            .contains("at least 1 GB"));
    }

    #[test]
    fn the_conditions_reject_with_a_reason() {
        let r = rule(
            json!({"affectedFeeds": ["f"], "conditions": {"rows": [row("category", "is", "TV")]}}),
        );
        let why = r.decide("f", &art("t", "Movies", None)).unwrap_err();
        assert!(why.starts_with("conditions"), "{why}");
        assert!(r.decide("f", &art("t", "tv", None)).is_ok());
    }

    #[test]
    fn sizes_read_in_every_spelling() {
        assert_eq!(parse_size("700 MB"), Ok(700 << 20));
        assert_eq!(parse_size("1.5 GiB"), Ok(3 * GB / 2));
        assert_eq!(parse_size("1,5gb"), Ok(3 * GB / 2));
        assert_eq!(parse_size("2048"), Ok(2048));
        assert_eq!(parse_size(" 3 tb "), Ok(3 << 40));
        assert!(parse_size("big").is_err());
        assert!(parse_size("5 parsecs").is_err());
        assert!(parse_size("").is_err());
    }

    #[test]
    fn a_bad_condition_is_refused_at_save() {
        let bad = |c: Value| Rule::from_json("r", &json!({ "conditions": c })).is_err();
        assert!(bad(row("title", "regex", "(unclosed")));
        assert!(bad(row("size", "gt", "lots")));
        assert!(bad(row("size", "between", "")));
        assert!(
            bad(json!({"field": "size", "op": "between", "value": "5 GB", "value2": "1 GB"})),
            "reversed"
        );
        assert!(bad(row("size", "contains", "1")), "a text op on a number");
        assert!(bad(row("title", "gt", "1")), "a number op on text");
        assert!(bad(row("colour", "is", "red")));
        assert!(bad(row("title", "sounds_like", "red")));
        assert!(bad(json!("title is x")), "not an object");
        assert!(bad(json!({"match": "most", "rows": []})));
        assert!(bad(json!({"rows": "none"})));
        assert!(Rule::from_json("r", &json!({"minSize": "5 GB", "maxSize": "1 GB"})).is_err());
        assert!(Rule::from_json("r", &json!({"minSize": -1})).is_err());
        assert!(Rule::from_json("r", &json!({"maxSize": true})).is_err());

        // Eight levels are allowed, nine are not.
        let nest = |n: usize| {
            let mut c = row("title", "contains", "x");
            for _ in 0..n {
                c = json!({"match": "all", "rows": [c]});
            }
            c
        };
        assert!(!bad(nest(8)));
        assert!(bad(nest(9)));
        let rows = |n: usize| {
            (0..n)
                .map(|_| row("title", "contains", "x"))
                .collect::<Vec<_>>()
        };
        assert!(
            bad(json!({ "rows": rows(100) })),
            "101 nodes with the group"
        );
        assert!(!bad(json!({ "rows": rows(99) })));
        // A rule built by hand is checked the same.
        let r = Rule {
            conditions: Some(Cond::Group {
                any: false,
                rows: vec![Cond::Test(Test {
                    field: Field::Title,
                    op: Op::Regex,
                    value: "(".into(),
                    value2: String::new(),
                    n1: 0,
                    n2: 0,
                })],
            }),
            ..Rule::default()
        };
        assert!(r.check().is_err());
    }

    #[test]
    fn empty_groups_are_dropped() {
        let r = rule(json!({"conditions": {"match": "any", "rows": [{"rows": []}]}}));
        assert_eq!(r.conditions, None);
        let r = rule(json!({"conditions": {"rows": [{"rows": []}, row("title", "is", "x")]}}));
        assert!(matches!(&r.conditions, Some(Cond::Group { rows, .. }) if rows.len() == 1));
    }

    #[test]
    fn the_new_keys_round_trip_and_are_absent_when_unset() {
        let plain = rule(json!({"mustContain": "x", "affectedFeeds": ["u"]}));
        let doc = plain.to_json();
        for k in ["conditions", "minSize", "maxSize"] {
            assert!(doc.get(k).is_none(), "{k} is written only when set");
        }
        assert_eq!(Rule::from_json("r", &doc).unwrap(), plain);

        let full = rule(json!({
            "minSize": "1 GB", "maxSize": 8 * GB,
            "conditions": {"match": "any", "rows": [
                row("category", "is", "TV"),
                {"match": "all", "rows": [
                    {"field": "size", "op": "between", "value": "700 MB", "value2": "1.5 GiB"},
                    row("age", "lt", "12"),
                ]},
            ]},
        }));
        let doc = full.to_json();
        assert_eq!(doc["minSize"], json!(GB));
        assert_eq!(doc["maxSize"], json!(8 * GB));
        assert_eq!(doc["conditions"]["match"], "any");
        assert_eq!(doc["conditions"]["rows"][1]["rows"][0]["value"], "700 MB");
        assert_eq!(Rule::from_json("r", &doc).unwrap(), full);
        assert!(full.uses_size());
        assert!(!plain.uses_size());
    }

    #[test]
    fn a_qbittorrent_document_is_read_as_before() {
        let r = rule(json!({
            "enabled": true, "mustContain": "show", "useRegex": false,
            "affectedFeeds": ["f"], "assignedCategory": "tv",
        }));
        assert_eq!(
            (r.conditions.is_none(), r.min_size, r.max_size),
            (true, 0, 0)
        );
        let v = r.decide("f", &art("The Show", "Whatever", None)).unwrap();
        assert!(!v.recheck, "no size test, so an unknown size is no matter");
    }
}
