//! A selection, however it was made, and every action that takes one.
//!
//! A selection is either rows the operator picked (`items`) or a FILTER with
//! exceptions (`filter` + `exclude`) -- what Ctrl+A means. The second used to
//! be turned into hashes by the browser: a million of them fetched from the
//! daemon, held in the page, and sent back in chunks, 43 MB each way to say
//! "the ones I am looking at". Now the filter travels, and the daemon resolves
//! it with the very function that answers the list page, so the set acted on
//! and the set shown cannot be two definitions that drift.
//!
//! ⚠⚠ The last time a filter crossed this API (2026-09-16), the endpoint had
//! no such field, serde dropped it, and the empty hash list that remained
//! meant the whole engine: 293k torrents started instead of 70k. Everything
//! here is built against that:
//! - `deny_unknown_fields` on every body: a key this code does not implement
//!   is a 400 that names it, never a silent default;
//! - an unknown filter parameter is refused the same way;
//! - an empty selection is a refusal, never "everything" -- `filter: ""` is
//!   the only way to say "the whole view", and it has to be said;
//! - a filter comes with `expect`, the count the operator confirmed, and a
//!   selection that resolves to MORE than that is refused (409) with the new
//!   count. Fewer is fine: nothing unconfirmed is touched.
//!
//! Actions run as a background job, one per request, because a million of
//! anything outlives an HTTP request. Each torrent goes through the same
//! `/api` route the browser used to call once per row, in-process: the rules
//! of each action (hardlink consent, reannounce cooldown, the agent a copy
//! lives on) stay in one place.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::AppState;

/// The list page's filter parameters, and nothing else: paging, sorting and
/// `fields` belong to a page, not to a set.
pub const FILTER_KEYS: &[&str] = &[
    "search",
    "category",
    "category_not",
    "tag",
    "tag_not",
    "tracker",
    "tracker_not",
    "error_class",
    "error_class_not",
    "state",
];

/// Errors kept per job. The count is exact; the list is for reading.
const ERRORS_KEPT: usize = 100;

/// How long a finished job stays readable.
const JOB_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct Item {
    pub hash: String,
    /// `local-<engine>` for a copy on this node, `<node>-<engine>` for one on
    /// another. Empty means this node.
    #[serde(default)]
    pub agent: String,
    /// The table the row came from, `hoard` or `race`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub items: Vec<Item>,
    /// The list page's query string, e.g. `category=movies&tracker=calewood`.
    #[serde(default)]
    pub filter: Option<String>,
    /// The list the filter was typed on. `hoard` when absent.
    #[serde(default)]
    pub view: Option<String>,
    #[serde(default)]
    pub exclude: Vec<Item>,
    /// How many torrents the operator was told the filter matched.
    #[serde(default)]
    pub expect: Option<usize>,
}

/// One copy an action applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub hash: String,
    pub agent: String,
    pub mode: String,
}

impl Target {
    pub fn is_local(&self) -> bool {
        is_local(&self.agent)
    }

    /// The engine this copy sits in, on whichever node holds it.
    pub fn engine(&self) -> String {
        match self.agent.strip_prefix("local-") {
            Some(e) => e.to_string(),
            None if self.is_local() => self.mode.clone(),
            // Only a guess without the node list; `split_remote` is the
            // authority for a remote row.
            None => self
                .agent
                .rsplit_once('-')
                .map(|(_, e)| e.to_string())
                .unwrap_or_default(),
        }
    }
}

fn is_local(agent: &str) -> bool {
    agent.is_empty() || agent == "local" || agent.starts_with("local-")
}

/// A refusal before any work: what was wrong, as JSON.
#[derive(Debug)]
pub struct Refusal {
    pub status: StatusCode,
    pub body: Value,
}

