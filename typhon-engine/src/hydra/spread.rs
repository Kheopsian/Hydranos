//! Spreading a category's new torrents over several engines, here and on nodes.
//!
//! 3.x did it with `placement`, `agents`, `strategy` and `min_free_bytes`; the
//! 4.x port dropped all four and routed on the category's mode alone, so an
//! install with one engine per VPN tunnel (`vpn1`, `vpn2`...) put every new
//! torrent on the first engine of the role and left the others idle.
//!
//! The category now carries three NEW keys, deliberately not the old ones: real
//! `categories.json` files still hold the 3.x values, and waking those would
//! have switched an old configuration on for people who never asked.
//!
//!   * `targets`: where a new torrent may go. `"vpn1"` is a local engine,
//!     `"nas:vpn1"` the engine `vpn1` of the node named `nas`. Empty means every
//!     LOCAL engine whose role is the category's mode.
//!   * `spread`: how to pick among them (`count`, `bytes`, `fill`, `free_space`).
//!   * `reserve_bytes`: a target with less free space than this at the
//!     category's folder is never picked, whatever the strategy.
//!
//! Two halves, kept apart on purpose. `choose` is pure: it takes what was
//! measured and answers an index, so every strategy is tested without a disk or
//! a network. The rest gathers the measures (cached for a few seconds, so an
//! import of a thousand torrents does not make a thousand network calls) and
//! never holds the store's lock or any sync mutex across an await.
//!
//! A torrent already placed never moves: the rule applies at the add only.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api::AppState;

/// How long a measure is believed. Short enough that an operator who frees a
/// disk sees it used within seconds, long enough that a burst of adds costs one
/// measure per target, not one per add.
const MEASURE_TTL: Duration = Duration::from_secs(5);

/// How to pick one target among the candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// The target holding the fewest torrents.
    Count,
    /// The target holding the fewest bytes (the sum of its torrents' sizes).
    #[default]
    Bytes,
    /// The first target, in order, that is above the reserve.
    Fill,
    /// The target whose filesystem has the most free space at the category's
    /// folder.
    FreeSpace,
}

impl Strategy {
    pub fn parse(s: &str) -> Option<Strategy> {
        match s.trim().to_ascii_lowercase().as_str() {
            "count" => Some(Strategy::Count),
            "bytes" => Some(Strategy::Bytes),
            "fill" => Some(Strategy::Fill),
            "free_space" => Some(Strategy::FreeSpace),
            _ => None,
        }
    }
}

/// Where a torrent may be sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Target {
    Local(String),
    Remote { node: String, engine: String },
}

impl Target {
    /// The string a category stores, and the API shows.
    pub fn label(&self) -> String {
        match self {
            Target::Local(e) => e.clone(),
            Target::Remote { node, engine } => format!("{node}:{engine}"),
        }
    }
}

/// What was measured on one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Measure {
    pub torrents: u64,
    pub bytes: u64,
    /// Free bytes at the category's folder, when the filesystem could be asked.
    pub free: Option<u64>,
}

/// A target with its measure; `None` when it could not be measured (a node that
/// did not answer, an older node without the route).
#[derive(Debug, Clone)]
pub struct Candidate {
    pub target: Target,
    pub measure: Option<Measure>,
}

/// Pick one candidate. Pure: no disk, no network.
///
/// A target below the reserve is out, whatever the strategy. One that could not
/// be measured stays in -- a failed measurement is not a full disk -- but ranks
/// last wherever a measure orders the choice. Ties go to the first in order,
/// which is the order of the category's `targets`. When every target is out the
/// answer is an error and the add is refused: sending it to the fullest disk
/// would be the one wrong thing to do.
pub fn choose(strategy: Strategy, reserve: u64, cands: &[Candidate]) -> Result<usize, String> {
    let eligible: Vec<usize> = (0..cands.len())
        .filter(|&i| {
            reserve == 0
                || match cands[i].measure.and_then(|m| m.free) {
                    Some(free) => free >= reserve,
                    None => true,
                }
        })
        .collect();
    if eligible.is_empty() {
        let names: Vec<String> = cands.iter().map(|c| c.target.label()).collect();
        return Err(format!(
            "no target can take it: every target ({}) has less than {reserve} bytes free at the category's folder",
            names.join(", ")
        ));
    }
    let key = |i: usize| -> (u8, u64) {
        let m = cands[i].measure;
        match strategy {
            Strategy::Fill => (0, 0),
            Strategy::Count => match m {
                Some(m) => (0, m.torrents),
                None => (1, 0),
            },
            Strategy::Bytes => match m {
                Some(m) => (0, m.bytes),
                None => (1, 0),
            },
            Strategy::FreeSpace => match m.and_then(|m| m.free) {
                Some(f) => (0, u64::MAX - f),
                None => (1, 0),
            },
        }
    };
    // `min_by_key` keeps the first of several equal minima: the tie rule.
    Ok(eligible
        .into_iter()
        .min_by_key(|&i| key(i))
        .expect("eligible is not empty"))
}

