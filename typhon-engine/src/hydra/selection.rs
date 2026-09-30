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
            None => self.agent.split_once('-').map(|(_, e)| e.to_string()).unwrap_or_default(),
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
        Refusal { status: StatusCode::BAD_REQUEST, body: json!({"error": msg.into()}) }
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
    Tags { tags: Vec<String>, op: String },
    Category { category: String, move_files: bool, allow_breaking_hardlinks: bool },
    Location { location: String, allow_breaking_hardlinks: bool },
    Reannounce,
    Recheck,
    Remove { delete_files: bool },
    Copy { engine: String },
    MoveEngine { engine: String },
    Handoff { node: String, engine: String, then: String },
    NodeFetch { node: String, from_engine: String, engine: String },
    NodeMove { node: String, engine: String },
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
                Action::Tags { tags: t.tags, op: t.op }
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
                Action::Location { location: l.location, allow_breaking_hardlinks: l.allow_breaking_hardlinks }
            }
            "reannounce" => {
                params::<NoParams>(p)?;
                Action::Reannounce
            }
            "recheck" => {
                params::<NoParams>(p)?;
                Action::Recheck
            }
            "remove" => Action::Remove { delete_files: params::<RemoveParams>(p)?.delete_files },
            "copy" => Action::Copy { engine: params::<EngineParams>(p)?.engine },
            "move-engine" => Action::MoveEngine { engine: params::<EngineParams>(p)?.engine },
            "handoff" => {
                let h: HandoffParams = params(p)?;
                if h.then != "keep" && h.then != "remove" {
                    return Err(Refusal::bad("params.then must be \"keep\" or \"remove\""));
                }
                Action::Handoff { node: h.node, engine: h.engine, then: h.then }
            }
            "node-fetch" => {
                let f: FetchParams = params(p)?;
                Action::NodeFetch { node: f.node, from_engine: f.from_engine, engine: f.engine }
            }
            "node-move" => {
                let m: NodeMoveParams = params(p)?;
                Action::NodeMove { node: m.node, engine: m.engine }
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

static JOBS: LazyLock<Mutex<HashMap<String, Shared>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

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
        !j.ended.is_some_and(|e| e.elapsed() > JOB_TTL)
    });
    jobs.insert(id, job.clone());
    job
}

fn get_job(id: &str) -> Option<Shared> {
    JOBS.lock().unwrap_or_else(|p| p.into_inner()).get(id).cloned()
}

/// What one torrent came to.
struct Outcome {
    key: String,
    error: Option<String>,
    consent: Option<(Item, u64, u64)>,
}

