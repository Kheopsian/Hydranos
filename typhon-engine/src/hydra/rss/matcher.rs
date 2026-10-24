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
        Ok(())
    }

    /// qBittorrent's document, both spellings of the add options included.
    pub fn to_json(&self) -> Value {
        json!({
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
    /// for an article dated `date`. What `matchingArticles` lists; `enabled`
    /// and the feeds are `decide`'s.
    ///
    /// `ignoreDays` is qBittorrent's test: an article dated less than that
    /// many days after the rule's last match is not taken, so a rule that
    /// matched stays quiet for the period, and older articles never wake it.
    pub fn matches_article(&self, title: &str, date: i64) -> Result<String, &'static str> {
        if !self.matches_title(title) {
            return Err("title does not match");
        }
        if self.ignore_days > 0
            && self.last_match > 0
            && date < self.last_match + self.ignore_days * 86400
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
        Ok(key)
    }

    /// The whole decision for one article of the feed at `feed_url`: why
    /// not, or `Ok` with the episode key the smart filter records (empty when
    /// the title has none).
    pub fn decide(&self, feed_url: &str, title: &str, date: i64) -> Result<String, &'static str> {
        if !self.enabled {
            return Err("rule disabled");
        }
        if !self.affected_feeds.iter().any(|u| u == feed_url) {
            return Err("feed not in the rule");
        }
        self.matches_article(title, date)
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
            r.decide("http://f", "Show.S01E01", now),
            Ok("S01E01".to_string())
        );
        assert!(
            r.decide("http://other", "Show.S01E01", now).is_err(),
            "feed not in the rule"
        );
        r.last_match = now - 86400;
        assert!(
            r.decide("http://f", "Show.S01E02", now).is_err(),
            "matched a day ago"
        );
        r.last_match = now - 3 * 86400;
        assert!(r.decide("http://f", "Show.S01E02", now).is_ok());

        let mut s = rule(
            json!({"mustContain": "show", "affectedFeeds": ["http://f"], "smartFilter": true}),
        );
        s.previously_matched.push("S01E01".into());
        assert!(s.decide("http://f", "Show.S01E01.720p", now).is_err());
        assert!(
            s.decide("http://f", "Show.S01E01.REPACK", now).is_ok(),
            "a repack is taken"
        );
        assert!(s.decide("http://f", "Show.S01E02", now).is_ok());
        s.enabled = false;
        assert!(s.decide("http://f", "Show.S01E02", now).is_err());
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
}