/// Read a target string. A local engine id wins; otherwise `node:engine`, the
/// node being the longest known name that prefixes it (a node name may itself
/// hold a colon).
pub fn parse_target(raw: &str, local_ids: &[String], nodes: &[String]) -> Result<Target, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("a target is empty".into());
    }
    if local_ids.iter().any(|e| e == raw) {
        return Ok(Target::Local(raw.to_string()));
    }
    let node = nodes
        .iter()
        .filter(|n| {
            raw.strip_prefix(n.as_str())
                .and_then(|r| r.strip_prefix(':'))
                .is_some_and(|e| !e.is_empty())
        })
        .max_by_key(|n| n.len());
    if let Some(n) = node {
        let engine = &raw[n.len() + 1..];
        return Ok(Target::Remote {
            node: n.clone(),
            engine: engine.to_string(),
        });
    }
    if raw.contains(':') {
        Err(format!("target {raw:?}: no node is named like that"))
    } else {
        Err(format!("target {raw:?}: no local engine has that id"))
    }
}

/// The measures, kept a few seconds. Keyed by (target, folder-or-category):
/// a remote target is measured by category, the node resolving the folder.
#[derive(Default)]
pub struct SpreadCache {
    map: Mutex<HashMap<CacheKey, (Instant, Option<Measure>)>>,
}

/// (target or filesystem, folder or category).
type CacheKey = (String, String);

impl SpreadCache {
    fn get(&self, key: &CacheKey) -> Option<Option<Measure>> {
        let map = self.map.lock().unwrap();
        map.get(key)
            .filter(|(at, _)| at.elapsed() < MEASURE_TTL)
            .map(|(_, m)| *m)
    }

    fn put(&self, key: CacheKey, m: Option<Measure>) {
        self.map.lock().unwrap().insert(key, (Instant::now(), m));
    }

    /// A torrent was just sent here: count it now, so the next add of a burst
    /// sees it instead of the figure taken before the burst began.
    fn bump(&self, key: &CacheKey, size: u64) {
        if let Some((_, Some(m))) = self.map.lock().unwrap().get_mut(key) {
            m.torrents += 1;
            m.bytes = m.bytes.saturating_add(size);
            m.free = m.free.map(|f| f.saturating_sub(size));
        }
    }
}

/// What a category asks of an add.
#[derive(Debug, Clone, Default)]
pub struct Rule {
    pub hoard: bool,
    pub targets: Vec<String>,
    pub strategy: Strategy,
    pub reserve: u64,
    pub save_path: String,
}

/// A rule resolved against the engines and nodes that exist now.
pub struct Plan {
    pub category: String,
    pub cands: Vec<Target>,
    pub strategy: Strategy,
    pub reserve: u64,
    pub save_path: String,
}