impl Outcome {
    fn ok(key: &str) -> Self {
        Outcome { key: key.into(), error: None, consent: None }
    }
    fn skip(key: &str) -> Self {
        Outcome::ok(key)
    }
    fn failed(hash: &str, why: String) -> Self {
        Outcome { key: "failed".into(), error: Some(format!("{}: {why}", &hash[..hash.len().min(8)])), consent: None }
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
        let mut req = axum::http::Request::builder().method(method).uri(uri).header("X-Api-Key", &self.key);
        let body = match body {
            Some(v) => {
                req = req.header(axum::http::header::CONTENT_TYPE, "application/json");
                axum::body::Body::from(v.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let Ok(req) = req.body(body) else {
            return (StatusCode::BAD_REQUEST, json!({"error": "unbuildable request"}));
        };
        let resp = match self.router.clone().oneshot(req).await {
            Ok(r) => r,
            Err(never) => match never {},
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20).await.unwrap_or_default();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    /// The route answered; say how, or why not.
    async fn simple(&self, method: Method, uri: &str, body: Option<Value>, hash: &str, ok: &str) -> Outcome {
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

async fn agent_action(c: &Caller, t: &Target, action: &str) -> Outcome {
    c.simple(
        Method::POST,
        &format!("/api/agents/{}/action", enc(&t.agent)),
        Some(json!({"engine": t.mode, "action": action, "info_hash": t.hash})),
        &t.hash,
        "sent",
    )
    .await
}

fn category_of(state: &AppState, hash: &str) -> String {
    let store = state.store.read().unwrap_or_else(|p| p.into_inner());
    store.export_rows(&[hash.to_string()]).ok().and_then(|r| r.into_iter().next()).map(|r| r.category).unwrap_or_default()
}

async fn one(state: &AppState, c: &Caller, action: &Action, t: &Target) -> Outcome {
    let h = t.hash.as_str();
    match action {
        Action::Pause(_) => unreachable!("set-based"),
        Action::Pin(_) | Action::Tags { .. } => unreachable!("set-based"),
        Action::Category { .. } | Action::Location { .. } => {
            let mode = if t.mode == "race" { "race" } else { "hoard" };
            let (route, body) = match action {
                Action::Category { category, move_files, allow_breaking_hardlinks } => ("category", json!({
                    "category": category,
                    "move_files": move_files,
                    "allow_breaking_hardlinks": allow_breaking_hardlinks,
                })),
                Action::Location { location, allow_breaking_hardlinks } => ("location", json!({
                    "location": location,
                    "allow_breaking_hardlinks": allow_breaking_hardlinks,
                })),
                _ => unreachable!("matched above"),
            };
            let (s, v) = c.call(Method::POST, &format!("/api/{mode}/torrents/{h}/{route}"), Some(body)).await;
            if s == StatusCode::CONFLICT && v.get("reason").and_then(Value::as_str) == Some("hardlinks") {
                let files = v.get("hardlinked_files").and_then(Value::as_u64).unwrap_or(0);
                let bytes = v.get("hardlinked_bytes").and_then(Value::as_u64).unwrap_or(0);
                let item = Item { hash: t.hash.clone(), agent: t.agent.clone(), mode: t.mode.clone() };
                return Outcome { key: "needs_consent".into(), error: None, consent: Some((item, files, bytes)) };
            }
            match s {
                StatusCode::ACCEPTED => Outcome::ok("moving"),
                s if s.is_success() => Outcome::ok("ok"),
                s => Outcome::failed(h, error_of(s, &v)),
            }
        }
        Action::Reannounce => {
            if !t.is_local() {
                return agent_action(c, t, "reannounce").await;
            }
            let (s, v) = c.call(Method::POST, &format!("/api/torrents/{h}/reannounce"), None).await;
            let status = v.get("status").and_then(Value::as_str).map(str::to_string);
            match status.as_deref() {
                Some(k @ ("ok" | "in_flight" | "cooldown" | "queued")) => Outcome::ok(k),
                _ if s.is_success() => Outcome::ok("ok"),
                _ => Outcome::failed(h, error_of(s, &v)),
            }
        }
        Action::Recheck => {
            if t.mode != "hoard" {
                return Outcome::skip("skipped");
            }
            if !t.is_local() {
                return agent_action(c, t, "verify").await;
            }
            c.simple(Method::POST, &format!("/api/hoard/torrents/{h}/verify"), None, h, "ok").await
        }
        Action::Remove { delete_files } => {
            c.simple(
                Method::DELETE,
                &format!("/api/torrents/{h}?delete_files={delete_files}&agent={}", enc(&t.agent)),
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
            c.simple(Method::POST, &format!("/api/torrents/{h}/copy"), Some(json!({"engine": engine})), h, "ok").await
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
            c.simple(
                Method::POST,
                &format!("/api/nodes/{}/handoff", enc(node)),
                Some(json!({"info_hash": h, "engine": engine, "category": category, "then": then})),
                h,
                "ok",
            )
            .await
        }
        Action::NodeFetch { node, from_engine, engine } => {
            let from = if from_engine.is_empty() { t.engine() } else { from_engine.clone() };
            c.simple(
                Method::POST,
                &format!("/api/nodes/{}/fetch", enc(node)),
                Some(json!({"info_hash": h, "engine": engine, "from_engine": from, "category": ""})),
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
    Tags { tags: Vec<String>, add: bool },
}

impl SetEdit {
    /// The actions that are nothing but a write to the store (and, for a
    /// pause, a flag in the engine): these never go torrent by torrent.
    fn of(action: &Action) -> Option<SetEdit> {
        match action {
            Action::Pause(p) => Some(SetEdit::Paused(*p)),
            Action::Pin(on) => Some(SetEdit::Pinned(*on)),
            Action::Tags { tags, op } => Some(SetEdit::Tags {
                tags: tags.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
                add: op == "add",
            }),
            // A relabel only. Moving files is real work per torrent, and a
            // category of the OTHER mode hands the torrent over: both stay on
            // the per-torrent route, which decides between them.
            Action::Category { category, move_files: false, .. } => Some(SetEdit::Category(category.clone())),
            _ => None,
        }
    }

    /// Whether this copy is one the edit applies to, as the single routes decide.
    fn applies(&self, t: &Target) -> bool {
        match self {
            SetEdit::Pinned(_) | SetEdit::Tags { .. } => t.mode == "hoard",
            SetEdit::Paused(_) | SetEdit::Category(_) => true,
        }
    }
}

/// A store-only action on the whole selection: a few transactions, not one
/// request per torrent. Copies on other nodes go through the agent relay for
/// a pause, and are reported as not here for the rest, as before.
async fn set_based(state: &AppState, c: &Caller, job: &Shared, edit: SetEdit, targets: &[Target]) {
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
            let o = agent_action(c, t, if p { "pause" } else { "resume" }).await;
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
            let changed = {
                let mut store = st.store.lock().unwrap_or_else(|p| p.into_inner());
                let edit = match &e {
                    SetEdit::Paused(p) => crate::store::BulkEdit::Paused(*p),
                    SetEdit::Pinned(p) => crate::store::BulkEdit::Pinned(*p),
                    SetEdit::Category(cat) => crate::store::BulkEdit::Category(cat),
                    SetEdit::Tags { tags, add } => crate::store::BulkEdit::Tags { tags, add: *add },
                };
                store.bulk_edit(&rows, edit)
            };
            // The engines, after the store and outside its lock: stopping a
            // torrent touches the engine and its announces, not the database.
            // Every copy, changed or not -- a row already marked stopped says
            // nothing about whether its engine agrees.
            if let (Ok(_), SetEdit::Paused(p)) = (&changed, &e) {
                for (h, engine) in &rows {
                    crate::api::apply_pause_to_engine(&st, engine, h, *p);
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
            Ok(Err(e)) => record(job, chunk.len(), Outcome { key: "failed".into(), error: Some(format!("store: {e}")), consent: None }),
            Err(e) => record(job, chunk.len(), Outcome { key: "failed".into(), error: Some(format!("store: {e}")), consent: None }),
        }
    }
}

async fn run(state: AppState, job: Shared, action: Action, targets: Vec<Target>) {
    let caller = Caller { router: crate::api::router(state.clone()), key: state.cfg().daemon.api_key.clone() };
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
    j.elapsed_ms = j.started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
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
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
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
        Err(e) => return Refusal::bad(format!("expected {{selection, params}}: {e}")).into_response(),
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
    (StatusCode::ACCEPTED, Json(json!({"job": id, "total": targets.len()}))).into_response()
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
                v.elapsed_ms = v.started.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0);
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
            let (h, _) = crate::api::add_torrent_bytes(&s.state, &torrent(name), cat, "/tmp", "", false, true, "hoard")
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
        let both = sel(json!({"items": [{"hash": h[0]}], "filter": "category=movies", "expect": 2}));
        assert_eq!(resolve(&s.state, &both).await.unwrap_err().status, StatusCode::BAD_REQUEST);
        let no_expect = sel(json!({"filter": "category=movies"}));
        assert_eq!(resolve(&s.state, &no_expect).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn a_filter_resolves_like_the_list_minus_its_exceptions() {
        let (s, h) = library("sel-filter");
        let got = resolve(&s.state, &sel(json!({"filter": "category=movies", "expect": 2}))).await.unwrap();
        let mut hashes: Vec<_> = got.iter().map(|t| t.hash.clone()).collect();
        hashes.sort();
        let mut want = vec![h[0].clone(), h[1].clone()];
        want.sort();
        assert_eq!(hashes, want);
        assert!(got.iter().all(|t| t.agent == "local-hoard" && t.mode == "hoard"));

        let minus = sel(json!({"filter": "category=movies", "exclude": [{"hash": h[0], "agent": "local"}], "expect": 2}));
        let got = resolve(&s.state, &minus).await.unwrap();
        assert_eq!(got.iter().map(|t| t.hash.clone()).collect::<Vec<_>>(), vec![h[1].clone()]);

        // "" is the whole list, and has to be said.
        let all = resolve(&s.state, &sel(json!({"filter": "", "expect": 3}))).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn a_filter_that_grew_past_what_was_confirmed_is_refused_with_the_new_count() {
        let (s, _) = library("sel-grew");
        let r = resolve(&s.state, &sel(json!({"filter": "", "expect": 2}))).await.unwrap_err();
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert_eq!(r.body["count"], 3);
        // Fewer than confirmed is fine: nothing unconfirmed is touched.
        assert_eq!(resolve(&s.state, &sel(json!({"filter": "", "expect": 10}))).await.unwrap().len(), 3);
    }

    async fn wait(s: &TestState, id: &str) -> Value {
        for _ in 0..200 {
            let r = get_job_route(State(s.state.clone()), Path(id.to_string()), RawQuery(None), keyed(KEY)).await;
            let v: Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
            if v["finished"] == true {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("job {id} never finished");
    }

    async fn start(s: &TestState, action: &str, body: Value) -> (StatusCode, Value) {
        let r = post_action(State(s.state.clone()), Path(action.into()), RawQuery(None), keyed(KEY), body.to_string()).await;
        let status = r.status();
        (status, serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap())
    }

    fn tags_of(s: &TestState, hash: &str) -> Vec<String> {
        let store = s.state.store.read().unwrap();
        store.export_rows(&[hash.to_string()]).unwrap().remove(0).tags
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
        assert!(!tags_of(&s, &h[2]).contains(&"noHL".to_string()), "series was not selected");
    }

    #[tokio::test]
    async fn a_stop_by_filter_is_one_store_write_and_reports_what_applied() {
        let (s, h) = library("sel-stop");
        let (st, v) = start(&s, "stop", json!({"selection": {"filter": "category=series", "expect": 1}})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["done"], 1, "{j}");
        assert_eq!(j["failed"], 0, "{j}");
        let paused: i64 = {
            let store = s.state.store.read().unwrap();
            store.paused_hashes("hoard").map(|p| p.contains(&h[2]) as i64 + 2 * p.contains(&h[0]) as i64).unwrap_or(-1)
        };
        assert_eq!(paused, 1, "the series torrent is stopped, the movies are not");
    }

    #[tokio::test]
    async fn explicit_items_still_work_and_an_unknown_action_or_param_is_refused() {
        let (s, h) = library("sel-items");
        let (st, v) = start(&s, "remove", json!({"selection": {"items": [{"hash": h[2], "agent": "local-hoard"}]}})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["tally"]["ok"], 1, "{j}");
        assert!(s.state.store.read().unwrap().export_rows(&[h[2].clone()]).unwrap().is_empty());

        let (st, _) = start(&s, "explode", json!({"selection": {"items": [{"hash": h[0]}]}})).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, v) = start(&s, "remove", json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"delete": true}})).await;
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
        let r = get_job_route(State(s.state.clone()), Path("x".into()), RawQuery(None), HeaderMap::new()).await;
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
        let (_, v) = start(&s, "stop", json!({"selection": {"filter": "category=movies", "expect": 2}})).await;
        wait(&s, v["job"].as_str().unwrap()).await;
        let (_, v) = start(&s, "category", json!({"selection": {"items": [{"hash": h[2]}]}, "params": {"category": "movies"}})).await;
        wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(dirty_rows(&s), 0, "the writes left rows for the list to re-read");
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
        let (st, _) = start(&s, "location", json!({"selection": {"items": [{"hash": h[0]}]}, "params": {"location": " "}})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = start(&s, "location", json!({"selection": {"items": [{"hash": h[0]}, {"hash": h[1]}]},
                                                   "params": {"location": to.to_string_lossy()}})).await;
        assert_eq!(st, StatusCode::ACCEPTED, "{v}");
        let j = wait(&s, v["job"].as_str().unwrap()).await;
        assert_eq!(j["tally"]["moving"], 2, "{j}");
    }

    #[tokio::test]
    async fn a_second_identical_write_is_reported_unchanged_not_done() {
        let (s, _) = library("sel-unchanged");
        for expected in ["ok", "unchanged"] {
            let (_, v) = start(&s, "stop", json!({"selection": {"filter": "", "expect": 3}})).await;
            let j = wait(&s, v["job"].as_str().unwrap()).await;
            assert_eq!(j["tally"][expected], 3, "{j}");
        }
    }
}