impl Refusal {
    fn bad(msg: impl Into<String>) -> Self {
        Refusal {
            status: StatusCode::BAD_REQUEST,
            body: json!({"error": msg.into()}),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

fn clean_hash(h: &str) -> Option<String> {
    let h = h.trim().to_ascii_lowercase();
    ((h.len() == 40 || h.len() == 64) && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// Refuse any parameter the list page would not read as a filter.
pub fn check_filter(filter: &str) -> Result<String, Refusal> {
    let f = filter.trim().trim_start_matches(['?', '&']);
    for pair in f.split('&').filter(|p| !p.is_empty()) {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        if !FILTER_KEYS.contains(&key) {
            return Err(Refusal::bad(format!(
                "`{key}` is not a filter; a selection filter takes only {}",
                FILTER_KEYS.join(", ")
            )));
        }
    }
    Ok(f.to_string())
}

/// Turn a selection into the copies it names.
pub async fn resolve(state: &AppState, sel: &Selection) -> Result<Vec<Target>, Refusal> {
    let view = sel.view.clone().unwrap_or_else(|| "hoard".into());
    let view_role = match state.engines.get(&view) {
        Some(e) => e.role.clone(),
        None => return Err(Refusal::bad(format!("no engine `{view}` on this node"))),
    };
    let norm = |agent: &str| -> String {
        if agent.is_empty() || agent == "local" {
            format!("local-{view}")
        } else {
            agent.to_string()
        }
    };

    match (&sel.filter, sel.items.is_empty()) {
        (Some(_), false) => Err(Refusal::bad("a selection is `items` or a `filter`, not both")),
        (None, true) => Err(Refusal::bad(
            "empty selection; name torrents in `items`, or send `filter` (\"\" is the whole list) with `expect`",
        )),
        (None, false) => {
            let mut seen = HashSet::new();
            let mut out = Vec::with_capacity(sel.items.len());
            for it in &sel.items {
                let hash = clean_hash(&it.hash)
                    .ok_or_else(|| Refusal::bad(format!("`{}` is not an info hash", it.hash)))?;
                let agent = norm(&it.agent);
                let mode = if it.mode.is_empty() { view_role.clone() } else { it.mode.clone() };
                if seen.insert((hash.clone(), agent.clone())) {
                    out.push(Target { hash, agent, mode });
                }
            }
            Ok(out)
        }
        (Some(filter), true) => {
            let filter = check_filter(filter)?;
            let Some(expect) = sel.expect else {
                return Err(Refusal::bad(
                    "a filter needs `expect`, the count the operator confirmed",
                ));
            };
            let query = if filter.is_empty() { "fields=hash".to_string() } else { format!("fields=hash&{filter}") };
            let page = crate::api::fleet_page(state, &view, &query).await;
            let excluded: HashSet<(String, String)> = sel
                .exclude
                .iter()
                .filter_map(|it| clean_hash(&it.hash).map(|h| (h, norm(&it.agent))))
                .collect();
            let mut out = Vec::new();
            let mut push = |hash: &str, agent: String| {
                if let Some(hash) = clean_hash(hash) {
                    if !excluded.contains(&(hash.clone(), agent.clone())) {
                        out.push(Target { hash, agent, mode: view_role.clone() });
                    }
                }
            };
            match page.get("copies").and_then(Value::as_array) {
                Some(copies) => {
                    for c in copies {
                        let h = c.get("hash").and_then(Value::as_str).unwrap_or_default();
                        let a = c.get("agent").and_then(Value::as_str).unwrap_or_default();
                        push(h, norm(a));
                    }
                }
                None => {
                    let engine = page.get("engine").and_then(Value::as_str).unwrap_or(&view).to_string();
                    for h in page.get("hashes").and_then(Value::as_array).into_iter().flatten() {
                        push(h.as_str().unwrap_or_default(), format!("local-{engine}"));
                    }
                }
            }
            if out.len() > expect {
                return Err(Refusal {
                    status: StatusCode::CONFLICT,
                    body: json!({
                        "error": format!("the filter now matches {} torrents, {expect} were confirmed", out.len()),
                        "reason": "grew",
                        "count": out.len(),
                        "expected": expect,
                    }),
                });
            }
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Action {
    Pause(bool),
    Pin(bool),
    Tags {
        tags: Vec<String>,
        op: String,
    },
    Category {
        category: String,
        move_files: bool,
        allow_breaking_hardlinks: bool,
    },
    Location {
        location: String,
        allow_breaking_hardlinks: bool,
    },
    Reannounce,
    Recheck,
    Remove {
        delete_files: bool,
    },
    Copy {
        engine: String,
    },
    MoveEngine {
        engine: String,
    },
    Handoff {
        node: String,
        engine: String,
        then: String,
    },
    NodeFetch {
        node: String,
        from_engine: String,
        engine: String,
    },
    NodeMove {
        node: String,
        engine: String,
    },
    /// Rate caps, KiB/s; `None` keeps a direction, 0 lifts it.
    Limits {
        up_kib: Option<i64>,
        down_kib: Option<i64>,
    },
    /// Share limits, `setShareLimits`' encoding (-2 follow the engine, -1
    /// none, minutes); `None` keeps a field.
    ShareLimits {
        ratio: Option<f64>,
        seeding: Option<i64>,
        inactive: Option<i64>,
    },
    /// Download order flags; `None` keeps one. File priorities are per
    /// torrent (file ids mean nothing across a selection).
    DownloadOrder {
        sequential: Option<bool>,
        first_last_piece: Option<bool>,
    },
}

fn params<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, Refusal> {
    let v = if v.is_null() { json!({}) } else { v.clone() };
    serde_json::from_value(v).map_err(|e| Refusal::bad(format!("params: {e}")))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagParams {
    tags: Vec<String>,
    op: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LocationParams {
    location: String,
    #[serde(default)]
    allow_breaking_hardlinks: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CategoryParams {
    category: String,
    #[serde(default)]
    move_files: bool,
    #[serde(default)]
    allow_breaking_hardlinks: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveParams {
    #[serde(default)]
    delete_files: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineParams {
    engine: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffParams {
    node: String,
    engine: String,
    then: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchParams {
    node: String,
    #[serde(default)]
    from_engine: String,
    engine: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitParams {
    #[serde(default)]
    up_kib: Option<i64>,
    #[serde(default)]
    down_kib: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareLimitParams {
    #[serde(default)]
    ratio_limit: Option<f64>,
    #[serde(default)]
    seeding_time_limit: Option<i64>,
    #[serde(default)]
    inactive_seeding_time_limit: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadOrderParams {
    #[serde(default)]
    sequential: Option<bool>,
    #[serde(default)]
    first_last_piece: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NodeMoveParams {
    node: String,
    engine: String,
}

impl Action {
    pub fn parse(name: &str, p: &Value) -> Result<Action, Refusal> {
        Ok(match name {
            "stop" | "start" => {
                params::<NoParams>(p)?;
                Action::Pause(name == "stop")
            }
            "pin" | "unpin" => {
                params::<NoParams>(p)?;
                Action::Pin(name == "pin")
            }
            "tags" => {
                let t: TagParams = params(p)?;
                if t.op != "add" && t.op != "remove" {
                    return Err(Refusal::bad("params.op must be \"add\" or \"remove\""));
                }
                if t.tags.iter().all(|x| x.trim().is_empty()) {
                    return Err(Refusal::bad("params.tags is empty"));
                }
                Action::Tags {
                    tags: t.tags,
                    op: t.op,
                }
            }
            "category" => {
                let c: CategoryParams = params(p)?;
                Action::Category {
                    category: c.category,
                    move_files: c.move_files,
                    allow_breaking_hardlinks: c.allow_breaking_hardlinks,
                }
            }
            "location" => {
                let l: LocationParams = params(p)?;
                if l.location.trim().is_empty() {
                    return Err(Refusal::bad("params.location is empty"));
                }
                Action::Location {
                    location: l.location,
                    allow_breaking_hardlinks: l.allow_breaking_hardlinks,
                }
            }
            "reannounce" => {
                params::<NoParams>(p)?;
                Action::Reannounce
            }
            "recheck" => {
                params::<NoParams>(p)?;
                Action::Recheck
            }
            "remove" => Action::Remove {
                delete_files: params::<RemoveParams>(p)?.delete_files,
            },
            "copy" => Action::Copy {
                engine: params::<EngineParams>(p)?.engine,
            },
            "move-engine" => Action::MoveEngine {
                engine: params::<EngineParams>(p)?.engine,
            },
            "handoff" => {
                let h: HandoffParams = params(p)?;
                if h.then != "keep" && h.then != "remove" {
                    return Err(Refusal::bad("params.then must be \"keep\" or \"remove\""));
                }
                Action::Handoff {
                    node: h.node,
                    engine: h.engine,
                    then: h.then,
                }
            }
            "node-fetch" => {
                let f: FetchParams = params(p)?;
                Action::NodeFetch {
                    node: f.node,
                    from_engine: f.from_engine,
                    engine: f.engine,
                }
            }
            "limits" => {
                let l: LimitParams = params(p)?;
                if l.up_kib.is_none() && l.down_kib.is_none() {
                    return Err(Refusal::bad(
                        "params needs up_kib and/or down_kib (KiB/s, 0 = unlimited)",
                    ));
                }
                Action::Limits {
                    up_kib: l.up_kib,
                    down_kib: l.down_kib,
                }
            }
            "share_limits" => {
                let l: ShareLimitParams = params(p)?;
                if l.ratio_limit.is_none()
                    && l.seeding_time_limit.is_none()
                    && l.inactive_seeding_time_limit.is_none()
                {
                    return Err(Refusal::bad(
                        "params needs ratio_limit, seeding_time_limit and/or inactive_seeding_time_limit \
                         (-2 = the engine's, -1 = none, times in minutes)",
                    ));
                }
                Action::ShareLimits {
                    ratio: l.ratio_limit,
                    seeding: l.seeding_time_limit,
                    inactive: l.inactive_seeding_time_limit,
                }
            }
            "download_order" => {
                let d: DownloadOrderParams = params(p)?;
                if d.sequential.is_none() && d.first_last_piece.is_none() {
                    return Err(Refusal::bad(
                        "params needs sequential and/or first_last_piece",
                    ));
                }
                Action::DownloadOrder {
                    sequential: d.sequential,
                    first_last_piece: d.first_last_piece,
                }
            }
            "node-move" => {
                let m: NodeMoveParams = params(p)?;
                Action::NodeMove {
                    node: m.node,
                    engine: m.engine,
                }
            }
            other => {
                return Err(Refusal {
                    status: StatusCode::NOT_FOUND,
                    body: json!({"error": format!("no selection action `{other}`")}),
                })
            }
        })
    }

    /// How many torrents run at once. One, as the browser did, except where
    /// waiting on each answer in turn was measured to be the whole cost.
    fn concurrency(&self) -> usize {
        match self {
            // The scheduler paces the announces; 8 only keeps the calls moving.
            Action::Reannounce => 8,
            _ => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct Consent {
    pub items: Vec<Item>,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Job {
    pub id: String,
    pub action: String,
    pub total: usize,
    pub done: usize,
    /// Outcome name -> how many torrents ended there.
    pub tally: BTreeMap<String, u64>,
    pub failed: u64,
    pub errors: Vec<String>,
    /// Category moves that need the operator's yes: files hardlinked elsewhere.
    pub consent: Consent,
    pub finished: bool,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    #[serde(skip)]
    started: Option<std::time::Instant>,
    #[serde(skip)]
    ended: Option<std::time::Instant>,
    #[serde(skip)]
    cancel: bool,
}

type Shared = Arc<Mutex<Job>>;

static JOBS: LazyLock<Mutex<HashMap<String, Shared>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn new_job(action: &str, total: usize) -> Shared {
    let id = format!("sel{:012x}", rand::random::<u64>() & 0xFFFF_FFFF_FFFF);
    let job = Arc::new(Mutex::new(Job {
        id: id.clone(),
        action: action.to_string(),
        total,
        started: Some(std::time::Instant::now()),
        ..Default::default()
    }));
    let mut jobs = JOBS.lock().unwrap_or_else(|p| p.into_inner());
    jobs.retain(|_, j| {
        let j = j.lock().unwrap_or_else(|p| p.into_inner());
        j.ended.is_none_or(|e| e.elapsed() <= JOB_TTL)
    });
    jobs.insert(id, job.clone());
    job
}

fn get_job(id: &str) -> Option<Shared> {
    JOBS.lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(id)
        .cloned()
}

/// What one torrent came to.
struct Outcome {
    key: String,
    error: Option<String>,
    consent: Option<(Item, u64, u64)>,
}

impl Outcome {
    fn ok(key: &str) -> Self {
        Outcome {
            key: key.into(),
            error: None,
            consent: None,
        }
    }
    fn skip(key: &str) -> Self {
        Outcome::ok(key)
    }
    fn failed(hash: &str, why: String) -> Self {
        Outcome {
            key: "failed".into(),
            error: Some(format!("{}: {why}", &hash[..hash.len().min(8)])),
            consent: None,
        }
    }
}

fn record(job: &Shared, n: usize, o: Outcome) {
    let mut j = job.lock().unwrap_or_else(|p| p.into_inner());
    j.done += n;
    *j.tally.entry(o.key.clone()).or_default() += n as u64;
    if let Some(e) = o.error {
        j.failed += n as u64;
        if j.errors.len() < ERRORS_KEPT {
            j.errors.push(e);
        }
    }
    if let Some((item, files, bytes)) = o.consent {
        j.consent.items.push(item);
        j.consent.files += files;
        j.consent.bytes += bytes;
    }
}

fn cancelled(job: &Shared) -> bool {
    job.lock().unwrap_or_else(|p| p.into_inner()).cancel
}

/// One `/api` request through the real router, carrying the node's own key.
struct Caller {
    router: Router,
    key: String,
}

impl Caller {
    async fn call(&self, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("X-Api-Key", &self.key);
        let body = match body {
            Some(v) => {
                req = req.header(axum::http::header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(v.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let Ok(req) = req.body(body) else {
            return (
                StatusCode::BAD_REQUEST,
                json!({"error": "unbuildable request"}),
            );
        };
        let resp = match self.router.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20)
            .await
            .unwrap_or_default();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    /// The route answered; say how, or why not.
    async fn simple(
        &self,
        method: Method,
        uri: &str,
        body: Option<Value>,
        hash: &str,
        ok: &str,
    ) -> Outcome {
        let (s, v) = self.call(method, uri, body).await;
        if s.is_success() {
            Outcome::ok(ok)
        } else {
            Outcome::failed(hash, error_of(s, &v))
        }
    }
}

fn error_of(s: StatusCode, v: &Value) -> String {
    v.get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| v.as_str().filter(|x| !x.is_empty()).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {}", s.as_u16()))
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The node and engine a remote row's `agent` names.
///
/// Matched against the declared node names, longest first: `agent` is
/// `<node>-<engine>` and either part may itself contain a hyphen. 4.3 split
/// on the first one, so a node named `nas-2` was looked up as `nas`.
fn split_remote(state: &AppState, agent: &str) -> Option<(crate::store::Node, String)> {
    let nodes = state.store.lock().ok()?.nodes().ok()?;
    nodes
        .into_iter()
        .filter(|n| {
            agent.len() > n.name.len() + 1
                && agent.starts_with(&n.name)
                && agent.as_bytes()[n.name.len()] == b'-'
        })
        .max_by_key(|n| n.name.len())
        .map(|n| {
            let engine = agent[n.name.len() + 1..].to_string();
            (n, engine)
        })
}

/// One remote row's action, sent to the node that holds it, on its own
/// routes for that engine's copy.
///
/// 4.3 sent these to `/api/agents/:id/action`, a stub that answered and did
/// nothing, or to this node's own routes, which acted on this node's copy of
/// the same hash -- or found none.
async fn remote_action(state: &AppState, t: &Target, action: &Action) -> Outcome {
    let h = t.hash.as_str();
    let Some((node, engine)) = split_remote(state, &t.agent) else {
        return Outcome::failed(h, format!("no declared node holds {}", t.agent));
    };
    let mode = if t.mode == "race" { "race" } else { "hoard" };
    let e = enc(&engine);
    let (method, path, body) = match action {
        Action::Category {
            category,
            move_files,
            allow_breaking_hardlinks,
        } => (
            reqwest::Method::POST,
            format!("/api/{mode}/torrents/{h}/category?engine={e}"),
            Some(
                json!({"category": category, "move_files": move_files, "allow_breaking_hardlinks": allow_breaking_hardlinks}),
            ),
        ),
        Action::Location {
            location,
            allow_breaking_hardlinks,
        } => (
            reqwest::Method::POST,
            format!("/api/{mode}/torrents/{h}/location?engine={e}"),
            Some(
                json!({"location": location, "allow_breaking_hardlinks": allow_breaking_hardlinks}),
            ),
        ),
        Action::Reannounce => (
            reqwest::Method::POST,
            format!("/api/torrents/{h}/reannounce?engine={e}"),
            None,
        ),
        Action::Recheck => (
            reqwest::Method::POST,
            format!("/api/hoard/torrents/{h}/verify?engine={e}"),
            None,
        ),
        Action::Pause(p) => (
            reqwest::Method::POST,
            format!("/api/engines/{e}/pause"),
            Some(json!({"hashes": [h], "paused": p})),
        ),
        Action::Limits { up_kib, down_kib } => (
            reqwest::Method::POST,
            format!("/api/torrents/{h}/limits?engine={e}"),
            Some(json!({"up_kib": up_kib, "down_kib": down_kib})),
        ),
        Action::DownloadOrder {
            sequential,
            first_last_piece,
        } => (
            reqwest::Method::POST,
            format!("/api/torrents/{h}/download-order?engine={e}"),
            Some(download_order_body(*sequential, *first_last_piece)),
        ),
        Action::Remove { delete_files } => (
            reqwest::Method::DELETE,
            format!("/api/torrents/{h}?delete_files={delete_files}&engine={e}"),
            None,
        ),
        _ => return Outcome::skip("not_here"),
    };
    let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
    match crate::nodes::forward(&node.url, &node.api_key, method, &path, body).await {
        Ok((status, bytes, _)) if status.is_success() => {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if status == reqwest::StatusCode::ACCEPTED {
                if v.get("kind").and_then(Value::as_str) == Some("graduate") {
                    return Outcome::ok("graduating");
                }
                return Outcome::ok("moving");
            }
            Outcome::ok("ok")
        }
        Ok((status, bytes, _)) => {
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let msg = v
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| status.to_string());
            Outcome::failed(h, format!("{}: {msg}", node.name))
        }
        Err(e) => Outcome::failed(h, format!("{}: {e}", node.name)),
    }
}

/// The route's body, without the flags left as they are: `null` there would
/// not parse as "keep".
fn download_order_body(sequential: Option<bool>, first_last_piece: Option<bool>) -> Value {
    let mut b = serde_json::Map::new();
    if let Some(s) = sequential {
        b.insert("sequential".into(), json!(s));
    }
    if let Some(f) = first_last_piece {
        b.insert("first_last_piece".into(), json!(f));
    }
    Value::Object(b)
}

fn category_of(state: &AppState, hash: &str) -> String {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    store
        .export_rows(&[hash.to_string()])
        .ok()
        .and_then(|r| r.into_iter().next())
        .map(|r| r.category)
        .unwrap_or_default()
}

async fn one(state: &AppState, c: &Caller, action: &Action, t: &Target) -> Outcome {
    let h = t.hash.as_str();
    match action {
        Action::Pause(_) => unreachable!("set-based"),
        Action::Pin(_) | Action::Tags { .. } | Action::ShareLimits { .. } => {
            unreachable!("set-based")
        }
        Action::Category { .. } | Action::Location { .. } => {
            // A row from another node is that node's torrent; the local
            // routes would act on this node's copy of the same hash. Relayed
            // by `remote_action` once nodes can be driven from here.
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            let mode = if t.mode == "race" { "race" } else { "hoard" };
            let (route, body) = match action {
                Action::Category {
                    category,
                    move_files,
                    allow_breaking_hardlinks,
                } => (
                    "category",
                    json!({
                        "category": category,
                        "move_files": move_files,
                        "allow_breaking_hardlinks": allow_breaking_hardlinks,
                    }),
                ),
                Action::Location {
                    location,
                    allow_breaking_hardlinks,
                } => (
                    "location",
                    json!({
                        "location": location,
                        "allow_breaking_hardlinks": allow_breaking_hardlinks,
                    }),
                ),
                _ => unreachable!("matched above"),
            };
            // The copy the row is: an extra engine's row named no engine, and
            // the route fell back to the hoard or race copy of the same hash
            // -- or answered "torrent not found".
            let engine_q = t
                .agent
                .strip_prefix("local-")
                .filter(|e| !e.is_empty())
                .map(|e| format!("?engine={}", enc(e)))
                .unwrap_or_default();
            let (s, v) = c
                .call(
                    Method::POST,
                    &format!("/api/{mode}/torrents/{h}/{route}{engine_q}"),
                    Some(body),
                )
                .await;
            if s == StatusCode::CONFLICT
                && v.get("reason").and_then(Value::as_str) == Some("hardlinks")
            {
                let files = v
                    .get("hardlinked_files")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let bytes = v
                    .get("hardlinked_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let item = Item {
                    hash: t.hash.clone(),
                    agent: t.agent.clone(),
                    mode: t.mode.clone(),
                };
                return Outcome {
                    key: "needs_consent".into(),
                    error: None,
                    consent: Some((item, files, bytes)),
                };
            }
            match s {
                // A graduation changes engine: the torrent stops seeding
                // while its data is copied. Told apart so the UI can say so.
                StatusCode::ACCEPTED
                    if v.get("kind").and_then(Value::as_str) == Some("graduate") =>
                {
                    Outcome::ok("graduating")
                }
                StatusCode::ACCEPTED => Outcome::ok("moving"),
                s if s.is_success() => Outcome::ok("ok"),
                s => Outcome::failed(h, error_of(s, &v)),
            }
        }
        Action::Reannounce => {
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            let (s, v) = c
                .call(Method::POST, &format!("/api/torrents/{h}/reannounce"), None)
                .await;
            let status = v.get("status").and_then(Value::as_str).map(str::to_string);
            match status.as_deref() {
                Some(k @ ("ok" | "in_flight" | "cooldown" | "queued")) => Outcome::ok(k),
                _ if s.is_success() => Outcome::ok("ok"),
                _ => Outcome::failed(h, error_of(s, &v)),
            }
        }
        Action::Recheck => {
            // Every engine's copies: the route checks the copy `engine`
            // names. 4.3 skipped race rows here.
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            c.simple(
                Method::POST,
                &format!("/api/hoard/torrents/{h}/verify?engine={}", enc(&t.engine())),
                None,
                h,
                "ok",
            )
            .await
        }
        Action::Remove { delete_files } => {
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            c.simple(
                Method::DELETE,
                &format!(
                    "/api/torrents/{h}?delete_files={delete_files}&agent={}",
                    enc(&t.agent)
                ),
                None,
                h,
                "ok",
            )
            .await
        }
        Action::Copy { engine } => {
            if !t.is_local() {
                return Outcome::skip("not_here");
            }
            c.simple(
                Method::POST,
                &format!("/api/torrents/{h}/copy"),
                Some(json!({"engine": engine})),
                h,
                "ok",
            )
            .await
        }
        Action::MoveEngine { engine } => {
            if !t.is_local() {
                return Outcome::skip("not_here");
            }
            c.simple(
                Method::POST,
                &format!("/api/torrents/{h}/engine?agent={}", enc(&t.agent)),
                Some(json!({"engine": engine})),
                h,
                "ok",
            )
            .await
        }
        Action::Handoff { node, engine, then } => {
            if !t.is_local() {
                return Outcome::skip("not_here");
            }
            // The category travels with it: the far side routes by category.
            let category = category_of(state, h);
            // A move is not done when the node took the metainfo: the local
            // copy goes once the node holds all of it, a job on the Jobs tab.
            // 4.3 counted it "done" here.
            let key = if then == "remove" {
                "waiting_node"
            } else {
                "ok"
            };
            c.simple(
                Method::POST,
                &format!("/api/nodes/{}/handoff", enc(node)),
                Some(json!({"info_hash": h, "engine": engine, "category": category, "then": then})),
                h,
                key,
            )
            .await
        }
        Action::NodeFetch {
            node,
            from_engine,
            engine,
        } => {
            let from = if from_engine.is_empty() {
                t.engine()
            } else {
                from_engine.clone()
            };
            c.simple(
                Method::POST,
                &format!("/api/nodes/{}/fetch", enc(node)),
                Some(
                    json!({"info_hash": h, "engine": engine, "from_engine": from, "category": ""}),
                ),
                h,
                "ok",
            )
            .await
        }
        Action::Limits { up_kib, down_kib } => {
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            // The copy the row is, like a category move: the same hash in
            // another engine keeps its own caps.
            c.simple(
                Method::POST,
                &format!("/api/torrents/{h}/limits?engine={}", enc(&t.engine())),
                Some(json!({"up_kib": up_kib, "down_kib": down_kib})),
                h,
                "ok",
            )
            .await
        }
        Action::DownloadOrder {
            sequential,
            first_last_piece,
        } => {
            if !t.is_local() {
                return remote_action(state, t, action).await;
            }
            // The copy the row is, like the caps.
            c.simple(
                Method::POST,
                &format!(
                    "/api/torrents/{h}/download-order?engine={}",
                    enc(&t.engine())
                ),
                Some(download_order_body(*sequential, *first_last_piece)),
                h,
                "ok",
            )
            .await
        }
        Action::NodeMove { node, engine } => {
            c.simple(
                Method::POST,
                &format!("/api/nodes/{}/move-engine", enc(node)),
                Some(json!({"info_hash": h, "engine": engine})),
                h,
                "ok",
            )
            .await
        }
    }
}

/// Rows per store transaction in a set-based write, the store's lock released
/// between two. Measured on the bench with the writer's 64 MB page cache:
/// 50 000 rows held the lock 0.6 s, 5 000 rows 0.27 s end to end in the
/// sqlite shell; 10 000 keeps each hold near the 200 ms the store already
/// warns about, and a million rows is a hundred of them.
pub const BULK_TX: usize = 10_000;

/// The store edit a set-based action makes, owned so it can cross into
/// `spawn_blocking`.
#[derive(Clone)]
enum SetEdit {
    Paused(bool),
    Pinned(bool),
    Category(String),
    Tags {
        tags: Vec<String>,
        add: bool,
    },
    /// One row per TORRENT, not per copy: the limits are the torrent's.
    ShareLimits {
        ratio: Option<f64>,
        seeding: Option<i64>,
        inactive: Option<i64>,
    },
}

impl SetEdit {
    /// The actions that are nothing but a write to the store (and, for a
    /// pause, a flag in the engine): these never go torrent by torrent.
    fn of(action: &Action) -> Option<SetEdit> {
        match action {
            Action::Pause(p) => Some(SetEdit::Paused(*p)),
            Action::Pin(on) => Some(SetEdit::Pinned(*on)),
            Action::Tags { tags, op } => Some(SetEdit::Tags {
                tags: tags
                    .iter()
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
                add: op == "add",
            }),
            // A relabel only. Moving files is real work per torrent, and a
            // category of the OTHER mode hands the torrent over: both stay on
            // the per-torrent route, which decides between them.
            Action::Category {
                category,
                move_files: false,
                ..
            } => Some(SetEdit::Category(category.clone())),
            Action::ShareLimits {
                ratio,
                seeding,
                inactive,
            } => Some(SetEdit::ShareLimits {
                ratio: *ratio,
                seeding: *seeding,
                inactive: *inactive,
            }),
            _ => None,
        }
    }

    /// Whether this copy is one the edit applies to, as the single routes decide.
    fn applies(&self, t: &Target) -> bool {
        match self {
            SetEdit::Pinned(_) | SetEdit::Tags { .. } => t.mode == "hoard",
            SetEdit::Paused(_) | SetEdit::Category(_) | SetEdit::ShareLimits { .. } => true,
        }
    }
}

/// A store-only action on the whole selection: a few transactions, not one
/// request per torrent. Copies on other nodes go through the agent relay for
/// a pause, and are reported as not here for the rest, as before.
async fn set_based(state: &AppState, _c: &Caller, job: &Shared, edit: SetEdit, targets: &[Target]) {
    let mut local: Vec<(String, String)> = Vec::with_capacity(targets.len());
    for t in targets {
        if !edit.applies(t) {
            record(job, 1, Outcome::skip("skipped"));
        } else if t.is_local() {
            local.push((t.hash.clone(), t.engine()));
        } else if let SetEdit::Paused(p) = edit {
            if cancelled(job) {
                return;
            }
            let o = remote_action(state, t, &Action::Pause(p)).await;
            record(job, 1, o);
        } else if let SetEdit::Category(cat) = &edit {
            if cancelled(job) {
                return;
            }
            let relabel = Action::Category {
                category: cat.clone(),
                move_files: false,
                allow_breaking_hardlinks: false,
            };
            let o = remote_action(state, t, &relabel).await;
            record(job, 1, o);
        } else {
            record(job, 1, Outcome::skip("not_here"));
        }
    }
    for chunk in local.chunks(BULK_TX) {
        if cancelled(job) {
            return;
        }
        let (st, rows, e) = (state.clone(), chunk.to_vec(), edit.clone());
        let res = tokio::task::spawn_blocking(move || {
            if let SetEdit::ShareLimits {
                ratio,
                seeding,
                inactive,
            } = &e
            {
                let mut hashes: Vec<String> = rows.iter().map(|(h, _)| h.clone()).collect();
                hashes.sort();
                hashes.dedup();
                // Counted per row, like the other set-based edits: every copy
                // of a torrent now has these limits.
                return crate::sharelimits::set_torrent_limits(
                    &st, &hashes, *ratio, *seeding, *inactive,
                )
                .map(|_| rows.len())
                .map_err(|e| anyhow::anyhow!(e));
            }
            let changed = {
                let mut store = st.store.lock().unwrap_or_else(|p| p.into_inner());
                let edit = match &e {
                    SetEdit::Paused(p) => crate::store::BulkEdit::Paused(*p),
                    SetEdit::Pinned(p) => crate::store::BulkEdit::Pinned(*p),
                    SetEdit::Category(cat) => crate::store::BulkEdit::Category(cat),
                    SetEdit::Tags { tags, add } => crate::store::BulkEdit::Tags { tags, add: *add },
                    SetEdit::ShareLimits { .. } => unreachable!("written above"),
                };
                let changed = store.bulk_edit(&rows, edit);
                // Force download STARTS the torrent, as qBittorrent's force
                // start does: a pin alone left one the user had stopped
                // stopped, forced in name only.
                if changed.is_ok() && matches!(e, SetEdit::Pinned(true)) {
                    store
                        .bulk_edit(&rows, crate::store::BulkEdit::Paused(false))
                        .and(changed)
                } else {
                    changed
                }
            };
            // The engines, after the store and outside its lock: stopping a
            // torrent touches the engine and its announces, not the database.
            // Every copy, changed or not -- a row already marked stopped says
            // nothing about whether its engine agrees. A start goes through
            // the path claim (`apply_pause_to_engine`).
            let paused = match &e {
                SetEdit::Paused(p) => Some(*p),
                SetEdit::Pinned(true) => Some(false),
                _ => None,
            };
            if let (Ok(_), Some(p)) = (&changed, paused) {
                for (h, engine) in &rows {
                    crate::api::apply_pause_to_engine(&st, engine, h, p);
                }
            }
            changed
        })
        .await;
        match res {
            Ok(Ok(changed)) => {
                if changed > 0 {
                    record(job, changed, Outcome::ok("ok"));
                }
                if changed < chunk.len() {
                    record(job, chunk.len() - changed, Outcome::ok("unchanged"));
                }
            }
            Ok(Err(e)) => record(
                job,
                chunk.len(),
                Outcome {
                    key: "failed".into(),
                    error: Some(format!("store: {e}")),
                    consent: None,
                },
            ),
            Err(e) => record(
                job,
                chunk.len(),
                Outcome {
                    key: "failed".into(),
                    error: Some(format!("store: {e}")),
                    consent: None,
                },
            ),
        }
    }
}

async fn run(state: AppState, job: Shared, action: Action, targets: Vec<Target>) {
    let caller = Caller {
        router: crate::api::router(state.clone()),
        key: state.cfg().daemon.api_key.clone(),
    };
    match SetEdit::of(&action) {
        Some(edit) => set_based(&state, &caller, &job, edit, &targets).await,
        None => {
            // In rounds of `n`, not a stream: a stream of futures borrowing
            // this frame is not `Send` in a way `tokio::spawn` can prove.
            let n = action.concurrency();
            for round in targets.chunks(n) {
                if cancelled(&job) {
                    break;
                }
                let mut futs = Vec::with_capacity(round.len());
                for t in round {
                    futs.push(one(&state, &caller, &action, t));
                }
                for o in futures::future::join_all(futs).await {
                    record(&job, 1, o);
                }
            }
        }
    }
    let mut j = job.lock().unwrap_or_else(|p| p.into_inner());
    j.finished = true;
    j.cancelled = j.cancel;
    j.ended = Some(std::time::Instant::now());
    j.elapsed_ms = j
        .started
        .map(|s| s.elapsed().as_millis() as u64)
        .unwrap_or(0);
    tracing::info!(
        job = %j.id, action = %j.action, total = j.total, done = j.done, failed = j.failed,
        cancelled = j.cancelled, ms = j.elapsed_ms, "selection action finished"
    );
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionBody {
    selection: Selection,
    #[serde(default)]
    params: Value,
}

fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": "unauthorized"})),
    )
        .into_response()
}

/// `POST /api/selection/:action` -- resolve, answer 202 with the job, work in
/// the background. Everything that can be refused is refused before the 202.
async fn post_action(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return unauthorised();
    }
    let req: ActionBody = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            return Refusal::bad(format!("expected {{selection, params}}: {e}")).into_response()
        }
    };
    let action = match Action::parse(&name, &req.params) {
        Ok(a) => a,
        Err(r) => return r.into_response(),
    };
    let targets = match resolve(&state, &req.selection).await {
        Ok(t) => t,
        Err(r) => return r.into_response(),
    };
    let job = new_job(&name, targets.len());
    let id = job.lock().unwrap_or_else(|p| p.into_inner()).id.clone();
    tracing::info!(job = %id, action = %name, total = targets.len(),
        by_filter = req.selection.filter.is_some(), "selection action started");
    tokio::spawn(run(state.clone(), job, action, targets.clone()));
    (
        StatusCode::ACCEPTED,
        Json(json!({"job": id, "total": targets.len()})),
    )
        .into_response()
}

async fn get_job_route(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return unauthorised();
    }
    match get_job(&id) {
        Some(j) => {
            let mut v = j.lock().unwrap_or_else(|p| p.into_inner()).clone();
            if !v.finished {
                v.elapsed_ms = v
                    .started
                    .map(|s| s.elapsed().as_millis() as u64)
                    .unwrap_or(0);
            }
            Json(v).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such job"}))).into_response(),
    }
}

async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return unauthorised();
    }
    match get_job(&id) {
        Some(j) => {
            j.lock().unwrap_or_else(|p| p.into_inner()).cancel = true;
            Json(json!({"ok": true})).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "no such job"}))).into_response(),
    }
}

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/selection/jobs/:id", get(get_job_route))
        .route("/api/selection/jobs/:id/cancel", post(cancel_job))
        .route("/api/selection/:action", post(post_action))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn torrent(name: &str) -> Vec<u8> {
        format!(
            "d4:infod6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}ee",
            name.len(),
            "A".repeat(20)
        )
        .into_bytes()
    }

    /// Three hoard torrents: two in `movies`, one in `series`.
    fn library(tag: &str) -> (TestState, Vec<String>) {
        let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let mut hashes = Vec::new();
        for (name, cat) in [("m1", "movies"), ("m2", "movies"), ("s1", "series")] {
            let (h, _) = crate::api::add_torrent_bytes(
                &s.state,
                &torrent(name),
                cat,
                "/tmp",
                "",
                false,
                true,
                "hoard",
            )
            .expect("added");
            hashes.push(h);
        }
        (s, hashes)
    }

    fn sel(v: Value) -> Selection {
        serde_json::from_value(v).expect("a selection")
    }

    #[tokio::test]
    async fn an_empty_selection_is_refused_never_read_as_everything() {
        let (s, _) = library("sel-empty");
        let r = resolve(&s.state, &sel(json!({}))).await.unwrap_err();
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_key_this_code_does_not_implement_is_named_not_dropped() {
        let e = serde_json::from_value::<Selection>(json!({"filters": "category=x"})).unwrap_err();
        assert!(e.to_string().contains("filters"), "{e}");
        let r = check_filter("category=movies&limit=5").unwrap_err();
        assert!(r.body["error"].as_str().unwrap().contains("`limit`"));
        assert!(check_filter("?category=movies&tag_not=a").is_ok());
    }

    #[tokio::test]
    async fn items_and_filter_together_or_a_filter_without_expect_are_refused() {
        let (s, h) = library("sel-both");
        let both =
            sel(json!({"items": [{"hash": h[0]}], "filter": "category=movies", "expect": 2}));
        assert_eq!(
            resolve(&s.state, &both).await.unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
        let no_expect = sel(json!({"filter": "category=movies"}));
        assert_eq!(
            resolve(&s.state, &no_expect).await.unwrap_err().status,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn a_filter_resolves_like_the_list_minus_its_exceptions() {
        let (s, h) = library("sel-filter");
        let got = resolve(
            &s.state,
            &sel(json!({"filter": "category=movies", "expect": 2})),
        )
        .await
        .unwrap();
        let mut hashes: Vec<_> = got.iter().map(|t| t.hash.clone()).collect();
        hashes.sort();
        let mut want = vec![h[0].clone(), h[1].clone()];
        want.sort();
        assert_eq!(hashes, want);
        assert!(got
            .iter()
            .all(|t| t.agent == "local-hoard" && t.mode == "hoard"));

        let minus = sel(
            json!({"filter": "category=movies", "exclude": [{"hash": h[0], "agent": "local"}], "expect": 2}),
        );
        let got = resolve(&s.state, &minus).await.unwrap();
        assert_eq!(
            got.iter().map(|t| t.hash.clone()).collect::<Vec<_>>(),
            vec![h[1].clone()]
        );

        // "" is the whole list, and has to be said.
        let all = resolve(&s.state, &sel(json!({"filter": "", "expect": 3})))
            .await
            .unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn a_filter_that_grew_past_what_was_confirmed_is_refused_with_the_new_count() {
        let (s, _) = library("sel-grew");
        let r = resolve(&s.state, &sel(json!({"filter": "", "expect": 2})))
            .await
            .unwrap_err();
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert_eq!(r.body["count"], 3);
        // Fewer than confirmed is fine: nothing unconfirmed is touched.
        assert_eq!(
            resolve(&s.state, &sel(json!({"filter": "", "expect": 10})))
                .await
                .unwrap()
                .len(),
            3
        );
    }

    async fn wait(s: &TestState, id: &str) -> Value {
        for _ in 0..200 {
            let r = get_job_route(
                State(s.state.clone()),
                Path(id.to_string()),
                RawQuery(None),
                keyed(KEY),
            )
            .await;
            let v: Value = serde_json::from_slice(
                &axum::body::to_bytes(r.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            if v["finished"] == true {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("job {id} never finished");
    }

    async fn start(s: &TestState, action: &str, body: Value) -> (StatusCode, Value) {
        let r = post_action(
            State(s.state.clone()),
            Path(action.into()),
            RawQuery(None),
            keyed(KEY),
            body.to_string(),
        )
        .await;
        let status = r.status();
        (
            status,
            serde_json::from_slice(
                &axum::body::to_bytes(r.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap(),
        )
    }

    fn tags_of(s: &TestState, hash: &str) -> Vec<String> {
        let store = s.state.store.read().unwrap();
        store
            .export_rows(&[hash.to_string()])
            .unwrap()
            .remove(0)
            .tags
    }

    #[tokio::test]
    async fn a_tag_by_filter_reaches_exactly_the_filtered_torrents() {
        let (s, h) = library("sel-tags");
        let (st, v) = start(
            &s,
            "tags",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {"tags": ["noHL"], "op": "add"}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        assert_eq!(v["total"], 2);
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["done"], 2);
        assert_eq!(j["failed"], 0, "{j}");
        assert!(tags_of(&s, &h[0]).contains(&"noHL".to_string()));
        assert!(tags_of(&s, &h[1]).contains(&"noHL".to_string()));
        assert!(
            !tags_of(&s, &h[2]).contains(&"noHL".to_string()),
            "series was not selected"
        );
        // The bulk write dated the tag, on the torrents it tagged only.
        let dated = |i: usize| {
            s.state
                .store
                .read()
                .unwrap()
                .tag_since_rows(&h[i])
                .into_iter()
                .map(|r| r.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(dated(0), ["noHL"]);
        assert_eq!(dated(1), ["noHL"]);
        assert!(dated(2).is_empty());
    }

    /// ⭐ `limits` by filter caps exactly the filtered torrents, in the engine
    /// itself (read back from the torrent, not from the job's tally), and an
    /// empty params is refused rather than read as "lift everything".
    #[tokio::test]
    async fn limits_by_filter_cap_exactly_the_filtered_torrents() {
        let (s, h) = library("sel-limits");
        let (st, _) = start(
            &s,
            "limits",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "no direction given");
        let (st, v) = start(
            &s,
            "limits",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {"up_kib": 100, "down_kib": 0}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(
            (j["done"].as_u64(), j["failed"].as_u64()),
            (Some(2), Some(0)),
            "{j}"
        );
        let hoard = s.state.engines.get("hoard").unwrap();
        let caps = |hash: &str| {
            hoard
                .manager
                .get(&typhon_engine::torrent::hex_decode(hash).unwrap())
                .unwrap()
                .rate_limits()
        };
        assert_eq!(caps(&h[0]), (102_400, 0));
        assert_eq!(caps(&h[1]), (102_400, 0));
        assert_eq!(caps(&h[2]), (0, 0), "series was not selected");
    }

    /// `download_order` by filter sets the flags on exactly the filtered
    /// torrents, in the engine; a flag left out is kept, empty params refused.
    #[tokio::test]
    async fn download_order_by_filter_reaches_exactly_the_filtered_torrents() {
        let (s, h) = library("sel-order");
        let (st, _) = start(
            &s,
            "download_order",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let hoard = s.state.engines.get("hoard").unwrap();
        let flags = |hash: &str| {
            hoard
                .manager
                .get(&typhon_engine::torrent::hex_decode(hash).unwrap())
                .unwrap()
                .download_order_flags()
        };
        hoard
            .manager
            .update_download_order(&typhon_engine::torrent::hex_decode(&h[0]).unwrap(), |o| {
                o.first_last = true
            })
            .unwrap();
        let (st, v) = start(
            &s,
            "download_order",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {"sequential": true}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(
            (j["done"].as_u64(), j["failed"].as_u64()),
            (Some(2), Some(0)),
            "{j}"
        );
        assert_eq!(flags(&h[0]), (true, true), "first/last kept");
        assert_eq!(flags(&h[1]), (true, false));
        assert_eq!(flags(&h[2]), (false, false), "series was not selected");
    }

    /// `share_limits` by filter is a store write on exactly the filtered
    /// torrents; an empty params is refused.
    #[tokio::test]
    async fn share_limits_by_filter_reach_exactly_the_filtered_torrents() {
        let (s, h) = library("sel-share");
        let (st, _) = start(
            &s,
            "share_limits",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = start(
            &s,
            "share_limits",
            json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {"ratio_limit": 2.5, "seeding_time_limit": -1}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(
            (j["done"].as_u64(), j["failed"].as_u64()),
            (Some(2), Some(0)),
            "{j}"
        );
        let own = |hash: &str| s.store.read().unwrap().share_limits_of(hash);
        assert_eq!(
            (
                own(&h[0]).ratio,
                own(&h[0]).seeding_minutes,
                own(&h[0]).inactive_minutes
            ),
            (2.5, -1, -2)
        );
        assert_eq!(own(&h[1]).ratio, 2.5);
        assert_eq!(
            own(&h[2]),
            crate::sharelimits::TorrentShareLimits::default(),
            "series was not selected"
        );
    }

    #[tokio::test]
    async fn a_stop_by_filter_is_one_store_write_and_reports_what_applied() {
        let (s, h) = library("sel-stop");
        let (st, v) = start(
            &s,
            "stop",
            json!({"selection": {"filter": "category=series", "expect": 1}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["done"], 1, "{j}");
        assert_eq!(j["failed"], 0, "{j}");
        let paused: i64 = {
            let store = s.state.store.read().unwrap();
            store
                .paused_hashes("hoard")
                .map(|p| p.contains(&h[2]) as i64 + 2 * p.contains(&h[0]) as i64)
                .unwrap_or(-1)
        };
        assert_eq!(
            paused, 1,
            "the series torrent is stopped, the movies are not"
        );
    }

    /// ⭐ Force download STARTS a torrent the user stopped, as qBittorrent's
    /// force start does -- from the selection and from the single-torrent
    /// route. 4.4.1 only pinned it, and it stayed stopped.
    #[tokio::test]
    async fn force_download_starts_a_stopped_torrent() {
        use tower::ServiceExt;
        let (s, h) = library("sel-force");
        let (_, v) = start(
            &s,
            "stop",
            json!({"selection": {"filter": "category=movies", "expect": 2}}),
        )
        .await;
        wait(&s, v["job"].as_str().unwrap()).await;
        let engine_paused = |hash: &str| {
            let (_, t) = crate::api::find_torrent(&s.state, hash).expect("held");
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed)
        };
        let stored_paused = |hash: &str| {
            s.state
                .store
                .read()
                .unwrap()
                .paused_hashes("hoard")
                .unwrap()
                .iter()
                .any(|p| p == hash)
        };
        assert!(
            engine_paused(&h[0]) && stored_paused(&h[0]),
            "stopped first"
        );

        let (st, v) = start(
            &s,
            "pin",
            json!({"selection": {"items": [{"hash": h[0], "agent": "local-hoard"}]}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["failed"], 0, "{j}");
        assert!(!stored_paused(&h[0]), "the intent says started");
        assert!(!engine_paused(&h[0]), "and the engine runs it");
        assert!(engine_paused(&h[1]), "the other movie was not selected");

        let req = axum::http::Request::builder()
            .method("POST")
            .uri(format!("/api/hoard/torrents/{}/pin", h[1]))
            .header("X-API-Key", KEY)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = crate::api::router(s.state.clone())
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            !stored_paused(&h[1]) && !engine_paused(&h[1]),
            "the single route starts it too"
        );
        assert!(s
            .state
            .store
            .read()
            .unwrap()
            .pinned("hoard")
            .unwrap()
            .contains(&h[1]));
    }

    #[tokio::test]
    async fn explicit_items_still_work_and_an_unknown_action_or_param_is_refused() {
        let (s, h) = library("sel-items");
        let (st, v) = start(
            &s,
            "remove",
            json!({"selection": {"items": [{"hash": h[2], "agent": "local-hoard"}]}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["tally"]["ok"], 1, "{j}");
        assert!(s
            .state
            .store
            .read()
            .unwrap()
            .export_rows(&[h[2].clone()])
            .unwrap()
            .is_empty());

        let (st, _) = start(
            &s,
            "explode",
            json!({"selection": {"items": [{"hash": h[0]}]}}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = start(
            &s,
            "remove",
            json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"delete": true}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("delete"), "{v}");
    }

    #[tokio::test]
    async fn a_selection_action_refuses_a_caller_with_no_key() {
        let (s, h) = library("sel-auth");
        let r = post_action(
            State(s.state.clone()),
            Path("remove".into()),
            RawQuery(None),
            HeaderMap::new(),
            json!({"selection": {"items": [{"hash": h[0]}]}}).to_string(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        let r = get_job_route(
            State(s.state.clone()),
            Path("x".into()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    fn dirty_rows(s: &TestState) -> i64 {
        let store = s.state.store.lock().unwrap();
        store.dirty_count_for_tests()
    }

    /// ⭐ The single-torrent tags route REPLACES the set (`{"tags": [...]}` is
    /// the state wanted), and the page used to send it `{tags, op}`: removing
    /// tag X from a selection SET X on every torrent instead. The selection
    /// honours `op`.
    #[tokio::test]
    async fn removing_a_tag_removes_that_tag_and_keeps_the_others() {
        let (s, h) = library("sel-untag");
        for tag in ["a", "b"] {
            let (_, v) = start(&s, "tags", json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"tags": [tag], "op": "add"}})).await;
            wait(&s, v["job"].as_str().unwrap()).await;
        }
        assert_eq!(tags_of(&s, &h[0]), vec!["a".to_string(), "b".to_string()]);
        let (_, v) = start(&s, "tags", json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"tags": ["a"], "op": "remove"}})).await;
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["tally"]["ok"], 1, "{j}");
        assert_eq!(tags_of(&s, &h[0]), vec!["b".to_string()]);
    }

    /// The list's cached facts are patched by the write itself: nothing is
    /// left for the next list request to read again under the store's lock,
    /// and that request already sees the new state.
    #[tokio::test]
    async fn a_set_based_write_leaves_the_list_nothing_to_reread() {
        let (s, h) = library("sel-facts");
        // Warm the list's cache, as a page view would.
        crate::api::fleet_page(&s.state, "hoard", "limit=1").await;
        assert_eq!(dirty_rows(&s), 0);
        let (_, v) = start(&s, "tags", json!({"selection": {"filter": "category=movies", "expect": 2}, "params": {"tags": ["x"], "op": "add"}})).await;
        wait(&s, v["job"].as_str().unwrap()).await;
        let (_, v) = start(
            &s,
            "stop",
            json!({"selection": {"filter": "category=movies", "expect": 2}}),
        )
        .await;
        wait(&s, v["job"].as_str().unwrap()).await;
        let (_, v) = start(
            &s,
            "category",
            json!({"selection": {"items": [{"hash": h[2]}]}, "params": {"category": "movies"}}),
        )
        .await;
        wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(
            dirty_rows(&s),
            0,
            "the writes left rows for the list to re-read"
        );
        let page = crate::api::fleet_page(&s.state, "hoard", "fields=hash&tag=x").await;
        assert_eq!(page["filtered"], 2);
        let page = crate::api::fleet_page(&s.state, "hoard", "fields=hash&category=movies").await;
        assert_eq!(page["filtered"], 3, "the relabel shows at once");
        let page = crate::api::fleet_page(&s.state, "hoard", "fields=hash&state=stopped").await;
        assert_eq!(page["filtered"], 2);
    }

    /// A location is queued per torrent through the single-torrent route, and
    /// an empty one is refused before any torrent is touched.
    #[tokio::test]
    async fn a_location_is_queued_per_torrent_and_an_empty_one_refused() {
        let (s, h) = library("sel-location");
        let to = std::env::temp_dir().join(format!("hydra-sel-location-{}", std::process::id()));
        let (st, _) = start(
            &s,
            "location",
            json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"location": " "}}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = start(
            &s,
            "location",
            json!({"selection": {"items": [{"hash": h[0]}, {"hash": h[1]}]},
                                                   "params": {"location": to.to_string_lossy()}}),
        )
        .await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["tally"]["moving"], 2, "{j}");
    }

    #[tokio::test]
    async fn a_second_identical_write_is_reported_unchanged_not_done() {
        let (s, _) = library("sel-unchanged");
        for expected in ["ok", "unchanged"] {
            let (_, v) = start(
                &s,
                "stop",
                json!({"selection": {"filter": "", "expect": 3}}),
            )
            .await;
            let j = wait(&s, v["job"].as_str().unwrap()).await;
            assert_eq!(j["tally"][expected], 3, "{j}");
        }
    }

    // ---- Target and the small pure helpers ----------------------------------

    fn target(hash: &str, agent: &str, mode: &str) -> Target {
        Target {
            hash: hash.into(),
            agent: agent.into(),
            mode: mode.into(),
        }
    }

    /// Which engine a copy sits in, from whatever the row said.
    #[test]
    fn a_target_names_the_engine_its_agent_or_mode_points_at() {
        assert_eq!(
            target("h", "local-vpn1", "hoard").engine(),
            "vpn1",
            "the agent wins over the mode"
        );
        assert_eq!(
            target("h", "", "race").engine(),
            "race",
            "an empty agent is this node, in the row's mode"
        );
        assert_eq!(target("h", "local", "hoard").engine(), "hoard");
        assert_eq!(
            target("h", "nas-race", "hoard").engine(),
            "race",
            "a remote row: the part after the last hyphen"
        );
        assert_eq!(
            target("h", "nas", "hoard").engine(),
            "",
            "no hyphen, no guess"
        );
    }

    #[test]
    fn only_the_local_spellings_are_this_node() {
        for yes in ["", "local", "local-race", "local-"] {
            assert!(target("h", yes, "hoard").is_local(), "{yes:?}");
        }
        for no in ["nas-race", "locale", "localhost-race", "Local", "remote"] {
            assert!(!target("h", no, "hoard").is_local(), "{no:?}");
        }
    }

    /// An info hash is 40 hex (v1) or 64 hex (v2), trimmed and lower-cased;
    /// anything else is not one.
    #[test]
    fn a_hash_is_forty_or_sixty_four_hex_characters_and_nothing_else() {
        let v1 = "AB".repeat(20);
        assert_eq!(clean_hash(&format!("  {v1}\n")), Some(v1.to_lowercase()));
        let v2 = "0f".repeat(32);
        assert_eq!(clean_hash(&v2), Some(v2.clone()));
        for bad in [
            "",
            "abc",
            &"a".repeat(39),
            &"a".repeat(41),
            &"g".repeat(40),
            &format!("{}z", "a".repeat(39)),
        ] {
            assert_eq!(clean_hash(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn url_encoding_keeps_unreserved_characters_and_escapes_the_rest() {
        assert_eq!(enc("vpn-1_a.b~c"), "vpn-1_a.b~c");
        assert_eq!(enc("a b/c?d&e=é"), "a%20b%2Fc%3Fd%26e%3D%C3%A9");
        assert_eq!(enc(""), "");
    }

    /// A route's refusal is read in the order the routes speak: an `error`
    /// field, a bare string, then the bare status.
    #[test]
    fn an_error_is_read_from_the_field_then_the_string_then_the_status() {
        assert_eq!(
            error_of(StatusCode::CONFLICT, &json!({"error": "taken"})),
            "taken"
        );
        assert_eq!(
            error_of(
                StatusCode::BAD_GATEWAY,
                &Value::String("upstream down".into())
            ),
            "upstream down"
        );
        assert_eq!(
            error_of(StatusCode::BAD_GATEWAY, &Value::String(String::new())),
            "HTTP 502"
        );
        assert_eq!(
            error_of(StatusCode::NOT_FOUND, &json!({"detail": "x"})),
            "HTTP 404"
        );
        assert_eq!(error_of(StatusCode::NOT_FOUND, &Value::Null), "HTTP 404");
    }

    /// Flags left alone are left out of the body: `null` would not parse as
    /// "keep" on the far side.
    #[test]
    fn the_download_order_body_carries_only_the_flags_that_were_given() {
        assert_eq!(download_order_body(None, None), json!({}));
        assert_eq!(
            download_order_body(Some(true), None),
            json!({"sequential": true})
        );
        assert_eq!(
            download_order_body(None, Some(false)),
            json!({"first_last_piece": false})
        );
        assert_eq!(
            download_order_body(Some(false), Some(true)),
            json!({"sequential": false, "first_last_piece": true})
        );
    }

    #[test]
    fn a_failure_names_the_torrent_by_its_first_eight_characters() {
        let o = Outcome::failed("0123456789abcdef", "boom".into());
        assert_eq!(o.key, "failed");
        assert_eq!(o.error.as_deref(), Some("01234567: boom"));
        assert_eq!(
            Outcome::failed("abc", "x".into()).error.as_deref(),
            Some("abc: x"),
            "a short hash does not panic"
        );
        assert_eq!(Outcome::skip("not_here").key, "not_here");
        assert!(Outcome::skip("not_here").error.is_none());
    }

    // ---- Action::parse -------------------------------------------------------

    fn refused(name: &str, params: Value) -> Refusal {
        Action::parse(name, &params).expect_err(&format!("{name} {params} must be refused"))
    }

    #[test]
    fn the_simple_actions_take_no_params_and_a_null_is_an_empty_object() {
        assert!(matches!(
            Action::parse("stop", &Value::Null),
            Ok(Action::Pause(true))
        ));
        assert!(matches!(
            Action::parse("start", &json!({})),
            Ok(Action::Pause(false))
        ));
        assert!(matches!(
            Action::parse("pin", &Value::Null),
            Ok(Action::Pin(true))
        ));
        assert!(matches!(
            Action::parse("unpin", &Value::Null),
            Ok(Action::Pin(false))
        ));
        assert!(matches!(
            Action::parse("reannounce", &Value::Null),
            Ok(Action::Reannounce)
        ));
        assert!(matches!(
            Action::parse("recheck", &Value::Null),
            Ok(Action::Recheck)
        ));
        for name in ["stop", "start", "pin", "unpin", "reannounce", "recheck"] {
            let r = refused(name, json!({"force": true}));
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{name}");
            let msg = r.body["error"].as_str().unwrap();
            assert!(
                msg.starts_with("params:") && msg.contains("force"),
                "{name}: {msg}"
            );
        }
    }

    #[test]
    fn tags_need_an_op_and_at_least_one_real_tag() {
        match Action::parse("tags", &json!({"tags": ["a", "b"], "op": "remove"})) {
            Ok(Action::Tags { tags, op }) => assert_eq!(
                (tags, op),
                (vec!["a".to_string(), "b".to_string()], "remove".to_string())
            ),
            other => panic!("{other:?}"),
        }
        assert!(
            refused("tags", json!({"tags": ["a"], "op": "set"})).body["error"]
                .as_str()
                .unwrap()
                .contains("add")
        );
        assert!(
            refused("tags", json!({"tags": [], "op": "add"})).body["error"]
                .as_str()
                .unwrap()
                .contains("empty")
        );
        assert!(
            refused("tags", json!({"tags": ["  ", ""], "op": "add"})).body["error"]
                .as_str()
                .unwrap()
                .contains("empty")
        );
        refused("tags", json!({"tags": ["a"]}));
        refused("tags", Value::Null);
    }

    #[test]
    fn category_location_and_remove_default_their_flags_to_off() {
        match Action::parse("category", &json!({"category": "movies"})) {
            Ok(Action::Category {
                category,
                move_files,
                allow_breaking_hardlinks,
            }) => {
                assert_eq!(
                    (category.as_str(), move_files, allow_breaking_hardlinks),
                    ("movies", false, false)
                )
            }
            other => panic!("{other:?}"),
        }
        match Action::parse(
            "category",
            &json!({"category": "", "move_files": true, "allow_breaking_hardlinks": true}),
        ) {
            Ok(Action::Category {
                category,
                move_files,
                allow_breaking_hardlinks,
            }) => {
                assert_eq!(
                    (category.as_str(), move_files, allow_breaking_hardlinks),
                    ("", true, true),
                    "an empty category clears it"
                )
            }
            other => panic!("{other:?}"),
        }
        refused("category", json!({}));
        match Action::parse("location", &json!({"location": "/data/x"})) {
            Ok(Action::Location {
                location,
                allow_breaking_hardlinks,
            }) => assert_eq!(
                (location.as_str(), allow_breaking_hardlinks),
                ("/data/x", false)
            ),
            other => panic!("{other:?}"),
        }
        assert!(
            refused("location", json!({"location": "   "})).body["error"]
                .as_str()
                .unwrap()
                .contains("empty")
        );
        assert!(matches!(
            Action::parse("remove", &Value::Null),
            Ok(Action::Remove {
                delete_files: false
            })
        ));
        assert!(matches!(
            Action::parse("remove", &json!({"delete_files": true})),
            Ok(Action::Remove { delete_files: true })
        ));
    }

    #[test]
    fn engine_and_node_actions_need_their_targets_named() {
        assert!(
            matches!(Action::parse("copy", &json!({"engine": "race"})), Ok(Action::Copy { engine }) if engine == "race")
        );
        assert!(
            matches!(Action::parse("move-engine", &json!({"engine": "hoard"})), Ok(Action::MoveEngine { engine }) if engine == "hoard")
        );
        refused("copy", json!({}));
        refused("move-engine", Value::Null);

        match Action::parse(
            "handoff",
            &json!({"node": "nas", "engine": "race", "then": "remove"}),
        ) {
            Ok(Action::Handoff { node, engine, then }) => assert_eq!(
                (node.as_str(), engine.as_str(), then.as_str()),
                ("nas", "race", "remove")
            ),
            other => panic!("{other:?}"),
        }
        assert!(refused(
            "handoff",
            json!({"node": "nas", "engine": "race", "then": "delete"})
        )
        .body["error"]
            .as_str()
            .unwrap()
            .contains("keep"));
        refused("handoff", json!({"node": "nas", "engine": "race"}));

        match Action::parse("node-fetch", &json!({"node": "nas", "engine": "race"})) {
            Ok(Action::NodeFetch {
                node,
                from_engine,
                engine,
            }) => assert_eq!(
                (node.as_str(), from_engine.as_str(), engine.as_str()),
                ("nas", "", "race")
            ),
            other => panic!("{other:?}"),
        }
        match Action::parse("node-move", &json!({"node": "nas", "engine": "vpn1"})) {
            Ok(Action::NodeMove { node, engine }) => {
                assert_eq!((node.as_str(), engine.as_str()), ("nas", "vpn1"))
            }
            other => panic!("{other:?}"),
        }
        refused("node-move", json!({"node": "nas"}));
    }

    /// An action that changes nothing is a refusal, not a job that touches
    /// the whole selection to no effect.
    #[test]
    fn a_limits_or_order_action_that_sets_nothing_is_refused() {
        refused("limits", json!({}));
        refused("share_limits", json!({}));
        refused("download_order", Value::Null);
        assert!(matches!(
            Action::parse("limits", &json!({"up_kib": 0})),
            Ok(Action::Limits {
                up_kib: Some(0),
                down_kib: None
            })
        ));
        assert!(matches!(
            Action::parse("limits", &json!({"down_kib": 50})),
            Ok(Action::Limits {
                up_kib: None,
                down_kib: Some(50)
            })
        ));
        match Action::parse("share_limits", &json!({"inactive_seeding_time_limit": 30})) {
            Ok(Action::ShareLimits {
                ratio,
                seeding,
                inactive,
            }) => assert_eq!((ratio, seeding, inactive), (None, None, Some(30))),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            Action::parse("download_order", &json!({"first_last_piece": true})),
            Ok(Action::DownloadOrder {
                sequential: None,
                first_last_piece: Some(true)
            })
        ));
    }

    #[test]
    fn an_unknown_action_is_a_404_that_names_it() {
        let r = refused("explode", Value::Null);
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert!(r.body["error"].as_str().unwrap().contains("explode"));
    }

    #[test]
    fn only_a_reannounce_runs_eight_at_a_time() {
        assert_eq!(Action::Reannounce.concurrency(), 8);
        for a in [
            Action::Recheck,
            Action::Pause(true),
            Action::Remove { delete_files: true },
            Action::Copy { engine: "x".into() },
        ] {
            assert_eq!(a.concurrency(), 1, "{a:?}");
        }
    }

    // ---- which edits go to the store as one write ------------------------------

    #[test]
    fn a_relabel_is_a_set_edit_but_moving_the_files_is_not() {
        let relabel = Action::Category {
            category: "movies".into(),
            move_files: false,
            allow_breaking_hardlinks: false,
        };
        assert!(matches!(SetEdit::of(&relabel), Some(SetEdit::Category(c)) if c == "movies"));
        let moving = Action::Category {
            category: "movies".into(),
            move_files: true,
            allow_breaking_hardlinks: false,
        };
        assert!(
            SetEdit::of(&moving).is_none(),
            "moving files is real work per torrent"
        );
        for a in [
            Action::Recheck,
            Action::Reannounce,
            Action::Remove {
                delete_files: false,
            },
            Action::Limits {
                up_kib: Some(1),
                down_kib: None,
            },
        ] {
            assert!(SetEdit::of(&a).is_none(), "{a:?}");
        }
        assert!(matches!(
            SetEdit::of(&Action::Pause(true)),
            Some(SetEdit::Paused(true))
        ));
        assert!(matches!(
            SetEdit::of(&Action::Pin(false)),
            Some(SetEdit::Pinned(false))
        ));
    }

    /// Tags are trimmed and blanks dropped before they reach the store; `op`
    /// decides add or remove.
    #[test]
    fn tags_are_trimmed_and_the_op_picks_add_or_remove() {
        let a = Action::Tags {
            tags: vec![" keep ".into(), "  ".into(), "x".into()],
            op: "add".into(),
        };
        match SetEdit::of(&a) {
            Some(SetEdit::Tags { tags, add }) => assert_eq!(
                (tags, add),
                (vec!["keep".to_string(), "x".to_string()], true)
            ),
            _ => panic!("tags are a set edit"),
        }
        match SetEdit::of(&Action::Tags {
            tags: vec!["x".into()],
            op: "remove".into(),
        }) {
            Some(SetEdit::Tags { add, .. }) => assert!(!add),
            _ => panic!("tags are a set edit"),
        }
    }

    /// Pins and tags exist for the hoard's rows only, as the single routes
    /// decide; a pause, a relabel and share limits reach every mode.
    #[test]
    fn pins_and_tags_apply_to_hoard_rows_only() {
        let hoard = target("h", "local-hoard", "hoard");
        let race = target("h", "local-race", "race");
        for e in [
            SetEdit::Pinned(true),
            SetEdit::Tags {
                tags: vec![],
                add: true,
            },
        ] {
            assert!(e.applies(&hoard));
            assert!(!e.applies(&race));
        }
        for e in [
            SetEdit::Paused(true),
            SetEdit::Category("c".into()),
            SetEdit::ShareLimits {
                ratio: None,
                seeding: None,
                inactive: None,
            },
        ] {
            assert!(e.applies(&hoard) && e.applies(&race));
        }
    }

    // ---- resolve: items ----------------------------------------------------------

    #[tokio::test]
    async fn items_are_normalised_deduplicated_and_given_the_views_mode() {
        let (s, _) = library("sel-items-norm");
        let a = "AA".repeat(20);
        let b = "bb".repeat(20);
        let got = resolve(
            &s.state,
            &sel(json!({"view": "race", "items": [
                {"hash": a},
                {"hash": a.to_lowercase(), "agent": "local"},
                {"hash": a, "agent": "local-race"},
                {"hash": a, "agent": "local-hoard", "mode": "hoard"},
                {"hash": format!(" {b} "), "agent": "nas-race", "mode": "race"},
            ]})),
        )
        .await
        .unwrap();
        let got: Vec<(String, String, String)> =
            got.into_iter().map(|t| (t.hash, t.agent, t.mode)).collect();
        assert_eq!(
            got,
            vec![
                (
                    a.to_lowercase(),
                    "local-race".to_string(),
                    "race".to_string()
                ),
                (
                    a.to_lowercase(),
                    "local-hoard".to_string(),
                    "hoard".to_string()
                ),
                (b.clone(), "nas-race".to_string(), "race".to_string()),
            ],
            "three spellings of one copy collapse; the same hash in another engine stays"
        );
    }

    #[tokio::test]
    async fn an_item_that_is_not_a_hash_or_a_view_that_is_not_an_engine_is_refused() {
        let (s, _) = library("sel-items-bad");
        let r = resolve(&s.state, &sel(json!({"items": [{"hash": "not-a-hash"}]})))
            .await
            .unwrap_err();
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.body["error"]
            .as_str()
            .unwrap()
            .contains("`not-a-hash` is not an info hash"));
        let r = resolve(
            &s.state,
            &sel(json!({"view": "nope", "items": [{"hash": "aa".repeat(20)}]})),
        )
        .await
        .unwrap_err();
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert!(r.body["error"]
            .as_str()
            .unwrap()
            .contains("no engine `nope`"));
        let r = resolve(&s.state, &sel(json!({"filter": "limit=5", "expect": 1})))
            .await
            .unwrap_err();
        assert!(
            r.body["error"]
                .as_str()
                .unwrap()
                .contains("`limit` is not a filter"),
            "an unknown filter key is refused through resolve too"
        );
    }

    /// An excluded copy is matched by hash AND agent, with the same
    /// normalisation as the selection: excluding `local-race` leaves the
    /// hoard copy of the same hash.
    #[tokio::test]
    async fn an_exclusion_is_matched_on_hash_and_agent_after_normalising() {
        let (s, h) = library("sel-exclude");
        let all = sel(json!({"filter": "", "expect": 3, "exclude": [
            {"hash": h[0].to_uppercase(), "agent": "local"},
            {"hash": h[1], "agent": "local-race"},
            {"hash": "garbage"}
        ]}));
        let got = resolve(&s.state, &all).await.unwrap();
        let mut hashes: Vec<String> = got.into_iter().map(|t| t.hash).collect();
        hashes.sort();
        let mut want = vec![h[1].clone(), h[2].clone()];
        want.sort();
        assert_eq!(
            hashes, want,
            "h0 excluded as local-hoard; h1's exclusion names the race copy, which is not selected"
        );
    }

    // ---- jobs --------------------------------------------------------------------

    #[test]
    fn a_job_keeps_the_exact_failure_count_and_only_the_first_hundred_errors() {
        let job = new_job("test", 150);
        for i in 0..150 {
            record(
                &job,
                1,
                Outcome::failed(&format!("{i:040x}"), "boom".into()),
            );
        }
        record(&job, 3, Outcome::ok("ok"));
        let j = job.lock().unwrap();
        assert_eq!((j.done, j.failed), (153, 150));
        assert_eq!(j.errors.len(), ERRORS_KEPT);
        assert_eq!(j.tally["failed"], 150);
        assert_eq!(j.tally["ok"], 3);
        assert!(j.errors[0].ends_with(": boom"));
    }

    #[test]
    fn consent_requests_accumulate_with_their_file_and_byte_counts() {
        let job = new_job("category", 2);
        for (hash, files, bytes) in [("a", 3u64, 1000u64), ("b", 2, 500)] {
            let item = Item {
                hash: hash.into(),
                agent: "local-hoard".into(),
                mode: "hoard".into(),
            };
            record(
                &job,
                1,
                Outcome {
                    key: "needs_consent".into(),
                    error: None,
                    consent: Some((item, files, bytes)),
                },
            );
        }
        let j = job.lock().unwrap();
        assert_eq!(
            j.consent
                .items
                .iter()
                .map(|i| i.hash.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!((j.consent.files, j.consent.bytes), (5, 1500));
        assert_eq!(j.failed, 0, "waiting for a yes is not a failure");
    }

    /// A finished job stays readable for an hour; the next job started after
    /// that sweeps it. A running job is never swept.
    #[test]
    fn a_job_finished_long_ago_is_forgotten_when_the_next_one_starts() {
        let old = new_job("old", 1);
        let running = new_job("running", 1);
        let recent = new_job("recent", 1);
        let (old_id, running_id, recent_id) = (
            old.lock().unwrap().id.clone(),
            running.lock().unwrap().id.clone(),
            recent.lock().unwrap().id.clone(),
        );
        let now = std::time::Instant::now();
        old.lock().unwrap().ended = Some(
            now.checked_sub(JOB_TTL + std::time::Duration::from_secs(5))
                .expect("host up long enough"),
        );
        recent.lock().unwrap().ended = Some(now);
        let _next = new_job("next", 1);
        assert!(get_job(&old_id).is_none(), "swept");
        assert!(get_job(&running_id).is_some(), "no end time, still running");
        assert!(get_job(&recent_id).is_some(), "finished a moment ago");
    }

    /// Drive a job to its end the way the route does, without going through
    /// HTTP, and hand back its final state.
    async fn run_to_end(s: &TestState, action: Action, targets: Vec<Target>) -> Job {
        let job = new_job("test", targets.len());
        run(s.state.clone(), job.clone(), action, targets).await;
        let j = job.lock().unwrap().clone();
        j
    }

    #[tokio::test]
    async fn a_cancelled_job_does_nothing_more_and_says_it_was_cancelled() {
        let (s, h) = library("sel-cancel-run");
        let targets: Vec<Target> = h
            .iter()
            .map(|x| target(x, "local-hoard", "hoard"))
            .collect();
        for action in [Action::Recheck, Action::Pause(true), Action::Reannounce] {
            let job = new_job("test", targets.len());
            job.lock().unwrap().cancel = true;
            run(s.state.clone(), job.clone(), action, targets.clone()).await;
            let j = job.lock().unwrap();
            assert!(
                j.finished && j.cancelled,
                "finished, and flagged as cancelled"
            );
            assert_eq!((j.done, j.failed), (0, 0), "nothing was touched");
        }
        assert!(
            s.state
                .store
                .read()
                .unwrap()
                .paused_hashes("hoard")
                .unwrap()
                .is_empty(),
            "no pause was written"
        );
    }

    #[tokio::test]
    async fn the_job_routes_answer_unknown_ids_and_cancel_a_known_one() {
        let (s, _) = library("sel-job-routes");
        let r = get_job_route(
            State(s.state.clone()),
            Path("sel-nope".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = cancel_job(
            State(s.state.clone()),
            Path("sel-nope".into()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = cancel_job(
            State(s.state.clone()),
            Path("sel-nope".into()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let job = new_job("test", 1);
        let id = job.lock().unwrap().id.clone();
        let r = cancel_job(
            State(s.state.clone()),
            Path(id.clone()),
            RawQuery(None),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(
            !job.lock().unwrap().cancel,
            "a caller with no key cancels nothing"
        );
        let r = cancel_job(
            State(s.state.clone()),
            Path(id.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(job.lock().unwrap().cancel);
        // A running job reads its elapsed time live.
        let r = get_job_route(State(s.state.clone()), Path(id), RawQuery(None), keyed(KEY)).await;
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (v["finished"].clone(), v["total"].clone()),
            (json!(false), json!(1))
        );
        assert!(
            v.get("cancel").is_none() && v.get("started").is_none(),
            "internals are not serialised"
        );
    }

    #[tokio::test]
    async fn a_malformed_request_is_refused_before_a_job_exists() {
        let (s, h) = library("sel-malformed");
        for body in ["not json", "{}", r#"{"selection": {"items": []}}"#] {
            let r = post_action(
                State(s.state.clone()),
                Path("stop".into()),
                RawQuery(None),
                keyed(KEY),
                body.into(),
            )
            .await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{body}");
        }
        let r = post_action(
            State(s.state.clone()),
            Path("stop".into()),
            RawQuery(None),
            keyed(KEY),
            json!({"selection": {"items": [{"hash": h[0]}]}, "extra": 1}).to_string(),
        )
        .await;
        assert_eq!(
            r.status(),
            StatusCode::BAD_REQUEST,
            "an unknown top-level key is named, not dropped"
        );
        let r = post_action(
            State(s.state.clone()),
            Path("stop".into()),
            RawQuery(None),
            keyed(KEY),
            json!({"selection": {"filter": "", "expect": 1}}).to_string(),
        )
        .await;
        assert_eq!(
            r.status(),
            StatusCode::CONFLICT,
            "3 torrents against 1 confirmed"
        );
        let v: Value = serde_json::from_slice(
            &axum::body::to_bytes(r.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            (
                v["reason"].clone(),
                v["count"].clone(),
                v["expected"].clone()
            ),
            (json!("grew"), json!(3), json!(1))
        );
        assert!(
            s.state
                .store
                .read()
                .unwrap()
                .paused_hashes("hoard")
                .unwrap()
                .is_empty(),
            "the refusal touched nothing"
        );
    }

    // ---- a node that holds some of the copies ------------------------------------------

    type Calls = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A loopback Hydra standing in for the declared node `nas`: it records
    /// every request (method, path+query, body) and answers by the hash the
    /// path carries, so one test can show every kind of answer.
    ///   aaaa..  200 {}            bbbb..  202 {"kind":"graduate"}
    ///   cccc..  202 {}            dddd..  500 {"error":"disk full"}
    ///   eeee..  502 (no JSON)
    /// `/api/<engine>/page` answers the selection universe.
    struct Nas {
        url: String,
        calls: Calls,
        _stop: tokio::sync::oneshot::Sender<()>,
    }

    async fn nas() -> Nas {
        let calls: Calls = Arc::default();
        let log = calls.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: axum::http::Uri, body: String| {
            let log = log.clone();
            async move {
                let pq = uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
                log.lock().unwrap().push((method.to_string(), pq.clone(), body));
                if pq.starts_with("/api/hoard/page") {
                    let h = "aa".repeat(20);
                    return (StatusCode::OK, Json(json!({
                        "total": 1, "filtered": 1,
                        "copies": [{"hash": h, "agent": "local-hoard"}, {"hash": "bad", "agent": "local-hoard"}],
                    })))
                    .into_response();
                }
                if pq.contains(&"bb".repeat(20)) {
                    (StatusCode::ACCEPTED, Json(json!({"kind": "graduate"}))).into_response()
                } else if pq.contains(&"cc".repeat(20)) {
                    (StatusCode::ACCEPTED, Json(json!({}))).into_response()
                } else if pq.contains(&"dd".repeat(20)) {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "disk full"}))).into_response()
                } else if pq.contains(&"ee".repeat(20)) {
                    (StatusCode::BAD_GATEWAY, "upstream").into_response()
                } else {
                    (StatusCode::OK, Json(json!({}))).into_response()
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        Nas {
            url,
            calls,
            _stop: tx,
        }
    }

    fn declare_nas(s: &TestState, url: &str) {
        s.state
            .store
            .lock()
            .unwrap()
            .put_node(&crate::store::Node {
                name: "nas".into(),
                url: url.into(),
                api_key: "node-key".into(),
                enabled: true,
                added_at: 0,
            })
            .unwrap();
    }

    fn calls_of(n: &Nas) -> Vec<(String, String, Value)> {
        n.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, pq, _)| !pq.starts_with("/api/hoard/page"))
            .map(|(m, pq, b)| {
                (
                    m.clone(),
                    pq.clone(),
                    serde_json::from_str(b).unwrap_or(Value::Null),
                )
            })
            .collect()
    }

    fn remote(hash_pair: &str, engine: &str, mode: &str) -> Target {
        target(&hash_pair.repeat(20), &format!("nas-{engine}"), mode)
    }

    /// A filter over a fleet resolves to the node's copies too, labelled
    /// `<node>-<engine>`, and a hash the node made up is dropped.
    #[tokio::test]
    async fn a_filter_over_the_fleet_includes_the_nodes_copies_under_its_name() {
        let (s, h) = library("sel-fleet-filter");
        let n = nas().await;
        declare_nas(&s, &n.url);
        let got = resolve(
            &s.state,
            &sel(json!({"filter": "category=movies", "expect": 3})),
        )
        .await
        .unwrap();
        let mut copies: Vec<(String, String)> =
            got.into_iter().map(|t| (t.hash, t.agent)).collect();
        copies.sort();
        let mut want = vec![
            (h[0].clone(), "local-hoard".to_string()),
            (h[1].clone(), "local-hoard".to_string()),
            ("aa".repeat(20), "nas-hoard".to_string()),
        ];
        want.sort();
        assert_eq!(copies, want);
        let asked = n.calls.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert_eq!(
            asked[0].1, "/api/hoard/page?fields=hash&category=movies",
            "the filter travels to the node as written"
        );
        // Fewer than confirmed is fine; more is refused with the count.
        let r = resolve(
            &s.state,
            &sel(json!({"filter": "category=movies", "expect": 2})),
        )
        .await
        .unwrap_err();
        assert_eq!(
            (r.status, r.body["count"].clone()),
            (StatusCode::CONFLICT, json!(3))
        );
    }

    /// Every kind of answer a node gives is a distinct outcome, and a refusal
    /// carries the node's name and its reason.
    #[tokio::test]
    async fn a_remote_category_move_reports_how_the_node_answered() {
        let (s, _) = library("sel-remote-answers");
        let n = nas().await;
        declare_nas(&s, &n.url);
        let action = Action::Category {
            category: "movies".into(),
            move_files: true,
            allow_breaking_hardlinks: true,
        };
        let targets = vec![
            remote("aa", "hoard", "hoard"),
            remote("bb", "hoard", "hoard"),
            remote("cc", "hoard", "hoard"),
            remote("dd", "hoard", "hoard"),
            remote("ee", "hoard", "hoard"),
        ];
        let j = run_to_end(&s, action, targets).await;
        assert_eq!(j.done, 5);
        assert_eq!(j.tally.get("ok"), Some(&1), "{:?}", j.tally);
        assert_eq!(j.tally.get("graduating"), Some(&1), "202 + kind graduate");
        assert_eq!(j.tally.get("moving"), Some(&1), "202 alone");
        assert_eq!(j.tally.get("failed"), Some(&2));
        assert_eq!(j.failed, 2);
        assert!(
            j.errors
                .iter()
                .any(|e| e == &format!("{}: nas: disk full", "dddddddd")),
            "{:?}",
            j.errors
        );
        assert!(
            j.errors.iter().any(|e| e.starts_with("eeeeeeee: nas: 502")),
            "a bare status when the node sent no reason: {:?}",
            j.errors
        );
        let calls = calls_of(&n);
        assert_eq!(calls[0].0, "POST");
        assert_eq!(
            calls[0].1,
            format!(
                "/api/hoard/torrents/{}/category?engine=hoard",
                "aa".repeat(20)
            )
        );
        assert_eq!(
            calls[0].2,
            json!({"category": "movies", "move_files": true, "allow_breaking_hardlinks": true})
        );
    }

    /// Each action has its own route on the node, built for THAT copy: the
    /// engine of the row (encoded), the mode of the row, the body it needs.
    #[tokio::test]
    async fn each_action_reaches_the_nodes_own_route_for_that_copy() {
        let (s, _) = library("sel-remote-routes");
        let n = nas().await;
        declare_nas(&s, &n.url);
        let h = "aa".repeat(20);
        let race = remote("aa", "vpn 1", "race");
        let cases: Vec<(Action, Target, &str, String, Value)> = vec![
            (
                Action::Location {
                    location: "/srv/x".into(),
                    allow_breaking_hardlinks: false,
                },
                race.clone(),
                "POST",
                format!("/api/race/torrents/{h}/location?engine=vpn%201"),
                json!({"location": "/srv/x", "allow_breaking_hardlinks": false}),
            ),
            (
                Action::Reannounce,
                race.clone(),
                "POST",
                format!("/api/torrents/{h}/reannounce?engine=vpn%201"),
                Value::Null,
            ),
            (
                Action::Recheck,
                race.clone(),
                "POST",
                format!("/api/hoard/torrents/{h}/verify?engine=vpn%201"),
                Value::Null,
            ),
            (
                Action::Pause(true),
                race.clone(),
                "POST",
                "/api/engines/vpn%201/pause".to_string(),
                json!({"hashes": [h], "paused": true}),
            ),
            (
                Action::Limits {
                    up_kib: Some(10),
                    down_kib: None,
                },
                race.clone(),
                "POST",
                format!("/api/torrents/{h}/limits?engine=vpn%201"),
                json!({"up_kib": 10, "down_kib": null}),
            ),
            (
                Action::DownloadOrder {
                    sequential: Some(true),
                    first_last_piece: None,
                },
                race.clone(),
                "POST",
                format!("/api/torrents/{h}/download-order?engine=vpn%201"),
                json!({"sequential": true}),
            ),
            (
                Action::Remove { delete_files: true },
                race.clone(),
                "DELETE",
                format!("/api/torrents/{h}?delete_files=true&engine=vpn%201"),
                Value::Null,
            ),
        ];
        for (action, t, method, path, body) in cases {
            let before = calls_of(&n).len();
            let j = run_to_end(&s, action.clone(), vec![t]).await;
            assert_eq!(
                j.tally.get("ok"),
                Some(&1),
                "{action:?}: {:?} {:?}",
                j.tally,
                j.errors
            );
            let calls = calls_of(&n);
            assert_eq!(calls.len(), before + 1, "{action:?}: exactly one call");
            let (m, pq, b) = calls.last().unwrap();
            assert_eq!(
                (m.as_str(), pq.as_str()),
                (method, path.as_str()),
                "{action:?}"
            );
            assert_eq!(b, &body, "{action:?}");
        }
        // A copy that is not a race row goes to the hoard's routes.
        let h2 = "aa".repeat(20);
        run_to_end(
            &s,
            Action::Location {
                location: "/y".into(),
                allow_breaking_hardlinks: true,
            },
            vec![remote("aa", "hoard", "hoard")],
        )
        .await;
        assert_eq!(
            calls_of(&n).last().unwrap().1,
            format!("/api/hoard/torrents/{h2}/location?engine=hoard")
        );
    }

    /// What has no relay is reported as not here, and a copy whose node is
    /// not declared fails with the agent it asked for.
    #[tokio::test]
    async fn actions_without_a_relay_or_for_an_undeclared_node_are_not_sent() {
        let (s, _) = library("sel-remote-skip");
        let n = nas().await;
        declare_nas(&s, &n.url);
        for action in [
            Action::Copy {
                engine: "race".into(),
            },
            Action::MoveEngine {
                engine: "race".into(),
            },
            Action::Handoff {
                node: "nas".into(),
                engine: "race".into(),
                then: "keep".into(),
            },
        ] {
            let j = run_to_end(&s, action.clone(), vec![remote("aa", "hoard", "hoard")]).await;
            assert_eq!(j.tally.get("not_here"), Some(&1), "{action:?}");
        }
        assert!(calls_of(&n).is_empty(), "nothing was sent to the node");

        let ghost = target(&"aa".repeat(20), "elsewhere-hoard", "hoard");
        let j = run_to_end(&s, Action::Reannounce, vec![ghost]).await;
        assert_eq!(j.failed, 1);
        assert_eq!(
            j.errors,
            vec!["aaaaaaaa: no declared node holds elsewhere-hoard".to_string()]
        );
        // A set action that has no relay on a node is reported, not dropped.
        let j = run_to_end(
            &s,
            Action::Tags {
                tags: vec!["x".into()],
                op: "add".into(),
            },
            vec![remote("aa", "hoard", "hoard")],
        )
        .await;
        assert_eq!(j.tally.get("not_here"), Some(&1));
        assert!(calls_of(&n).is_empty());
    }

    /// A handoff that the node accepted is counted by what `then` promises:
    /// "remove" is only waiting on the node (the local copy goes once it holds
    /// it all), "keep" is done.
    #[tokio::test]
    async fn a_handoff_counts_waiting_node_when_it_moves_and_ok_when_it_keeps() {
        let (s, h) = library("sel-handoff-then");
        let n = nas().await;
        declare_nas(&s, &n.url);
        for (then, want, other) in [
            ("remove", "waiting_node", "ok"),
            ("keep", "ok", "waiting_node"),
        ] {
            let action = Action::Handoff {
                node: "nas".into(),
                engine: "race".into(),
                then: then.into(),
            };
            let j = run_to_end(&s, action, vec![target(&h[0], "local-hoard", "hoard")]).await;
            assert_eq!((j.done, j.failed), (1, 0), "{then}: {:?}", j.errors);
            assert_eq!(j.tally.get(want), Some(&1), "{then}: {:?}", j.tally);
            assert!(
                !j.tally.contains_key(other),
                "{then}: only {want}: {:?}",
                j.tally
            );
        }
    }

    /// A node that is down fails its copies with the node's name and the
    /// class of the failure, and the local ones go on.
    #[tokio::test]
    async fn a_node_that_is_down_fails_only_its_own_copies() {
        let (s, h) = library("sel-remote-down");
        declare_nas(&s, "http://127.0.0.1:1");
        let targets = vec![
            target(&h[0], "local-hoard", "hoard"),
            remote("aa", "hoard", "hoard"),
        ];
        let j = run_to_end(&s, Action::Pause(true), targets).await;
        assert_eq!(j.done, 2);
        assert_eq!(j.errors, vec!["aaaaaaaa: nas: injoignable".to_string()]);
        assert_eq!(j.tally.get("ok"), Some(&1));
        let (_, t) = crate::api::find_torrent(&s.state, &h[0]).unwrap();
        assert!(
            t.is_paused.load(std::sync::atomic::Ordering::Relaxed),
            "the local copy was stopped"
        );
    }

    /// Set-based edits over a mixed selection: a pause and a relabel are
    /// relayed to the node, a tag or pin is not (and a race row is skipped
    /// by them, as the single routes do).
    #[tokio::test]
    async fn a_set_edit_over_local_and_remote_copies_relays_only_what_the_node_can_take() {
        let (s, h) = library("sel-remote-set");
        let n = nas().await;
        declare_nas(&s, &n.url);
        let local = target(&h[2], "local-hoard", "hoard");
        let node_copy = remote("aa", "hoard", "hoard");

        let j = run_to_end(
            &s,
            Action::Pause(true),
            vec![local.clone(), node_copy.clone()],
        )
        .await;
        assert_eq!((j.done, j.failed), (2, 0), "{:?}", j.errors);
        assert_eq!(j.tally.get("ok"), Some(&2));
        assert_eq!(calls_of(&n)[0].1, "/api/engines/hoard/pause");

        let relabel = Action::Category {
            category: "movies".into(),
            move_files: false,
            allow_breaking_hardlinks: false,
        };
        let j = run_to_end(&s, relabel, vec![local.clone(), node_copy.clone()]).await;
        assert_eq!(j.tally.get("ok"), Some(&2), "{:?}", j.tally);
        let c = calls_of(&n);
        assert_eq!(
            c[1].1,
            format!(
                "/api/hoard/torrents/{}/category?engine=hoard",
                "aa".repeat(20)
            )
        );
        assert_eq!(
            c[1].2,
            json!({"category": "movies", "move_files": false, "allow_breaking_hardlinks": false})
        );

        let tags = Action::Tags {
            tags: vec!["t".into()],
            op: "add".into(),
        };
        let race_row = target(&h[0], "local-race", "race");
        let j = run_to_end(&s, tags, vec![local, node_copy, race_row]).await;
        assert_eq!(j.tally.get("ok"), Some(&1), "the local hoard row");
        assert_eq!(
            j.tally.get("not_here"),
            Some(&1),
            "the node's copy has no tag relay"
        );
        assert_eq!(j.tally.get("skipped"), Some(&1), "tags are the hoard's");
        assert_eq!(calls_of(&n).len(), 2, "no tag call was made");
        assert!(tags_of(&s, &h[2]).contains(&"t".to_string()));
    }

    /// A cancel seen between two remote relays stops the rest.
    #[tokio::test]
    async fn a_cancel_stops_the_relays_that_have_not_gone_yet() {
        let (s, _) = library("sel-remote-cancel");
        let n = nas().await;
        declare_nas(&s, &n.url);
        let job = new_job("test", 2);
        job.lock().unwrap().cancel = true;
        run(
            s.state.clone(),
            job.clone(),
            Action::Pause(true),
            vec![
                remote("aa", "hoard", "hoard"),
                remote("bb", "hoard", "hoard"),
            ],
        )
        .await;
        assert_eq!(job.lock().unwrap().done, 0);
        assert!(calls_of(&n).is_empty());
    }

    // ---- per-torrent actions on this node ----------------------------------------------

    #[tokio::test]
    async fn actions_that_fail_on_this_node_say_so_per_torrent() {
        let (s, h) = library("sel-local-fail");
        let t = |i: usize| target(&h[i], "local-hoard", "hoard");
        // Copying or moving into an engine that does not exist, handing off
        // to or fetching from a node nobody declared: each torrent fails with
        // its own reason, none succeeds by accident.
        for action in [
            Action::Copy {
                engine: "no-such-engine".into(),
            },
            Action::MoveEngine {
                engine: "no-such-engine".into(),
            },
            Action::NodeMove {
                node: "ghost".into(),
                engine: "race".into(),
            },
            Action::NodeFetch {
                node: "ghost".into(),
                from_engine: String::new(),
                engine: "race".into(),
            },
            Action::Handoff {
                node: "ghost".into(),
                engine: "race".into(),
                then: "remove".into(),
            },
        ] {
            let j = run_to_end(&s, action.clone(), vec![t(0), t(1)]).await;
            assert_eq!((j.done, j.failed), (2, 2), "{action:?}: {:?}", j.tally);
            assert_eq!(j.tally.get("failed"), Some(&2), "{action:?}");
            assert!(
                j.errors
                    .iter()
                    .all(|e| e.starts_with(&h[0][..8]) || e.starts_with(&h[1][..8])),
                "{:?}",
                j.errors
            );
            assert!(
                !j.tally.contains_key("ok") && !j.tally.contains_key("waiting_node"),
                "{action:?}"
            );
        }
        // Nothing was copied or moved on the way.
        assert!(s
            .state
            .engines
            .get("race")
            .unwrap()
            .manager
            .all()
            .is_empty());
        assert_eq!(s.state.engines.get("hoard").unwrap().manager.all().len(), 3);
    }

    /// Copying a torrent to another engine leaves the original where it is.
    #[tokio::test]
    async fn a_copy_to_another_engine_adds_it_there_and_keeps_the_original() {
        let (s, h) = library("sel-local-copy");
        let j = run_to_end(
            &s,
            Action::Copy {
                engine: "race".into(),
            },
            vec![target(&h[0], "local-hoard", "hoard")],
        )
        .await;
        assert_eq!(
            (j.failed, j.tally.get("ok").copied()),
            (0, Some(1)),
            "{:?}",
            j.errors
        );
        let race = s.state.engines.get("race").unwrap();
        assert!(
            race.manager
                .get(&typhon_engine::torrent::hex_decode(&h[0]).unwrap())
                .is_some(),
            "now in race"
        );
        let hoard = s.state.engines.get("hoard").unwrap();
        assert!(
            hoard
                .manager
                .get(&typhon_engine::torrent::hex_decode(&h[0]).unwrap())
                .is_some(),
            "and still in hoard"
        );
    }

    /// A hash this node does not hold fails on the local routes with the
    /// route's own words.
    #[tokio::test]
    async fn a_torrent_this_node_does_not_hold_fails_with_the_routes_reason() {
        let (s, _) = library("sel-local-unknown");
        let stranger = target(&"9f".repeat(20), "local-hoard", "hoard");
        for action in [
            Action::Recheck,
            Action::Remove {
                delete_files: false,
            },
            Action::Limits {
                up_kib: Some(1),
                down_kib: None,
            },
        ] {
            let j = run_to_end(&s, action.clone(), vec![stranger.clone()]).await;
            assert_eq!(j.failed, 1, "{action:?}: {:?}", j.tally);
            assert!(j.errors[0].starts_with("9f9f9f9f: "), "{:?}", j.errors);
        }
    }

    // ---- what this node's own routes answered, one torrent at a time -----------------

    /// A stand-in for this node's `/api`: records every request and answers by
    /// the hash the path carries, so each kind of answer a route gives is one
    /// torrent. `one` is handed it in place of the real router.
    ///   aa 200 {}                       bb 202 {"kind":"graduate"}
    ///   cc 202 {}                       dd 500 {"error":"disk full"}
    ///   ee 409 hardlinks (3 files, 4096 bytes)
    ///   ff 409 {"reason":"space"}       11 200 {"reason":"hardlinks"}
    ///   33 429 {"status":"cooldown"}    44 200 {"status":"in_flight"}
    ///   55 200 {"status":"queued"}      66 200 {"status":"ok"}
    ///   77 200 {"status":"weird"}       88 500 {"status":"weird"}
    fn stub_caller() -> (Caller, Calls) {
        let calls: Calls = Arc::default();
        let log = calls.clone();
        let router = axum::Router::new().fallback(move |method: Method, uri: axum::http::Uri, body: String| {
            let log = log.clone();
            async move {
                let pq = uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
                log.lock().unwrap().push((method.to_string(), pq.clone(), body));
                let has = |pair: &str| pq.contains(&pair.repeat(20));
                if has("bb") {
                    (StatusCode::ACCEPTED, Json(json!({"kind": "graduate"}))).into_response()
                } else if has("cc") {
                    (StatusCode::ACCEPTED, Json(json!({}))).into_response()
                } else if has("dd") {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "disk full"}))).into_response()
                } else if has("ee") {
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"reason": "hardlinks", "hardlinked_files": 3, "hardlinked_bytes": 4096})),
                    )
                        .into_response()
                } else if has("ff") {
                    (StatusCode::CONFLICT, Json(json!({"reason": "space", "error": "not enough free space"}))).into_response()
                } else if has("11") {
                    (StatusCode::OK, Json(json!({"reason": "hardlinks"}))).into_response()
                } else if has("33") {
                    (StatusCode::TOO_MANY_REQUESTS, Json(json!({"status": "cooldown"}))).into_response()
                } else if has("44") {
                    (StatusCode::OK, Json(json!({"status": "in_flight"}))).into_response()
                } else if has("55") {
                    (StatusCode::OK, Json(json!({"status": "queued"}))).into_response()
                } else if has("66") {
                    (StatusCode::OK, Json(json!({"status": "ok"}))).into_response()
                } else if has("77") {
                    (StatusCode::OK, Json(json!({"status": "weird"}))).into_response()
                } else if has("88") {
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"status": "weird"}))).into_response()
                } else {
                    (StatusCode::OK, Json(json!({}))).into_response()
                }
            }
        });
        (
            Caller {
                router,
                key: KEY.into(),
            },
            calls,
        )
    }

    fn category_move() -> Action {
        Action::Category {
            category: "movies".into(),
            move_files: true,
            allow_breaking_hardlinks: false,
        }
    }

    /// Every answer a local category or location route gives is its own
    /// outcome: a 200 is done, a 202 is a move (a graduation when the route
    /// says so), a refusal is a failure with the route's words, and a 409 about
    /// hardlinks -- only that one -- is a request for the operator's yes.
    #[tokio::test]
    async fn a_local_category_or_location_move_reports_each_answer_of_the_route() {
        let (s, _) = library("sel-one-answers");
        let (caller, _) = stub_caller();
        let move_location = Action::Location {
            location: "/srv/x".into(),
            allow_breaking_hardlinks: false,
        };
        for action in [category_move(), move_location] {
            for (pair, key) in [
                ("aa", "ok"),
                ("bb", "graduating"),
                ("cc", "moving"),
                ("dd", "failed"),
                ("ee", "needs_consent"),
                ("ff", "failed"),
                ("11", "ok"),
            ] {
                let o = one(
                    &s.state,
                    &caller,
                    &action,
                    &target(&pair.repeat(20), "local-hoard", "hoard"),
                )
                .await;
                assert_eq!(o.key, key, "{action:?} on {pair}");
                match pair {
                    "dd" => assert_eq!(
                        o.error.as_deref(),
                        Some(format!("{}: disk full", pair.repeat(4)).as_str())
                    ),
                    "ff" => assert_eq!(o.error.as_deref(), Some("ffffffff: not enough free space")),
                    "ee" => {
                        let (item, files, bytes) = o.consent.expect("the operator is asked");
                        assert_eq!(
                            (item.hash, item.agent, item.mode),
                            (
                                "ee".repeat(20),
                                "local-hoard".to_string(),
                                "hoard".to_string()
                            )
                        );
                        assert_eq!((files, bytes), (3, 4096));
                        assert!(o.error.is_none(), "waiting for a yes is not a failure");
                    }
                    _ => assert!(
                        o.consent.is_none() && (o.key == "failed") == o.error.is_some(),
                        "{pair}"
                    ),
                }
            }
        }
    }

    /// The route a local row's move is sent to is built from the row: the
    /// mode picks `race` or `hoard`, the agent names an extra engine (and
    /// `local`, or `local-` with nothing after, names none), and the body
    /// carries the action's own fields.
    #[tokio::test]
    async fn a_local_move_is_sent_to_the_route_of_the_rows_mode_and_engine() {
        let (s, _) = library("sel-one-routes");
        let (caller, calls) = stub_caller();
        let h = "aa".repeat(20);
        let cat = category_move();
        let loc = Action::Location {
            location: "/srv/x".into(),
            allow_breaking_hardlinks: true,
        };
        let cases = [
            (
                &cat,
                "local-hoard",
                "hoard",
                format!("/api/hoard/torrents/{h}/category?engine=hoard"),
            ),
            (
                &cat,
                "local-race",
                "race",
                format!("/api/race/torrents/{h}/category?engine=race"),
            ),
            (
                &cat,
                "local-vpn 1",
                "hoard",
                format!("/api/hoard/torrents/{h}/category?engine=vpn%201"),
            ),
            (
                &cat,
                "local-",
                "race",
                format!("/api/race/torrents/{h}/category"),
            ),
            (
                &cat,
                "local",
                "hoard",
                format!("/api/hoard/torrents/{h}/category"),
            ),
            (
                &cat,
                "",
                "hoard",
                format!("/api/hoard/torrents/{h}/category"),
            ),
            (
                &loc,
                "local-race",
                "race",
                format!("/api/race/torrents/{h}/location?engine=race"),
            ),
            (
                &loc,
                "local-",
                "hoard",
                format!("/api/hoard/torrents/{h}/location"),
            ),
        ];
        for (action, agent, mode, want) in cases {
            calls.lock().unwrap().clear();
            let o = one(&s.state, &caller, action, &target(&h, agent, mode)).await;
            assert_eq!(o.key, "ok", "{agent:?} {mode}");
            let sent = calls.lock().unwrap().clone();
            assert_eq!(sent.len(), 1, "exactly one call");
            assert_eq!(
                (sent[0].0.as_str(), sent[0].1.as_str()),
                ("POST", want.as_str()),
                "{agent:?} {mode}"
            );
            let body: Value = serde_json::from_str(&sent[0].2).unwrap();
            if std::ptr::eq(action, &cat) {
                assert_eq!(
                    body,
                    json!({"category": "movies", "move_files": true, "allow_breaking_hardlinks": false})
                );
            } else {
                assert_eq!(
                    body,
                    json!({"location": "/srv/x", "allow_breaking_hardlinks": true})
                );
            }
        }
    }

    /// Only a 409 that says `hardlinks` asks for consent: a 409 with another
    /// reason, or a hardlinks reason on a 200, is read for what it is.
    #[tokio::test]
    async fn only_a_conflict_about_hardlinks_asks_for_consent() {
        let (s, _) = library("sel-one-consent");
        let (caller, _) = stub_caller();
        async fn ask(s: &TestState, caller: &Caller, pair: &str) -> Outcome {
            one(
                &s.state,
                caller,
                &category_move(),
                &target(&pair.repeat(20), "local-hoard", "hoard"),
            )
            .await
        }
        assert!(ask(&s, &caller, "ee").await.consent.is_some());
        assert!(
            ask(&s, &caller, "ff").await.consent.is_none(),
            "a conflict over space is a failure"
        );
        assert_eq!(ask(&s, &caller, "ff").await.key, "failed");
        assert!(
            ask(&s, &caller, "11").await.consent.is_none(),
            "a 200 is not a conflict"
        );
        assert!(ask(&s, &caller, "aa").await.consent.is_none());
    }

    /// A reannounce is told by the route's own `status` word, whatever the
    /// HTTP code (a cooldown is a 429 and still not a failure); without one,
    /// the code decides.
    #[tokio::test]
    async fn a_reannounce_is_told_by_the_routes_status_word_then_by_its_code() {
        let (s, _) = library("sel-one-reannounce");
        let (caller, calls) = stub_caller();
        for (pair, key) in [
            ("33", "cooldown"),
            ("44", "in_flight"),
            ("55", "queued"),
            ("66", "ok"),
            ("77", "ok"),
            ("88", "failed"),
            ("dd", "failed"),
            ("aa", "ok"),
        ] {
            let o = one(
                &s.state,
                &caller,
                &Action::Reannounce,
                &target(&pair.repeat(20), "local-hoard", "hoard"),
            )
            .await;
            assert_eq!(o.key, key, "{pair}");
            assert_eq!(o.error.is_some(), key == "failed", "{pair}: {:?}", o.error);
        }
        let h = "33".repeat(20);
        assert_eq!(
            calls.lock().unwrap()[0].1,
            format!("/api/torrents/{h}/reannounce")
        );
    }

    /// A node's 202 is a graduation only when its body says so; the same
    /// answers, one torrent at a time, so a swap between the two shows.
    #[tokio::test]
    async fn a_nodes_202_is_a_graduation_only_when_it_says_so() {
        let (s, _) = library("sel-remote-202");
        let n = nas().await;
        declare_nas(&s, &n.url);
        for (pair, key) in [("aa", "ok"), ("bb", "graduating"), ("cc", "moving")] {
            let action = Action::Category {
                category: "movies".into(),
                move_files: true,
                allow_breaking_hardlinks: false,
            };
            let j = run_to_end(&s, action, vec![remote(pair, "hoard", "hoard")]).await;
            assert_eq!(j.tally.len(), 1, "{pair}: {:?}", j.tally);
            assert_eq!(j.tally.get(key), Some(&1), "{pair}: {:?}", j.tally);
        }
    }

    /// A batch half changed reports both halves, and a batch wholly changed or
    /// wholly unchanged reports only the one: no empty entry in the tally.
    #[tokio::test]
    async fn a_partly_changed_set_edit_counts_what_changed_and_what_did_not() {
        let (s, h) = library("sel-partial");
        let tally = |j: &Job| serde_json::to_value(&j.tally).unwrap();
        let all: Vec<Target> = h
            .iter()
            .map(|x| target(x, "local-hoard", "hoard"))
            .collect();
        let series = vec![all[2].clone()];

        let j = run_to_end(&s, Action::Pause(true), series).await;
        assert_eq!(
            (tally(&j), j.done),
            (json!({"ok": 1}), 1),
            "everything changed: no `unchanged` entry"
        );
        let j = run_to_end(&s, Action::Pause(true), all.clone()).await;
        assert_eq!(
            (tally(&j), j.done),
            (json!({"ok": 2, "unchanged": 1}), 3),
            "two changed, one was already stopped"
        );
        let j = run_to_end(&s, Action::Pause(true), all.clone()).await;
        assert_eq!(
            (tally(&j), j.done),
            (json!({"unchanged": 3}), 3),
            "nothing changed: no `ok` entry"
        );
        let j = run_to_end(&s, Action::Pause(false), all).await;
        assert_eq!(tally(&j), json!({"ok": 3}));
    }

    /// A job carries the action it was started for, and a running one reads
    /// its elapsed time live while a finished one keeps the time it took.
    #[tokio::test]
    async fn a_job_names_its_action_and_times_itself_while_running_only() {
        let (s, _) = library("sel-job-time");
        let running = new_job("category", 4);
        let finished = new_job("stop", 1);
        assert_eq!(running.lock().unwrap().action, "category");
        {
            let mut f = finished.lock().unwrap();
            f.finished = true;
            f.elapsed_ms = 7;
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let read = |job: &Shared| {
            let id = job.lock().unwrap().id.clone();
            let st = s.state.clone();
            async move {
                let r = get_job_route(State(st), Path(id), RawQuery(None), keyed(KEY)).await;
                serde_json::from_slice::<Value>(
                    &axum::body::to_bytes(r.into_body(), usize::MAX)
                        .await
                        .unwrap(),
                )
                .unwrap()
            }
        };
        let v = read(&running).await;
        assert_eq!(
            (v["action"].as_str(), v["total"].as_u64()),
            (Some("category"), Some(4))
        );
        let ms = v["elapsed_ms"].as_u64().unwrap();
        assert!(
            (50..60_000).contains(&ms),
            "a running job reads its clock: {ms}"
        );
        let v = read(&finished).await;
        assert_eq!(v["elapsed_ms"], 7, "a finished job keeps the time it took");
        assert_eq!(v["action"], "stop");
    }
}

#[cfg(test)]
mod remote_tests {
    use super::*;
    use crate::api::testing::state_from;

    /// A node name with a hyphen is found whole, and the longest declared
    /// name wins: `nas-2-race` is node `nas-2`, engine `race`, even with a
    /// node called `nas` declared too. 4.3 split on the first hyphen.
    #[test]
    fn a_remote_row_names_its_node_whole() {
        let s = state_from("remote-split", "");
        {
            let store = s.state.store.lock().unwrap();
            for name in ["nas", "nas-2"] {
                store
                    .put_node(&crate::store::Node {
                        name: name.into(),
                        url: "http://192.0.2.10:8199".into(),
                        api_key: "k".into(),
                        enabled: true,
                        added_at: 0,
                    })
                    .unwrap();
            }
        }
        let (n, e) = split_remote(&s.state, "nas-2-race").expect("found");
        assert_eq!((n.name.as_str(), e.as_str()), ("nas-2", "race"));
        let (n, e) = split_remote(&s.state, "nas-vpn-1").expect("found");
        assert_eq!((n.name.as_str(), e.as_str()), ("nas", "vpn-1"));
        assert!(split_remote(&s.state, "other-race").is_none());
    }
}