/// Which targets a rule means right now.
///
/// `Ok(None)` is "no spreading applies": the category is unknown, or no local
/// engine has the role (the legacy route then answers, as it always did). An
/// error is a category whose every configured target is gone or out of reach.
pub fn plan(
    state: &AppState,
    category: &str,
    rule: &Rule,
    allow_remote: bool,
) -> Result<Option<Plan>, String> {
    let engines = state.engines.engines();
    let role = if rule.hoard { "hoard" } else { "race" };
    let cands: Vec<Target> = if rule.targets.is_empty() {
        engines
            .iter()
            .filter(|e| e.role == role)
            .map(|e| Target::Local(e.id.clone()))
            .collect()
    } else {
        let local_ids: Vec<String> = engines.iter().map(|e| e.id.clone()).collect();
        let nodes: Vec<crate::store::Node> =
            state.store.lock().unwrap().nodes().unwrap_or_default();
        let names: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
        let mut out = Vec::new();
        for raw in &rule.targets {
            match parse_target(raw, &local_ids, &names) {
                Ok(Target::Remote { node, engine }) => {
                    let enabled = nodes.iter().any(|n| n.name == node && n.enabled);
                    if !enabled {
                        tracing::warn!(category, target = %raw, "spread: the node is disabled, target ignored");
                    } else if !allow_remote {
                        tracing::debug!(category, target = %raw, "spread: this add cannot reach a node, target skipped");
                    } else {
                        out.push(Target::Remote { node, engine });
                    }
                }
                Ok(t) => out.push(t),
                Err(e) => tracing::warn!(
                    category,
                    "spread: a target is no longer valid, ignored: {e}"
                ),
            }
        }
        if out.is_empty() {
            return Err(format!(
                "category {category:?} has no usable target: every one of its targets is unknown{}",
                if allow_remote {
                    ""
                } else {
                    " or on a node, which this way of adding cannot reach"
                }
            ));
        }
        out
    };
    if cands.is_empty() {
        return Ok(None);
    }
    Ok(Some(Plan {
        category: category.to_string(),
        cands,
        strategy: rule.strategy,
        reserve: rule.reserve,
        save_path: rule.save_path.clone(),
    }))
}

/// Count, bytes and free space of one local engine at `save_path`.
pub fn measure_local(state: &AppState, engine_id: &str, save_path: &str) -> Option<Measure> {
    let engine = state.engines.get(engine_id)?;
    let mut bytes = 0u64;
    engine
        .manager
        .for_each(|t| bytes = bytes.saturating_add(t.meta.total_size));
    let free = if save_path.is_empty() {
        None
    } else {
        crate::jobs::free_space_near(std::path::Path::new(save_path))
    };
    Some(Measure {
        torrents: engine.manager.len() as u64,
        bytes,
        free,
    })
}

fn local_key(plan: &Plan, engine: &str) -> CacheKey {
    (engine.to_string(), plan.save_path.clone())
}

/// The filesystem's figure is kept apart from the engines': every engine of a
/// node files into the same folder, so they must see one number. Read apart,
/// two engines would differ by whatever another process wrote between the two
/// reads, and `free_space` would rank them on that noise instead of tying.
fn fs_key(plan: &Plan) -> CacheKey {
    ("\0filesystem".to_string(), plan.save_path.clone())
}

fn remote_key(plan: &Plan, t: &Target) -> CacheKey {
    (t.label(), plan.category.clone())
}

fn local_measured(state: &AppState, plan: &Plan, engine: &str) -> Option<Measure> {
    let key = local_key(plan, engine);
    let held = match state.spread.get(&key) {
        Some(m) => m,
        None => {
            let m = measure_local(state, engine, "");
            state.spread.put(key, m);
            m
        }
    };
    let fs = fs_key(plan);
    let free = match state.spread.get(&fs) {
        Some(m) => m.and_then(|m| m.free),
        None => {
            let free = if plan.save_path.is_empty() {
                None
            } else {
                crate::jobs::free_space_near(std::path::Path::new(&plan.save_path))
            };
            state.spread.put(
                fs,
                Some(Measure {
                    free,
                    ..Default::default()
                }),
            );
            free
        }
    };
    held.map(|m| Measure { free, ..m })
}

fn conclude(
    state: &AppState,
    plan: &Plan,
    measured: Vec<Candidate>,
    size: u64,
) -> Result<Target, String> {
    let i = choose(plan.strategy, plan.reserve, &measured)?;
    let target = measured[i].target.clone();
    match &target {
        Target::Local(e) => {
            state.spread.bump(&local_key(plan, e), size);
            state.spread.bump(&fs_key(plan), size);
        }
        t => state.spread.bump(&remote_key(plan, t), size),
    }
    Ok(target)
}

/// The local engine a new torrent of `category` goes to, among local targets
/// only. `Ok(None)`: nothing to spread, the legacy route answers. For the
/// callers that cannot follow a torrent to a node (the qBittorrent shim lists
/// local engines only, a client that adds then polls the hash would lose it).
pub fn resolve_local(
    state: &AppState,
    category: &str,
    size: u64,
) -> Result<Option<String>, String> {
    let Some(rule) = crate::api::category_rule(state, category) else {
        return Ok(None);
    };
    let Some(plan) = plan(state, category, &rule, false)? else {
        return Ok(None);
    };
    // One candidate and no reserve: nothing to measure, nothing to compare.
    if let ([Target::Local(e)], 0) = (plan.cands.as_slice(), plan.reserve) {
        return Ok(Some(e.clone()));
    }
    let measured: Vec<Candidate> = plan
        .cands
        .iter()
        .map(|t| Candidate {
            target: t.clone(),
            measure: match t {
                Target::Local(e) => local_measured(state, &plan, e),
                Target::Remote { .. } => None,
            },
        })
        .collect();
    match conclude(state, &plan, measured, size)? {
        Target::Local(e) => Ok(Some(e)),
        Target::Remote { .. } => unreachable!("a local plan has no remote candidate"),
    }
}

/// Where an add goes: here (an engine id), or a node's engine.
#[derive(Debug, Clone)]
pub enum Chosen {
    /// `None`: nothing to spread, the legacy route decides.
    Local(Option<String>),
    Remote {
        node: crate::store::Node,
        engine: String,
    },
}

/// `resolve_local`, with nodes' engines among the candidates.
pub async fn resolve(state: &AppState, category: &str, size: u64) -> Result<Chosen, String> {
    let Some(rule) = crate::api::category_rule(state, category) else {
        return Ok(Chosen::Local(None));
    };
    let Some(plan) = plan(state, category, &rule, true)? else {
        return Ok(Chosen::Local(None));
    };
    if let ([Target::Local(e)], 0) = (plan.cands.as_slice(), plan.reserve) {
        return Ok(Chosen::Local(Some(e.clone())));
    }
    let nodes: Vec<crate::store::Node> = state.store.lock().unwrap().nodes().unwrap_or_default();

    // Locals are measured in place; nodes concurrently, so two dead nodes cost
    // one timeout. No lock is held across the awaits.
    let remote = futures::future::join_all(plan.cands.iter().map(|t| {
        let nodes = &nodes;
        let plan = &plan;
        async move {
            match t {
                Target::Local(e) => (t.clone(), local_measured(state, plan, e)),
                Target::Remote { node, engine } => {
                    let key = remote_key(plan, t);
                    if let Some(m) = state.spread.get(&key) {
                        return (t.clone(), m);
                    }
                    let n = nodes.iter().find(|n| &n.name == node);
                    let m = match n {
                        Some(n) => fetch_remote(n, engine, &plan.category).await,
                        None => None,
                    };
                    if m.is_none() {
                        tracing::warn!(node = %node, "spread: the node could not be measured, it stays a candidate and ranks last");
                    }
                    state.spread.put(key, m);
                    (t.clone(), m)
                }
            }
        }
    }))
    .await;
    let measured: Vec<Candidate> = remote
        .into_iter()
        .map(|(target, measure)| Candidate { target, measure })
        .collect();
    match conclude(state, &plan, measured, size)? {
        Target::Local(e) => Ok(Chosen::Local(Some(e))),
        Target::Remote { node, engine } => {
            let node = nodes
                .into_iter()
                .find(|n| n.name == node)
                .ok_or("the node was removed while choosing")?;
            Ok(Chosen::Remote { node, engine })
        }
    }
}

/// Ask a node for an engine's measure. `None` for anything but a clean answer.
async fn fetch_remote(node: &crate::store::Node, engine: &str, category: &str) -> Option<Measure> {
    let mut q = reqwest::Url::parse("http://x/").ok()?;
    q.query_pairs_mut().append_pair("category", category);
    let mut id = reqwest::Url::parse("http://x/").ok()?;
    id.path_segments_mut().ok()?.push(engine);
    let path = format!(
        "api/engines{}/measure?{}",
        id.path(),
        q.query().unwrap_or_default()
    );
    let (status, body, _) = crate::nodes::forward(
        &node.url,
        &node.api_key,
        reqwest::Method::GET,
        &path,
        Vec::new(),
    )
    .await
    .ok()?;
    if !status.is_success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
    Some(Measure {
        torrents: v.get("torrents")?.as_u64()?,
        bytes: v.get("bytes")?.as_u64()?,
        free: v.get("free_bytes").and_then(|f| f.as_u64()),
    })
}

/// The engines of a node, `(id, role)`, or why they could not be read.
pub async fn node_engines(node: &crate::store::Node) -> Result<Vec<(String, String)>, String> {
    let (status, body, _) = crate::nodes::forward(
        &node.url,
        &node.api_key,
        reqwest::Method::GET,
        "api/engines",
        Vec::new(),
    )
    .await
    .map_err(|e| format!("the node {} did not answer ({e})", node.name))?;
    if !status.is_success() {
        return Err(format!(
            "the node {} answered HTTP {}",
            node.name,
            status.as_u16()
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|_| {
        format!(
            "the node {} sent an engine list that is not JSON",
            node.name
        )
    })?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some((
                        e.get("id")?.as_str()?.to_string(),
                        e.get("role")
                            .and_then(|r| r.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Check a category's `targets` at registration: every one must exist, and its
/// role must be the category's mode. Refused here, not discovered at the first
/// add.
pub async fn validate_targets(
    state: &AppState,
    mode: &str,
    targets: &[String],
) -> Result<(), String> {
    let local: Vec<(String, String)> = state
        .engines
        .engines()
        .iter()
        .map(|e| (e.id.clone(), e.role.clone()))
        .collect();
    let local_ids: Vec<String> = local.iter().map(|(i, _)| i.clone()).collect();
    let nodes: Vec<crate::store::Node> = state.store.lock().unwrap().nodes().unwrap_or_default();
    let names: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
    let mut seen = std::collections::HashSet::new();
    for raw in targets {
        let t = parse_target(raw, &local_ids, &names)?;
        if !seen.insert(t.clone()) {
            return Err(format!("target {raw:?} is listed twice"));
        }
        let (role, what) = match &t {
            Target::Local(id) => (
                local
                    .iter()
                    .find(|(i, _)| i == id)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_default(),
                format!("engine {id}"),
            ),
            Target::Remote { node, engine } => {
                let n = nodes.iter().find(|n| &n.name == node).expect("parsed");
                let list = node_engines(n).await?;
                let Some((_, role)) = list.into_iter().find(|(i, _)| i == engine) else {
                    return Err(format!("the node {node} has no engine {engine}"));
                };
                if role.is_empty() {
                    return Err(format!(
                        "the node {node} does not report the role of {engine}; update it"
                    ));
                }
                (role, format!("engine {engine} of node {node}"))
            }
        };
        if role != mode {
            return Err(format!(
                "target {raw:?}: {what} has the role {role}, but the category's mode is {mode}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, m: Option<(u64, u64, Option<u64>)>) -> Candidate {
        Candidate {
            target: Target::Local(name.into()),
            measure: m.map(|(torrents, bytes, free)| Measure {
                torrents,
                bytes,
                free,
            }),
        }
    }

    #[test]
    fn count_takes_the_target_with_the_fewest_torrents() {
        let v = [
            c("a", Some((9, 1, None))),
            c("b", Some((2, 900, None))),
            c("c", Some((5, 5, None))),
        ];
        assert_eq!(choose(Strategy::Count, 0, &v), Ok(1));
    }

    #[test]
    fn bytes_takes_the_target_with_the_fewest_bytes_whatever_the_count() {
        let v = [
            c("a", Some((9, 1, None))),
            c("b", Some((2, 900, None))),
            c("c", Some((5, 5, None))),
        ];
        assert_eq!(choose(Strategy::Bytes, 0, &v), Ok(0));
    }

    #[test]
    fn fill_takes_the_first_target_and_moves_on_only_below_the_reserve() {
        let v = [
            c("a", Some((0, 0, Some(10)))),
            c("b", Some((0, 0, Some(500)))),
        ];
        assert_eq!(choose(Strategy::Fill, 0, &v), Ok(0));
        assert_eq!(choose(Strategy::Fill, 100, &v), Ok(1));
    }

    #[test]
    fn free_space_takes_the_target_with_the_most_room() {
        let v = [
            c("a", Some((0, 0, Some(10)))),
            c("b", Some((0, 0, Some(500)))),
            c("c", Some((0, 0, Some(70)))),
        ];
        assert_eq!(choose(Strategy::FreeSpace, 0, &v), Ok(1));
    }

    #[test]
    fn a_tie_goes_to_the_first_in_order() {
        let v = [c("a", Some((3, 3, Some(9)))), c("b", Some((3, 3, Some(9))))];
        for s in [Strategy::Count, Strategy::Bytes, Strategy::FreeSpace] {
            assert_eq!(choose(s, 0, &v), Ok(0), "{s:?}");
        }
    }

    #[test]
    fn the_reserve_drops_a_target_whatever_the_strategy() {
        // `a` would win count, bytes and fill, but has 10 free against a
        // reserve of 100; `b` and `c` are above it.
        let v = [
            c("a", Some((0, 0, Some(10)))),
            c("b", Some((8, 800, Some(140)))),
            c("c", Some((9, 900, Some(150)))),
        ];
        for s in [Strategy::Count, Strategy::Bytes, Strategy::Fill] {
            assert_eq!(choose(s, 100, &v), Ok(1), "{s:?}");
        }
        assert_eq!(choose(Strategy::FreeSpace, 100, &v), Ok(2));
    }

    #[test]
    fn every_target_below_the_reserve_refuses_the_add() {
        let v = [
            c("a", Some((0, 0, Some(10)))),
            c("b", Some((0, 0, Some(20)))),
        ];
        let e = choose(Strategy::Bytes, 100, &v).unwrap_err();
        assert!(e.contains("no target can take it"), "{e}");
        assert!(e.contains("a, b"), "{e}");
    }

    #[test]
    fn an_unmeasurable_target_stays_a_candidate_and_ranks_last() {
        // Alone it is picked: not being able to measure is not a full disk.
        let alone = [c("n", None)];
        assert_eq!(choose(Strategy::FreeSpace, 100, &alone), Ok(0));
        // Beside a measured one it loses on every measured ranking...
        let v = [c("n", None), c("m", Some((50, 50, Some(1000))))];
        for s in [Strategy::Count, Strategy::Bytes, Strategy::FreeSpace] {
            assert_eq!(choose(s, 0, &v), Ok(1), "{s:?}");
        }
        // ...but survives a reserve that the measured one fails.
        let v = [c("n", None), c("m", Some((0, 0, Some(5))))];
        assert_eq!(choose(Strategy::FreeSpace, 100, &v), Ok(0));
    }

    #[test]
    fn a_target_is_read_as_an_engine_or_as_node_colon_engine() {
        let local = vec!["race".to_string(), "vpn1".to_string()];
        let nodes = vec!["nas".to_string(), "nas:b".to_string()];
        assert_eq!(
            parse_target("vpn1", &local, &nodes),
            Ok(Target::Local("vpn1".into()))
        );
        assert_eq!(
            parse_target(" nas:vpn1 ", &local, &nodes),
            Ok(Target::Remote {
                node: "nas".into(),
                engine: "vpn1".into()
            })
        );
        // The longest node name wins.
        assert_eq!(
            parse_target("nas:b:vpn2", &local, &nodes),
            Ok(Target::Remote {
                node: "nas:b".into(),
                engine: "vpn2".into()
            })
        );
        assert!(parse_target("vpn9", &local, &nodes).is_err());
        assert!(parse_target("other:vpn1", &local, &nodes).is_err());
        assert!(parse_target("nas:", &local, &nodes).is_err());
        assert!(parse_target("", &local, &nodes).is_err());
    }

    #[test]
    fn a_strategy_name_is_read_case_insensitively_and_nothing_else_is() {
        assert_eq!(Strategy::parse("Free_Space"), Some(Strategy::FreeSpace));
        assert_eq!(Strategy::parse("least_load"), None);
        assert_eq!(Strategy::parse(""), None);
        assert_eq!(Strategy::default(), Strategy::Bytes);
    }

    #[test]
    fn the_cache_counts_what_was_just_sent_and_forgets_after_its_ttl() {
        let cache = SpreadCache::default();
        let key = ("vpn1".to_string(), "/d".to_string());
        cache.put(
            key.clone(),
            Some(Measure {
                torrents: 1,
                bytes: 100,
                free: Some(1000),
            }),
        );
        cache.bump(&key, 40);
        assert_eq!(
            cache.get(&key),
            Some(Some(Measure {
                torrents: 2,
                bytes: 140,
                free: Some(960)
            }))
        );
        cache.map.lock().unwrap().get_mut(&key).unwrap().0 =
            Instant::now() - MEASURE_TTL - Duration::from_secs(1);
        assert_eq!(cache.get(&key), None);
    }
}
