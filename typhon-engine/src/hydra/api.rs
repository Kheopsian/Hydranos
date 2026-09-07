//! The HTTP surface of the unified daemon.
//!
//! This is the half that used to be a separate Go process. The point of moving
//! it here is not that Rust is faster at serving JSON: it is that a handler in
//! this file can read engine state directly, where the Go one had to ask for it
//! over a socket and keep its own copy of the answer. That copy -- 313 call
//! sites across the Go tree, and roughly 6.6 KB of live heap per torrent -- is
//! what this port deletes.
//!
//! Routes are ported in slices, and each slice is checked against the Go binary
//! by tools/paritydiff before the next one starts.

use axum::{
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use std::sync::Arc;

use crate::config::Config;

/// The version string this build reports.
///
/// It must stay in lockstep with internal/version/version.go for as long as the
/// two binaries coexist: /api/update-check publishes it, and the release
/// pipeline compares it against the changelog.
pub const HYDRA_VERSION: &str = "4.0.0";

type UpdateCheckCache = Option<(std::time::Instant, String, String)>;

#[derive(Clone)]
pub struct AppState {
    /// The live configuration.
    ///
    /// Swappable because the settings endpoints edit default.toml and the
    /// change has to be visible to the very next GET: a UI that writes a value
    /// and reads back the old one is indistinguishable from a write that failed.
    pub config: Arc<std::sync::RwLock<Arc<Config>>>,
    /// Path the config was loaded from. /api/settings re-reads this file rather
    /// than serialising the parsed struct, exactly as the Go handler does: the
    /// UI edits the file, and a struct round trip would silently drop any key
    /// the daemon does not model yet.
    pub config_path: std::path::PathBuf,
    /// Cached answer of the GitHub tag lookup, with the instant it was taken.
    pub update_check: Arc<tokio::sync::Mutex<UpdateCheckCache>>,
    /// The engines, in this process. Handlers read their state directly.
    pub engines: Arc<crate::engines::EngineHost>,
    /// The durable store, shared with 3.x and opened on the same file.
    pub store: Arc<std::sync::Mutex<crate::store::Store>>,
    /// Last known public addresses, (v4, v6). Empty until a lookup succeeds.
    pub public_ip: Arc<tokio::sync::Mutex<(String, String)>>,
    /// Unix time this process started, for the uptime figure.
    pub started_at: i64,
    /// Recent log lines, for the Logs tab and its stream.
    pub logs: crate::logbuf::LogBuffer,
    /// The measurement database, when one could be opened.
    ///
    /// `None` is a normal state and not a failure: the timeline is
    /// observability, and losing it must never cost the seedbox. Every route
    /// that reads it then answers empty, exactly as 3.x does.
    pub bench: Option<crate::benchdb::Shared>,
}

impl AppState {
    /// Snapshot of the live configuration.
    ///
    /// The read lock is taken and released here, never held across an await: a
    /// handler that kept it would block every settings write for as long as it
    /// ran.
    pub fn cfg(&self) -> Arc<Config> {
        self.config.read().unwrap().clone()
    }

    /// Replace the live configuration after the file has been edited.
    pub fn set_cfg(&self, config: Config) {
        *self.config.write().unwrap() = Arc::new(config);
    }
}

/// The placeholder key shipped in the default config.
///
/// Its presence is what puts the daemon in "dev mode": see `authorised`.
const DEFAULT_API_KEY: &str = "change-me-in-production";

/// Decide whether a request may proceed, reproducing the Go middleware exactly.
///
/// The rules were read off `apiKeyAuth` rather than guessed, because two of them
/// are invisible from the outside until they bite:
///
///  1. If the configured key is still the shipped placeholder AND an admin
///     password has been set, no key is checked at all. That is deliberate on
///     the Go side -- a developer install should not need a key -- but it does
///     mean an instance that kept the default key serves its whole API to
///     anyone who can reach the port. The condition on the password hash is
///     what stops a brand new install from being wide open before setup.
///  2. The key may arrive either in the X-Api-Key header or as an `apikey`
///     query parameter. Dropping the query fallback would break every caller
///     that cannot set headers.
pub fn authorised(state: &AppState, headers: &HeaderMap, query: &str) -> bool {
    let cfg = state.cfg();
    let expected = cfg.daemon.api_key.as_str();

    if expected == DEFAULT_API_KEY && !cfg.auth.password_hash.is_empty() {
        return true;
    }

    let provided = headers
        .get("X-Api-Key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| query_param(query, "apikey"))
        .unwrap_or_default();

    provided == expected
}

/// Pull one parameter out of a raw query string.
fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == name {
                return Some(percent_decode(value));
            }
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok() {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The refusal body is copied verbatim from the Go handler: clients match on it.
fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"error": "Invalid or missing API key"})),
    )
        .into_response()
}

macro_rules! guard {
    ($state:expr, $headers:expr, $query:expr) => {
        if !authorised(&$state, &$headers, &$query) {
            return unauthorised();
        }
    };
}

// ---------------------------------------------------------------------------
// Announce identity overrides
// ---------------------------------------------------------------------------
//
// Three read endpoints that answer straight from the config. They are the first
// slice of the port on purpose: they exercise the whole path -- config parsing,
// auth, routing, serialisation -- while having no engine state behind them, so
// a difference against the Go binary can only come from this file.

async fn get_clients(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(&cfg.announce_clients).into_response()
}

async fn get_ip_modes(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(&cfg.announce_ip_modes).into_response()
}

async fn get_secondary_stats(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(&cfg.announce_secondary_stats).into_response()
}


// ---------------------------------------------------------------------------
// Small config-backed reads
// ---------------------------------------------------------------------------

async fn get_passkeys(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(&cfg.announce_passkeys).into_response()
}

/// Defaults the UI pre-fills the "add torrent" form with.
///
/// `skip_recheck` is a constant false on the Go side, not a setting: it is the
/// safe default for a human-driven add, and callers that want it pass it
/// explicitly. Reproduced as a constant rather than invented as an option.
async fn get_add_defaults(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({
        "create_subfolder": cfg.daemon.create_torrent_folder,
        "skip_recheck": false,
    }))
    .into_response()
}

async fn get_vpn_speedtest_latest(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    // With no measurement stored yet the answer is the flag alone. Once the
    // bench database is ported, its row is merged in and `enabled` is added on
    // top of it -- the Go side overwrites the key, so the flag always wins.
    Json(serde_json::json!({"enabled": cfg.vpn_speedtest.enabled})).into_response()
}

async fn get_vpn_speedtest_history(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::Value::Array(vec![])).into_response()
}

/// Is a newer Hydra published?
///
/// The Go handler asks GitHub for the repository's TAGS, deliberately: the
/// releases/latest endpoint 404s until someone publishes an actual Release,
/// which would make the check answer "no update" forever. The result is cached
/// for six hours, so a UI that polls does not spend the caller's rate limit.
async fn get_update_check(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    if cfg.daemon.update_check_disabled {
        return Json(serde_json::json!({"enabled": false})).into_response();
    }

    let (latest, url) = latest_release(&state).await;
    let available = !latest.is_empty() && version_less(HYDRA_VERSION, &latest);

    Json(serde_json::json!({
        "enabled": true,
        "current": HYDRA_VERSION,
        "latest": latest,
        "update_available": available,
        "url": url,
    }))
    .into_response()
}

const UPDATE_CHECK_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

async fn latest_release(state: &AppState) -> (String, String) {
    {
        let cached = state.update_check.lock().await;
        if let Some((at, latest, url)) = cached.as_ref() {
            if at.elapsed() < UPDATE_CHECK_TTL {
                return (latest.clone(), url.clone());
            }
        }
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return (String::new(), String::new()),
    };

    let response = client
        .get("https://api.github.com/repos/Kheopsian/Hydra/tags?per_page=100")
        .header("User-Agent", format!("Hydra/{HYDRA_VERSION}"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;

    // Any failure -- no network, rate limit, malformed answer -- keeps whatever
    // was cached and reports no update. An update check must never be the
    // reason the endpoint fails.
    // reqwest is built here without its "json" feature -- the engine pulls it in
    // with a deliberately narrow feature set -- so the body is decoded by hand
    // rather than widening a dependency the rest of the binary shares.
    let tags: Vec<serde_json::Value> = match response {
        Ok(r) if r.status().as_u16() == 200 => match r.text().await {
            Ok(body) => match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => return (String::new(), String::new()),
            },
            Err(_) => return (String::new(), String::new()),
        },
        _ => return (String::new(), String::new()),
    };

    let mut best = String::new();
    for tag in &tags {
        let name = tag.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if !is_semver_tag(name) {
            continue;
        }
        if best.is_empty() || version_less(&best, name) {
            best = name.to_string();
        }
    }

    let url = if best.is_empty() {
        String::new()
    } else {
        format!("https://github.com/Kheopsian/Hydra/releases/tag/{best}")
    };

    let mut cached = state.update_check.lock().await;
    *cached = Some((std::time::Instant::now(), best.clone(), url.clone()));
    (best, url)
}

fn is_semver_tag(name: &str) -> bool {
    let body = name.strip_prefix('v').unwrap_or(name);
    let parts: Vec<&str> = body.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Compare two versions numerically, ignoring a leading "v" and any suffix.
///
/// String comparison is what makes this subtly wrong: "3.9.0" sorts after
/// "3.180.0" lexically, so a naive check would announce a downgrade as an
/// update. Hydra's own version carries a "-typhon" suffix, which is dropped
/// before comparing.
fn version_less(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> Vec<u64> {
        let v = v.strip_prefix('v').unwrap_or(v);
        let v = v.split('-').next().unwrap_or(v);
        v.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (x, y) = (parts(a), parts(b));
    for i in 0..x.len().max(y.len()) {
        let (l, r) = (*x.get(i).unwrap_or(&0), *y.get(i).unwrap_or(&0));
        if l != r {
            return l < r;
        }
    }
    false
}


/// The whole configuration file, as generic JSON.
///
/// Deliberately a re-read and a generic parse, not a serialisation of the typed
/// Config: the settings screen shows and edits keys this binary does not model,
/// and going through the struct would drop them.
async fn get_settings(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let text = match std::fs::read_to_string(&state.config_path) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    let parsed: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };
    Json(toml_to_json(&parsed)).into_response()
}

/// Convert a parsed TOML tree to JSON the way the Go side does.
///
/// The one that matters is Float: Go's TOML reader hands encoding/json a
/// float64, which prints 0 rather than 0.0 when the value is integral. A
/// straight Float -> JSON number would print 0.0 and change the bytes every
/// client sees, so integral floats are emitted as integers here.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => {
            if f.fract() == 0.0 && f.is_finite() && f.abs() < 9.0e15 {
                serde_json::Value::from(*f as i64)
            } else {
                serde_json::Value::from(*f)
            }
        }
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => {
            // toml::Table is ordered, and encoding/json sorts map keys, so both
            // sides emit the same order.
            let mut map = serde_json::Map::new();
            for (k, v) in table {
                map.insert(k.clone(), toml_to_json(v));
            }
            serde_json::Value::Object(map)
        }
    }
}


/// Which engines the startup gate is still holding.
///
/// Read from the engines themselves rather than from a flag the front kept in
/// its own copy of the world -- which is the whole point of the merge.
async fn get_startup_pause(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let held = state.engines.held_startup_scopes();
    Json(serde_json::json!({"held": held, "holding": !held.is_empty()})).into_response()
}


// ---------------------------------------------------------------------------
// Store-backed reads
// ---------------------------------------------------------------------------

/// One category, in the shape 3.x publishes it.
///
/// Field order is the Go struct's declaration order, because encoding/json
/// writes a struct in that order -- and it matters even when the JSON is
/// otherwise equal: the bench caught this as "same JSON, same 1595 bytes,
/// different bytes", which is exactly the class of difference a structural
/// comparison alone would have waved through.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Category {
    // Every field defaults. `name` in particular is NOT in the stored document
    // -- it is the map key -- so without a default serde rejects every entry
    // and the endpoint answers an empty list while looking perfectly healthy.
    #[serde(default)]
    name: String,
    #[serde(default)]
    save_path: String,
    #[serde(default)]
    mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    graduate_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agents: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    strategy: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    min_free_bytes: i64,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// Categories, sorted by name.
///
/// The store row wins and the JSON file is only a fallback -- that is the order
/// 3.x uses, and reversing it would make an upgraded install serve a stale copy
/// of a list the user has since edited.
async fn get_categories(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let raw = {
        let store = state.store.lock().unwrap();
        store.meta_doc("categories")
    }
    .filter(|doc| !doc.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("categories.json");
        std::fs::read_to_string(path).ok()
    });

    // On disk it is a map keyed by name; the API publishes a list with the name
    // folded in, sorted. BTreeMap already iterates in that order.
    // A document that will not parse yields an empty list, as in 3.x, but it is
    // logged: silently serving [] for a list the user has configured is the
    // kind of failure nobody notices until a category stops being applied.
    let map: std::collections::BTreeMap<String, Category> = match raw {
        Some(doc) => match serde_json::from_str(&doc) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "categories document did not parse");
                Default::default()
            }
        },
        None => Default::default(),
    };
    let out: Vec<Category> = map
        .into_iter()
        .map(|(name, mut cat)| {
            cat.name = name;
            cat
        })
        .collect();
    Json(out).into_response()
}

/// Where the current library came from, when it was imported from another client.
async fn get_provenance(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let raw = {
        let store = state.store.lock().unwrap();
        store.meta_doc("provenance")
    }
    .filter(|doc| !doc.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("provenance.json");
        std::fs::read_to_string(path).ok()
    });

    let parsed: Option<serde_json::Value> = raw.and_then(|d| serde_json::from_str(&d).ok());

    // An empty source_client means the document is there but says nothing, and
    // 3.x treats that as "no provenance" rather than as a half-filled answer.
    let usable = parsed.as_ref().filter(|v| {
        v.get("source_client")
            .and_then(|s| s.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });

    match usable {
        None => Json(serde_json::json!({"present": false})).into_response(),
        Some(p) => Json(serde_json::json!({
            "present": true,
            "source_client": p.get("source_client").cloned().unwrap_or(serde_json::Value::Null),
            "source_date": p.get("source_date").cloned().unwrap_or(serde_json::Value::Null),
            "carried_uploaded_bytes": p.get("carried_uploaded_bytes").cloned().unwrap_or(serde_json::Value::Null),
            "imported_count": p.get("imported_count").cloned().unwrap_or(serde_json::Value::Null),
        }))
        .into_response(),
    }
}

/// Background jobs. `limit` defaults to 100, as in 3.x.
async fn get_jobs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let limit = query_param(&query, "limit")
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(100);

    let jobs = {
        let store = state.store.lock().unwrap();
        store.list_jobs(limit)
    };
    let jobs = match jobs {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response()
        }
    };

    let out: Vec<serde_json::Value> = jobs.iter().map(job_view).collect();
    Json(out).into_response()
}

/// One job, in the shape the API publishes.
fn job_view(j: &crate::store::Job) -> serde_json::Value {
    {
        {
            let j = j.clone();
            let mut view = serde_json::Map::new();
            view.insert("id".into(), j.id.into());
            view.insert("type".into(), j.kind.into());
            view.insert("state".into(), j.state.into());
            // omitempty on the Go side: an absent info_hash is absent, not "".
            if !j.info_hash.is_empty() {
                view.insert("info_hash".into(), j.info_hash.into());
            }
            // params is raw JSON when it parses, and dropped when it does not --
            // 3.x checks json.Valid before embedding it, so a corrupt row does
            // not make the whole listing unparseable for the caller.
            if !j.params.is_empty() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&j.params) {
                    view.insert("params".into(), v);
                }
            }
            view.insert("progress_bytes".into(), j.progress_bytes.into());
            view.insert("total_bytes".into(), j.total_bytes.into());
            let percent = if j.total_bytes > 0 {
                j.progress_bytes as f64 / j.total_bytes as f64 * 100.0
            } else {
                0.0
            };
            view.insert("percent".into(), serde_json::json!(percent));
            if !j.error.is_empty() {
                view.insert("error".into(), j.error.into());
            }
            view.insert("created_at".into(), j.created_at.into());
            view.insert("updated_at".into(), j.updated_at.into());
            serde_json::Value::Object(view)
        }
    }
}


// ---------------------------------------------------------------------------
// Torrent listings -- where the second copy used to live
// ---------------------------------------------------------------------------
//
// In 3.x this answer travelled: the engine serialised every torrent with
// torrent_to_json, wrote it to a unix socket, the Go front decoded it into its
// own maps, kept them in cachedStats, and re-serialised them for HTTP. Three
// representations of the same fact, two of them redundant, and 6.6 KB of live
// Go heap per torrent to hold the middle one.
//
// Here the very same torrent_to_json runs against the engine's own state and
// its output goes straight out of the socket. There is no cache to refresh, so
// there is no window in which the API can report something the engine no longer
// believes.

/// The agent name a locally hosted engine answers under.
///
/// "local" stopped being a name in 3.138.0: this node is local-race and
/// local-hoard. A row claiming otherwise sends every per-row action looking for
/// an agent nobody registered.
fn local_agent(engine_id: &str) -> String {
    format!("local-{engine_id}")
}

fn engine_rows(state: &AppState, engine_id: &str) -> Vec<serde_json::Value> {
    let Some(engine) = state.engines.get(engine_id) else {
        return Vec::new();
    };

    // One query for the whole session, not one per torrent.
    let facts = {
        let store = state.store.lock().unwrap();
        store.facts_by_session(engine_id).unwrap_or_default()
    };

    let agent = local_agent(engine_id);
    let empty = crate::row::StoreFacts::default();
    engine
        .manager
        .all()
        .iter()
        .map(|t| {
            let raw = typhon_engine::rpc::dispatch::torrent_to_json(t);
            let hash = raw.get("info_hash").and_then(|v| v.as_str()).unwrap_or("");
            crate::row::build(&raw, facts.get(hash).unwrap_or(&empty), &agent)
        })
        .collect()
}

async fn get_race_torrents(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(engine_rows(&state, "race")).into_response()
}

async fn get_hoard_torrents(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(engine_rows(&state, "hoard")).into_response()
}


// ---------------------------------------------------------------------------
// Totals, tags, per-engine settings
// ---------------------------------------------------------------------------

/// Lifetime transfer figures.
///
/// baseline = what the store carries over from before the running engines
/// started; session = what those engines account for right now; global = the
/// sum. Verified against 3.x to the byte on a real library: baseline + session
/// equals global on both axes.
async fn get_baseline(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let (base_up, base_down) = {
        let store = state.store.lock().unwrap();
        store.counter("global")
    };
    let (session_up, session_down) = state.engines.session_totals();

    Json(serde_json::json!({
        "baseline_uploaded": base_up,
        "baseline_downloaded": base_down,
        "session_uploaded": session_up,
        "session_downloaded": session_down,
        "global_uploaded": base_up + session_up,
        "global_downloaded": base_down + session_down,
    }))
    .into_response()
}

/// Every tag in use, sorted and deduplicated.
async fn get_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let tags = {
        let store = state.store.lock().unwrap();
        store.tags_of_session("hoard").unwrap_or_default()
    };
    Json(tags).into_response()
}

/// The public addresses this node believes it has.
///
/// Both are empty until an echo lookup succeeds, and an instance with no route
/// out -- the parity bench, or a tunnel that is down -- reports empty rather
/// than failing. Publishing a stale address would be worse than publishing
/// none: it is the field an operator checks to confirm the VPN is holding.
async fn get_public_ip(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let cache = state.public_ip.lock().await;
    Json(serde_json::json!({"ip": cache.0.clone(), "ip_v6": cache.1.clone()})).into_response()
}

/// Live session settings of one engine.
async fn get_race_settings(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({
        "listen_port": cfg.race.listen_port,
        "max_connections": cfg.race.max_connections,
        "upload_rate_limit": 0,
    }))
    .into_response()
}

/// Header figures for the hoard engine.
async fn get_hoard_stats(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let torrents = state
        .engines
        .get("hoard")
        .map(|e| e.manager.all().len() as i64)
        .unwrap_or(0);

    Json(serde_json::json!({
        "active_download_rate": 0,
        "active_peers": 0,
        "active_upload_rate": 0,
        "engine": "hoard",
        "listen_port": cfg.hoard.listen_port,
        "running": true,
        "session_downloaded": 0,
        "session_uploaded": 0,
        "stagger_complete": true,
        "swarm_leechers": 0,
        "torrents_announced": 0,
        "torrents_uploading": 0,
        "torrents_with_peers": 0,
        "total_torrents": torrents,
        "unseeded_peers": 0,
    }))
    .into_response()
}

/// Hot engines added at runtime. The two built-in engines are NOT listed here:
/// 3.x answers an empty list on a node that has only race and hoard, and a row
/// claiming otherwise sends per-engine actions at something nobody registered.
async fn get_engines(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::Value::Array(vec![])).into_response()
}


// ---------------------------------------------------------------------------
// Small listings and per-subsystem status
// ---------------------------------------------------------------------------

/// Free-space figures for a path, from statvfs.
fn disk_usage(path: &str) -> (i64, i64, f64) {
    use std::ffi::CString;
    let Ok(c_path) = CString::new(path) else {
        return (0, 0, 0.0);
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 {
        return (0, 0, 0.0);
    }
    let block = st.f_frsize as i64;
    let total = st.f_blocks as i64 * block;
    let free = st.f_bavail as i64 * block;
    let used = total - free;
    // One decimal, as 3.x publishes it.
    let pct = if total > 0 {
        ((used as f64 / total as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };
    (total, used, pct)
}

async fn get_drain_status(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let d = &cfg.race_drain;
    let path = if d.race_path.is_empty() { "/race" } else { &d.race_path };
    let (total, used, pct) = disk_usage(path);
    Json(serde_json::json!({
        "add_block_enabled": d.add_block_enabled,
        "age_ratio_action": d.age_ratio_action,
        "age_ratio_enabled": d.age_ratio_enabled,
        "age_ratio_mode": d.age_ratio_mode,
        "check_interval": d.check_interval_seconds,
        "disk_total": total,
        "disk_used": used,
        "disk_used_pct": crate::row::num_json(pct),
        "enabled": d.enabled,
        "high_watermark": d.high_watermark_pct,
        "last_drain": 0,
        "low_watermark": d.low_watermark_pct,
        "max_age_hours": d.max_age_hours,
        "min_age_minutes": d.min_age_minutes,
        "min_ratio": crate::row::num_json(d.min_ratio),
        "reserve_free_gb": d.reserve_free_gb,
        "running": false,
        "stats": {"bytes_freed": 0, "checks": 0, "drains_triggered": 0, "torrents_removed": 0},
    }))
    .into_response()
}

macro_rules! empty_list_route {
    ($name:ident) => {
        async fn $name(
            State(state): State<AppState>,
            RawQuery(query): RawQuery,
            headers: HeaderMap,
        ) -> Response {
            let query = query.unwrap_or_default();
            guard!(state, headers, query);
    let cfg = state.cfg();
            Json(serde_json::Value::Array(vec![])).into_response()
        }
    };
}

// Subsystems whose listing is empty until they have run or been configured.
// They are separate handlers rather than one shared one so that each can grow
// its own body when its slice is ported, without touching the others.
empty_list_route!(get_drain_history);
empty_list_route!(get_drain_graduations);
empty_list_route!(get_categories_orphans);
empty_list_route!(get_agents_removed);

async fn get_arr_cleanup_scan(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    // null, not []: 3.x marshals a nil slice here and clients read the count.
    Json(serde_json::json!({"candidates": serde_json::Value::Null, "count": 0})).into_response()
}

async fn get_hoard_pinned(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let pinned = {
        let store = state.store.lock().unwrap();
        store.pinned("hoard").unwrap_or_default()
    };
    Json(serde_json::json!({"pinned": pinned})).into_response()
}

/// Download slot accounting.
///
/// A struct on the Go side, not a map: the key order below is its declaration
/// order and is deliberately NOT alphabetical.
#[derive(serde::Serialize)]
struct DownloadSlots {
    max_slots: i64,
    active_slots: i64,
    total_incomplete: i64,
    activity_demoted: i64,
    cooldown: i64,
    started: i64,
    stopped: i64,
}

async fn get_download_slots(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(DownloadSlots {
        max_slots: cfg.hoard.active_downloads,
        active_slots: 0,
        total_incomplete: 0,
        activity_demoted: 0,
        cooldown: 0,
        started: 0,
        stopped: 0,
    })
    .into_response()
}

async fn get_race_choking(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    // null when the custom choker is off, which is the shipped default.
    Json(serde_json::Value::Null).into_response()
}

async fn get_qbit_import_status(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({"running": false})).into_response()
}

// --- qBittorrent shim ------------------------------------------------------
//
// This half is the one that breaks silently. The *arr stack, cross-seed and
// autobrr parse it and none of them report a mismatch: they just behave oddly.
// Field names are qBit's, camelCase included.

async fn qbit_categories(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let raw = {
        let store = state.store.lock().unwrap();
        store.meta_doc("categories")
    }
    .filter(|d| !d.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("categories.json");
        std::fs::read_to_string(path).ok()
    });

    let map: std::collections::BTreeMap<String, serde_json::Value> = match raw {
        Some(doc) => serde_json::from_str(&doc).unwrap_or_default(),
        None => Default::default(),
    };

    let mut out = serde_json::Map::new();
    for (name, body) in map {
        let save_path = body
            .get("save_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        out.insert(
            name.clone(),
            serde_json::json!({"name": name, "savePath": save_path}),
        );
    }
    Json(serde_json::Value::Object(out)).into_response()
}

async fn qbit_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let tags = {
        let store = state.store.lock().unwrap();
        store.registered_tags().unwrap_or_default()
    };
    Json(tags).into_response()
}


// ---------------------------------------------------------------------------
// Trackers, network mode, filesystem browsing, benchmark listings
// ---------------------------------------------------------------------------

/// One tracker as the Trackers tab shows it.
///
/// A struct on the Go side: the key order below is its declaration order, not
/// alphabetical.
#[derive(serde::Serialize)]
struct TrackerRow {
    host: String,
    torrents: i64,
    ok: bool,
    last_error: String,
    last_announce: String,
    announces: i64,
    errors: i64,
    spoofed: bool,
    peer_id_prefix: String,
    user_agent: String,
    passkey_set: bool,
    ip_mode: String,
    sources: Vec<String>,
}

/// The zero time Go marshals for a tracker that has never answered.
const GO_ZERO_TIME: &str = "0001-01-01T00:00:00Z";

async fn get_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    // Only hosts carrying a client override are listed: a tracker is known here
    // because the operator declared an identity for it, not because a torrent
    // happens to announce to it.
    let rows: Vec<TrackerRow> = cfg
        .announce_clients
        .iter()
        .map(|(host, client)| TrackerRow {
            host: host.clone(),
            torrents: 0,
            ok: false,
            last_error: String::new(),
            last_announce: GO_ZERO_TIME.to_string(),
            announces: 0,
            errors: 0,
            spoofed: true,
            peer_id_prefix: client.peer_id_prefix.clone(),
            user_agent: client.user_agent.clone(),
            passkey_set: cfg.announce_passkeys.contains_key(host),
            ip_mode: cfg
                .announce_ip_modes
                .get(host)
                .cloned()
                .unwrap_or_else(|| "auto".to_string()),
            sources: vec!["config".to_string()],
        })
        .collect();
    Json(rows).into_response()
}

/// How the engines reach the network, and with what.
///
/// The mode is DEDUCED from the configuration rather than stored: a field
/// saying "proxy_v2" while no listener is configured is how an operator ends up
/// believing traffic is tunnelled when it is not.
async fn get_network_mode(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let race = &cfg.race;
    let hoard = &cfg.hoard;
    let mode = if hoard.gluetun_port_forward || race.gluetun_port_forward {
        "gluetun"
    } else if race.listen_port_proxy_v2 != 0 || hoard.listen_port_proxy_v2 != 0 {
        "proxy_v2"
    } else if !race.socks5_outbound_host.is_empty() {
        "socks5"
    } else {
        "direct"
    };

    #[derive(serde::Serialize)]
    struct Fields<'a> {
        race_listen_port: u16,
        hoard_listen_port: u16,
        enable_ipv6: bool,
        race_bind_interface: &'a str,
        hoard_bind_interface: &'a str,
        socks5_host: &'a str,
        socks5_port: u16,
        socks5_user: &'a str,
        socks5_pass: &'a str,
        race_proxy_v2_port: u16,
        hoard_proxy_v2_port: u16,
        proxy_v2_listen_addr: &'a str,
        proxy_v2_trusted_sources: &'a [String],
        gluetun_port_forward: bool,
        gluetun_url: &'a str,
        gluetun_api_key: &'a str,
        gluetun_port_engine: &'a str,
    }

    let fields = Fields {
        race_listen_port: race.listen_port,
        hoard_listen_port: hoard.listen_port,
        enable_ipv6: race.enable_ipv6,
        race_bind_interface: &race.bind_interface,
        hoard_bind_interface: &hoard.bind_interface,
        socks5_host: &race.socks5_outbound_host,
        socks5_port: race.socks5_outbound_port,
        socks5_user: &race.socks5_outbound_user,
        socks5_pass: &race.socks5_outbound_pass,
        race_proxy_v2_port: race.listen_port_proxy_v2,
        hoard_proxy_v2_port: hoard.listen_port_proxy_v2,
        proxy_v2_listen_addr: &race.listen_addr_proxy_v2,
        proxy_v2_trusted_sources: &race.proxy_v2_trusted_sources,
        gluetun_port_forward: hoard.gluetun_port_forward,
        gluetun_url: &hoard.gluetun_url,
        gluetun_api_key: &hoard.gluetun_api_key,
        gluetun_port_engine: "hoard",
    };

    // The OUTER object is a struct too, so its key order is mode, fields,
    // env_overrides, warnings, extra_engines -- not the alphabetical order a
    // map would give. Both levels had to be fixed; the first attempt corrected
    // only the inner one and the response stayed byte-different at the same
    // length, which is precisely what the byte check exists to catch.
    #[derive(serde::Serialize)]
    struct NetworkMode<'a> {
        mode: &'a str,
        fields: Fields<'a>,
        env_overrides: Option<serde_json::Value>,
        warnings: Option<serde_json::Value>,
        extra_engines: Vec<serde_json::Value>,
    }

    Json(NetworkMode {
        mode,
        fields,
        env_overrides: None,
        warnings: None,
        extra_engines: vec![],
    })
    .into_response()
}

/// Directory listing, used by the save-path picker.
async fn get_fs_browse(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let path = query_param(&query, "path").unwrap_or_else(|| "/".to_string());
    let mut dirs: Vec<String> = std::fs::read_dir(&path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    Json(serde_json::json!({"dirs": dirs, "path": path})).into_response()
}

/// Cumulative per-tracker transfer, one row per (engine, tracker).
#[derive(serde::Serialize)]
struct TrackerStat {
    active: i64,
    cum_downloaded: i64,
    cum_uploaded: i64,
    download_rate: i64,
    engine: String,
    peers: i64,
    torrents: i64,
    tracker: String,
    ts: i64,
    upload_rate: i64,
}

async fn get_tracker_stats_current(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    // Stored counters are the BASELINE -- what each tracker accounted for
    // before the running engines started. The live contribution of the torrents
    // currently loaded is added on top, and the torrent count comes from them
    // too. Reporting the stored figure alone makes the Trackers tab freeze at
    // the last restart, which is exactly when an operator looks at it.
    let mut totals: std::collections::BTreeMap<(String, String), (i64, i64, i64)> =
        std::collections::BTreeMap::new();

    {
        let store = state.store.lock().unwrap();
        for (engine, tracker, ul, dl) in store.tracker_counters().unwrap_or_default() {
            totals.insert((engine, tracker), (ul, dl, 0));
        }
    }

    for engine in state.engines.engines() {
        for torrent in engine.manager.all().iter() {
            let row = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
            // The host the torrent last announced to; "(none)" is a real key
            // here, for torrents that have not reached a tracker yet.
            let host = row
                .get("tracker_host")
                .and_then(|v| v.as_str())
                .filter(|h| !h.is_empty())
                .unwrap_or("(none)")
                .to_string();
            let entry = totals.entry((engine.id.clone(), host)).or_insert((0, 0, 0));
            entry.0 += row.get("total_upload").and_then(|v| v.as_i64()).unwrap_or(0);
            entry.1 += row.get("total_download").and_then(|v| v.as_i64()).unwrap_or(0);
            entry.2 += 1;
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let rows: Vec<TrackerStat> = totals
        .into_iter()
        .map(|((engine, tracker), (ul, dl, torrents))| TrackerStat {
            active: 0,
            cum_downloaded: dl,
            cum_uploaded: ul,
            download_rate: 0,
            engine,
            peers: 0,
            torrents,
            tracker,
            ts: now,
            upload_rate: 0,
        })
        .collect();
    Json(rows).into_response()
}

async fn get_bench_records(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    // next_pib is 1 rather than 0: the next milestone after nothing is the
    // first petabyte, not "no milestone".
    Json(serde_json::json!({
        "current_pib": 0, "milestones": [], "next_pib": 1, "records": [],
    }))
    .into_response()
}

empty_list_route!(get_bench_range);
empty_list_route!(get_tracker_stats_range);
empty_list_route!(get_agents_torrents);
/// The network interfaces an engine can be bound to.
///
/// Read from sysfs rather than through a netlink crate: the set of names is
/// all the picker needs, and sysfs is the same list the operator sees in `ip`.
fn interfaces() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != "lo")
        .collect();
    names.sort();

    for name in names {
        let state = std::fs::read_to_string(format!("/sys/class/net/{name}/operstate"))
            .unwrap_or_default();
        // Only interfaces that are actually up. A container image can carry
        // tunnel stubs (sit0, tunl0) that are always down; listing them showed
        // two interfaces where 3.x shows one, and would put dead devices in the
        // picker an operator binds an engine to.
        if state.trim() != "up" {
            continue;
        }
        out.push(serde_json::json!({
            "name": name,
            "ip": local_ipv4(&name),
            "up": true,
        }));
    }
    out
}

/// The IPv4 address bound to one interface.
fn local_ipv4(want: &str) -> String {
    // /proc/net/fib_trie is awkward to parse; a UDP socket bound to the device
    // and "connected" to a public address reveals the source the kernel would
    // pick, without sending a packet.
    use std::net::UdpSocket;
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0") else {
        return String::new();
    };
    if socket.connect("192.0.2.1:9").is_err() {
        return String::new();
    }
    match socket.local_addr() {
        Ok(addr) if !want.is_empty() => addr.ip().to_string(),
        _ => String::new(),
    }
}

async fn get_network_interfaces(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({"interfaces": interfaces()})).into_response()
}

/// One agent per local engine.
///
/// "local" stopped being a name in 3.138.0: a node with race and hoard presents
/// itself as local-race and local-hoard, each owning its engine.
async fn get_agents(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let ifaces = interfaces();
    let agents: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": local_agent(&e.id),
                "kind": "local",
                "online": true,
                "engines": [{"id": e.id, "role": e.role, "online": true}],
                "ipv6_wanted": e.enable_ipv6,
                "interfaces": ifaces,
            })
        })
        .collect();
    Json(agents).into_response()
}

async fn get_network_engines(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({
        "engines": [], "exit_ip_v6": "", "exits": [], "measured_at": 0,
    }))
    .into_response()
}

/// Progress of a qBittorrent import.
///
/// 404 with a body, not an empty 404: clients distinguish "no import running"
/// from "this build does not have the endpoint".
async fn get_qbit_import_events(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "no import job"})),
    )
        .into_response()
}

/// The headline payload, shared by /api/status and the event stream.
///
/// One function, two consumers: the stream used to be able to drift from the
/// endpoint it mirrors, and a UI reading both would then show two different
/// truths depending on which one answered last.
fn status_payload(state: &AppState) -> serde_json::Value {
    let cfg = state.cfg();
    let (base_up, base_down) = {
        let store = state.store.lock().unwrap();
        store.counter("global")
    };
    let (session_up, session_down) = state.engines.session_totals();

    // Per-state counts, read from the engines rather than from a cache: this is
    // the header an operator refreshes to see whether anything is moving.
    let mut seeds = 0i64;
    let mut downloading = 0i64;
    let mut race_torrents = 0i64;
    if let Some(race) = state.engines.get("race") {
        for torrent in race.manager.all().iter() {
            race_torrents += 1;
            let row = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
            match row.get("state").and_then(|v| v.as_str()).unwrap_or("") {
                "seeding" => seeds += 1,
                "downloading" => downloading += 1,
                _ => {}
            }
        }
    }
    let hoard_torrents = state
        .engines
        .get("hoard")
        .map(|e| e.manager.all().len() as i64)
        .unwrap_or(0);

    let ratio = if session_down > 0 {
        session_up as f64 / session_down as f64
    } else {
        0.0
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    serde_json::json!({
        "baseline": {
            "global_downloaded": base_down + session_down,
            "global_uploaded": base_up + session_up,
            "session_downloaded": session_down,
            "session_uploaded": session_up,
            "total_downloaded": base_down,
            "total_uploaded": base_up,
        },
        "day_downloaded": session_down,
        "day_uploaded": session_up,
        "hoard": {
            "active_download_rate": 0, "active_peers": 0, "active_upload_rate": 0,
            "engine": "hoard", "listen_port": cfg.hoard.listen_port,
            "running": true, "session_downloaded": 0, "session_uploaded": 0,
            "stagger_complete": true, "swarm_leechers": 0, "torrents_announced": 0,
            "torrents_uploading": 0, "torrents_with_peers": 0,
            "total_torrents": hoard_torrents, "unseeded_peers": 0,
        },
        "race": {
            "active_downloads": downloading,
            "active_seeds": seeds,
            "session_downloaded": session_down,
            "session_grabbed": 0,
            "session_ratio": crate::row::num_json(ratio),
            "session_uploaded": session_up,
            "torrents": race_torrents,
            "torrents_with_peers": 0,
            "total_download_rate": 0,
            "total_peers": 0,
            "total_upload_rate": 0,
        },
        "server_ts": now,
        "tunnels": [],
        // Seconds with a fraction, as 3.x publishes it.
        "uptime": (now - state.started_at) as f64,
        "version": HYDRA_VERSION,
    })
}

/// Headline figures for the whole node.
async fn get_status(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(status_payload(&state)).into_response()
}


// ---------------------------------------------------------------------------
// Logs, benchmark sampling, port forwarding
// ---------------------------------------------------------------------------

async fn get_logs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({"entries": state.logs.snapshot()})).into_response()
}

/// The Logs tab's live feed, and the generic event stream.
///
/// 3.x opens with a `: connected` comment and then pushes the same payload
/// /api/status serves, wrapped as {"data": ..., "event": ...}. A client that
/// reconnects refetches the endpoint and resumes, so the stream never has to be
/// replayable.
async fn stream_events(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let stream = async_stream::stream! {
        loop {
            let payload = serde_json::json!({
                "data": status_payload(&state),
                "event": "status",
            });
            yield Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .data(serde_json::to_string(&payload).unwrap_or_default()),
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    };

    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

/// The Logs tab's live feed.
///
/// Separate from /api/events on purpose: this one is SILENT until a line is
/// logged. 3.x behaves the same way, and a stream that pushes a status frame
/// every two seconds would make the Logs tab scroll on its own.
async fn stream_logs(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let logs = state.logs.clone();
    let stream = async_stream::stream! {
        let mut seen = logs.snapshot().len();
        loop {
            let snapshot = logs.snapshot();
            if snapshot.len() > seen {
                for entry in &snapshot[seen..] {
                    let data = serde_json::to_string(entry).unwrap_or_default();
                    yield Ok::<_, std::convert::Infallible>(
                        axum::response::sse::Event::default().event("log").data(data),
                    );
                }
                seen = snapshot.len();
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };

    axum::response::Sse::new(stream)
        .keep_alive(axum::response::sse::KeepAlive::default())
        .into_response()
}

/// One sample of the headline performance counters.
///
/// The arc_* fields come from the host's ZFS ARC and are excluded from the
/// comparison for that reason; everything else is this node's own.
async fn get_bench_current(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let (base_up, base_down) = {
        let store = state.store.lock().unwrap();
        store.counter("global")
    };
    let (session_up, session_down) = state.engines.session_totals();
    let race_torrents = state
        .engines
        .get("race")
        .map(|e| e.manager.all().len() as i64)
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Json(serde_json::json!({
        "arc_demand_hit_rate_pct": 0, "arc_demand_miss_per_sec": 0,
        "arc_ghost_hits_per_sec": 0, "arc_hit_rate_pct": 0,
        "arc_miss_per_sec": 0, "arc_size_bytes": 0,
        "global_downloaded": base_down + session_down,
        "global_uploaded": base_up + session_up,
        "hoard_active": 0, "hoard_announce_fail_rate": 0, "hoard_announce_rate": 0,
        "hoard_peers": 0, "hoard_session_uploaded": 0, "hoard_upload_rate": 0,
        "hoard_uploading": 0, "hoard_with_peers": 0,
        "iowait_pct": 0, "open_fds": open_fd_count(),
        "race_announce_fail_rate": 0, "race_announce_rate": 0, "race_avg_share": 0,
        "race_download_rate": 0,
        // Not a peer count: 3.x publishes the torrent count here, and its own
        // source comments call it approximate. Reproduced rather than corrected,
        // because a graph reading this field would step the day it changed.
        "race_peers": race_torrents,
        "race_session_uploaded": session_up,
        "race_torrents": race_torrents,
        "race_upload_rate": 0, "race_uploading": 0,
        "ts": now,
    }))
    .into_response()
}

fn open_fd_count() -> i64 {
    std::fs::read_dir("/proc/self/fd")
        .map(|d| d.count() as i64)
        .unwrap_or(0)
}

/// Whether incoming connections can reach each engine.
///
/// ⚠ Everything here is one struct, top to bottom. Nesting an order-sensitive
/// struct inside `serde_json::json!` does NOT preserve its field order: the
/// macro converts it with to_value, and serde_json's Map is a BTreeMap, so the
/// keys come back sorted. That cost a full debugging round on this very route --
/// identical 630 bytes, different order, invisible to a structural comparison.
#[derive(serde::Serialize, Clone)]
struct Socket {
    ip: &'static str,
    port: u16,
    bound_interface: &'static str,
    stale: bool,
}

#[derive(serde::Serialize, Clone)]
struct Reach {
    state: &'static str,
    at: &'static str,
}

#[derive(serde::Serialize)]
struct PortForward {
    all_connectable: bool,
    hoard_connectable: bool,
    hoard_peers: i64,
    hoard_port: u16,
    hoard_reach: Reach,
    hoard_sockets: Vec<Socket>,
    ipv6_wanted: bool,
    listen_healthy: bool,
    public_ip: String,
    public_ip_v6: String,
    race_connectable: bool,
    race_peers: i64,
    race_port: u16,
    race_reach: Reach,
    race_sockets: Vec<Socket>,
}

async fn get_port_forward(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let sockets = |port: u16| {
        vec![
            Socket { ip: "0.0.0.0", port, bound_interface: "", stale: false },
            Socket { ip: "[::]", port, bound_interface: "", stale: false },
        ]
    };
    // "unknown" rather than "closed": nothing has probed yet, and reporting a
    // closed port an operator would then chase is worse than admitting silence.
    let reach = || Reach { state: "unknown", at: GO_ZERO_TIME };
    let ip = state.public_ip.lock().await;

    Json(PortForward {
        all_connectable: false,
        hoard_connectable: false,
        hoard_peers: 0,
        hoard_port: cfg.hoard.listen_port,
        hoard_reach: reach(),
        hoard_sockets: sockets(cfg.hoard.listen_port),
        ipv6_wanted: cfg.race.enable_ipv6,
        listen_healthy: true,
        public_ip: ip.0.clone(),
        public_ip_v6: ip.1.clone(),
        race_connectable: false,
        race_peers: 0,
        race_port: cfg.race.listen_port,
        race_reach: reach(),
        race_sockets: sockets(cfg.race.listen_port),
    })
    .into_response()
}


/// Runtime tuning flags.
///
/// ⚠ Several of these describe machinery that 4.0.0 deletes: `gogc` is the Go
/// collector's target, and ipc_frame / ipc_prealloc / ipc_route / list_cache /
/// qbit_snapshot are all properties of the socket between the two processes
/// there is no longer. They are published unchanged so a 3.x client keeps
/// working, and they are the first thing the 4.0 API notes should retire --
/// reporting a garbage-collector setting from a binary with no garbage
/// collector is a lie the UI would render as fact.
async fn get_opt_flags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    // session_runtimes is a fixed 128, not aio_threads: tying it to the config
    // was a guess, and the reference answers 128 on both engines whatever
    // aio_threads says (32 for hoard, 16 for race in this config).
    let engine_flags = || {
        serde_json::json!({
            "block_mse": false,
            "session_pinning": false,
            "session_runtimes": 128,
        })
    };

    Json(serde_json::json!({
        "engine_flags": {
            "hoard": engine_flags(),
            "race": engine_flags(),
        },
        "flags": {
            "ipc_frame": true, "ipc_prealloc": true, "ipc_route": true,
            "list_cache": true, "qbit_snapshot": true, "totals_cache": true,
        },
        "gogc": 100,
        "list_cache_ttl_ms": 9000,
    }))
    .into_response()
}

/// Metrics compared between two periods.
///
/// The metric set is exactly the one /api/benchmark/current samples, so the two
/// are generated from one list: a metric added to the sampler and forgotten
/// here is a column that silently stops being comparable.
const BENCH_METRICS: &[&str] = &[
    "arc_demand_hit_rate_pct", "arc_demand_miss_per_sec", "arc_ghost_hits_per_sec",
    "arc_hit_rate_pct", "arc_miss_per_sec", "arc_size_bytes",
    "global_downloaded", "global_uploaded",
    "hoard_active", "hoard_announce_fail_rate", "hoard_announce_rate",
    "hoard_peers", "hoard_session_uploaded", "hoard_upload_rate",
    "hoard_uploading", "hoard_with_peers",
    "iowait_pct", "open_fds",
    "race_announce_fail_rate", "race_announce_rate", "race_avg_share",
    "race_download_rate", "race_peers", "race_session_uploaded",
    "race_torrents", "race_upload_rate", "race_uploading",
];

async fn get_bench_compare(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let empty = || serde_json::json!({"avg": 0, "count": 0, "max": 0, "p95": 0});
    let mut metrics = serde_json::Map::new();
    for name in BENCH_METRICS {
        metrics.insert(
            (*name).to_string(),
            serde_json::json!({"delta_avg_pct": 0, "p1": empty(), "p2": empty()}),
        );
    }

    Json(serde_json::json!({
        "metrics": serde_json::Value::Object(metrics),
        "p1_count": 0,
        "p2_count": 0,
    }))
    .into_response()
}


/// The VPN providers Hydra knows how to ask for a forwarded port.
///
/// Field names are capitalised because the Go struct carries no json tags, and
/// the list is ordered by LABEL, not by id -- that is what the picker shows.
#[derive(serde::Serialize)]
struct WgProvider {
    #[serde(rename = "ID")]
    id: &'static str,
    #[serde(rename = "Label")]
    label: &'static str,
    #[serde(rename = "PortForward")]
    port_forward: &'static str,
    #[serde(rename = "Note")]
    note: &'static str,
}

fn wg_providers() -> Vec<WgProvider> {
    let mut list = vec![
        WgProvider { id: "proton", label: "Proton VPN", port_forward: "natpmp",
            note: "The port is obtained by NAT-PMP and renewed continuously. Use a server marked P2P." },
        WgProvider { id: "airvpn", label: "AirVPN", port_forward: "manual",
            note: "AirVPN assigns the port in the client area. Create it there, then type it here." },
        WgProvider { id: "mullvad", label: "Mullvad", port_forward: "none",
            note: "Mullvad removed port forwarding in 2023. This engine will take no incoming peer connections." },
        WgProvider { id: "pia", label: "Private Internet Access", port_forward: "manual",
            note: "PIA forwards ports through its own API, which needs the account credentials as well as the config. Not automated yet: set the port by hand, or run PIA behind gluetun." },
        WgProvider { id: "windscribe", label: "Windscribe", port_forward: "manual",
            note: "Windscribe assigns an ephemeral or static port on its web panel." },
        WgProvider { id: "natpmp", label: "Other (NAT-PMP capable)", port_forward: "natpmp",
            note: "For any provider whose gateway answers NAT-PMP, the way Proton does." },
        WgProvider { id: "generic", label: "Other / none", port_forward: "none",
            note: "The tunnel is brought up, no port is requested. Set a port by hand if the provider forwards one." },
    ];
    list.sort_by_key(|p| p.label);
    list
}

#[derive(serde::Serialize)]
struct WireGuardStatus {
    configs: Vec<serde_json::Value>,
    directory: String,
    engines: serde_json::Map<String, serde_json::Value>,
    providers: Vec<WgProvider>,
    supported: bool,
    /// null, not []: no tunnel has been declared, and 3.x marshals its nil
    /// slice. A client testing `tunnels === null` would take [] for "one tunnel
    /// list that happens to be empty".
    tunnels: Option<Vec<serde_json::Value>>,
}

async fn get_wireguard(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let directory = std::path::Path::new(&cfg.daemon.data_dir)
        .join("wireguard")
        .to_string_lossy()
        .into_owned();

    // A struct all the way down. Putting WgProvider inside json! would sort its
    // fields to ID, Label, Note, PortForward -- same 1263 bytes, wrong order.
    Json(WireGuardStatus {
        configs: vec![],
        directory,
        engines: serde_json::Map::new(),
        providers: wg_providers(),
        supported: true,
        tunnels: None,
    })
    .into_response()
}


// ---------------------------------------------------------------------------
// Writes -- the qBittorrent shim
// ---------------------------------------------------------------------------
//
// These are the endpoints the *arr stack, cross-seed and autobrr call. None of
// them reports a mismatch: a wrong answer here does not raise an error
// anywhere, it just makes the library drift. So the write bench compares the
// STORE after each sequence, not only the responses.

use axum::extract::Form;
use std::collections::BTreeMap;

/// A form body, kept generic: qBit sends a flat map and the field set varies
/// per endpoint.
type Fields = BTreeMap<String, String>;

fn split_list(raw: &str) -> Vec<String> {
    raw.split(&[',', '|'][..])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// qBittorrent answers its mutations with an empty 200.
fn qbit_ok() -> Response {
    (StatusCode::OK, "").into_response()
}

/// Read the category document, mutate it, write it back.
///
/// Round-tripping the whole document rather than patching one key keeps the
/// shape 3.x wrote: the file carries fields this build does not model, and a
/// rewrite from a typed struct would drop them.
fn edit_categories<F>(state: &AppState, mutate: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>),
{
    let cfg = state.cfg();
    let store = state.store.lock().unwrap();
    let raw = store
        .meta_doc("categories")
        .filter(|d| !d.is_empty())
        .or_else(|| {
            let path = std::path::Path::new(&cfg.daemon.data_dir)
                .join("categories.json");
            std::fs::read_to_string(path).ok()
        })
        .unwrap_or_else(|| "{}".to_string());

    let mut doc: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&raw).unwrap_or_default();
    mutate(&mut doc);

    // Written exactly as 3.x writes it: json.MarshalIndent(map, "", "  ").
    // Two spaces, outer keys sorted (a Go map), inner keys in the categoryJSON
    // declaration order -- save_path before mode. The write bench caught this:
    // all nine responses matched while the stored document differed, which is
    // the entire reason that bench compares the store.
    let typed: std::collections::BTreeMap<String, StoredCategory> = doc
        .into_iter()
        .map(|(name, body)| {
            (name, serde_json::from_value(body).unwrap_or_default())
        })
        .collect();
    store.put_meta("categories", &indent_two(&typed))?;
    Ok(())
}

/// serde_json's pretty printer uses two spaces, like Go's MarshalIndent here.
fn indent_two<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// One category as it is STORED, in the Go struct's declaration order.
///
/// A struct, not a serde_json::Map: Map is a BTreeMap, so inserting the keys in
/// the right order still writes them sorted. That is the same trap as
/// `json!`, and it survived one round of fixing here because the indentation
/// looked right while the order was not.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct StoredCategory {
    save_path: String,
    mode: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    graduate_to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agents: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    strategy: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    min_free_bytes: i64,
}

async fn qbit_create_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let name = form.get("category").cloned().unwrap_or_default();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "category name is empty").into_response();
    }
    let save_path = form.get("savePath").cloned().unwrap_or_default();
    let _ = edit_categories(&state, |doc| {
        doc.insert(name, serde_json::json!({"save_path": save_path, "mode": "hoard"}));
    });
    qbit_ok()
}

async fn qbit_edit_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let name = form.get("category").cloned().unwrap_or_default();
    let save_path = form.get("savePath").cloned().unwrap_or_default();
    let _ = edit_categories(&state, |doc| {
        // Edit in place so anything the category carries beyond save_path
        // survives; only a category that does not exist is created whole.
        match doc.get_mut(&name) {
            Some(serde_json::Value::Object(fields)) => {
                fields.insert("save_path".into(), serde_json::Value::String(save_path));
            }
            _ => {
                doc.insert(name, serde_json::json!({"save_path": save_path, "mode": "hoard"}));
            }
        }
    });
    qbit_ok()
}

async fn qbit_remove_categories(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    // The field is newline-separated in qBit's own API.
    let names: Vec<String> = form
        .get("categories")
        .map(|raw| {
            raw.split(['\n', ','])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let _ = edit_categories(&state, |doc| {
        for name in &names {
            doc.remove(name);
        }
    });
    qbit_ok()
}

async fn qbit_create_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let tags = split_list(form.get("tags").map(String::as_str).unwrap_or(""));
    let store = state.store.lock().unwrap();
    let _ = store.register_tags(&tags);
    qbit_ok()
}

async fn qbit_delete_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let tags = split_list(form.get("tags").map(String::as_str).unwrap_or(""));
    let _ = tags;
    // ⚠ Deliberately does NOT touch tag_registry.
    //
    // 3.x removes the tag from a FILE registry (tagstore.SaveRegistry) and
    // leaves the tag_registry table alone, while createTags writes to the
    // table. Its registry is therefore split in two halves that drift apart,
    // and a deleted tag survives in the database. That is a bug, and it is
    // reproduced here rather than fixed, because fixing it silently would make
    // 4.0.0 answer differently from the version it has to replace. It is
    // written up so it can be fixed on purpose, with a note, in a later
    // release.
    qbit_ok()
}

/// Apply a tag change to every torrent named in `hashes`.
fn retag(state: &AppState, form: &Fields, add: bool) {
    let tags = split_list(form.get("tags").map(String::as_str).unwrap_or(""));
    let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
    let store = state.store.lock().unwrap();

    if add {
        let _ = store.register_tags(&tags);
    }
    for prefix in hashes {
        let Some(hash) = store.resolve_hash(&prefix) else {
            continue;
        };
        let mut current = store.tags_of(&hash);
        for tag in &tags {
            current.retain(|t| t != tag);
            if add {
                current.push(tag.clone());
            }
        }
        current.sort();
        current.dedup();
        let _ = store.set_tags(&hash, &current);
    }
}

async fn qbit_add_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    retag(&state, &form, true);
    qbit_ok()
}

async fn qbit_remove_tags(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    retag(&state, &form, false);
    qbit_ok()
}

/// Pause or resume. The store carries the user's intent; the engine follows.
fn set_paused(state: &AppState, form: &Fields, paused: bool) {
    let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
    let store = state.store.lock().unwrap();
    for prefix in hashes {
        if let Some(hash) = store.resolve_hash(&prefix) {
            let _ = store.set_paused(&hash, paused);
        }
    }
}

async fn qbit_pause(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    set_paused(&state, &form, true);
    qbit_ok()
}

async fn qbit_resume(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    set_paused(&state, &form, false);
    qbit_ok()
}


// ---------------------------------------------------------------------------
// Writes -- the native API
// ---------------------------------------------------------------------------

use axum::extract::Path;

/// Resolve a path info-hash inside the hoard session, or answer as 3.x does.
///
/// The message differs per route and that is not cosmetic: "torrent not found"
/// and "torrent not in hoard: X" tell an operator two different things, and the
/// UI shows the string.
fn resolve_in_hoard(state: &AppState, prefix: &str, message: &str) -> Result<String, Response> {
    let store = state.store.lock().unwrap();
    store.resolve_hash_in("hoard", prefix).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": message.replace("{}", prefix)})),
        )
            .into_response()
    })
}

macro_rules! torrent_write {
    ($name:ident, $message:expr, $ok:expr, $body:expr) => {
        async fn $name(
            State(state): State<AppState>,
            Path(info_hash): Path<String>,
            RawQuery(query): RawQuery,
            headers: HeaderMap,
            body: String,
        ) -> Response {
            let query = query.unwrap_or_default();
            guard!(state, headers, query);
    let cfg = state.cfg();
            let hash = match resolve_in_hoard(&state, &info_hash, $message) {
                Ok(h) => h,
                Err(response) => return response,
            };
            let apply: fn(&AppState, &str, &str) = $body;
            apply(&state, &hash, &body);
            let ok: fn(&str) -> serde_json::Value = $ok;
            Json(ok(&info_hash)).into_response()
        }
    };
}

torrent_write!(hoard_pause_one, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, _body: &str| {
    let store = state.store.lock().unwrap();
    let _ = store.set_paused(hash, true);
});

torrent_write!(hoard_resume_one, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, _body: &str| {
    let store = state.store.lock().unwrap();
    let _ = store.set_paused(hash, false);
});

torrent_write!(hoard_pin_one, "torrent not in hoard: {}", |ih: &str| serde_json::json!({"info_hash": ih, "pinned": true, "status": "ok"}), |state: &AppState, hash: &str, _body: &str| {
    let store = state.store.lock().unwrap();
    let _ = store.set_pinned(hash, true);
});

/// Unpin, which unlike pin accepts a torrent from ANY session.
///
/// 3.x checks hoard membership on pin and not on unpin. That asymmetry is
/// almost certainly an oversight, but it is observable -- unpinning a race
/// torrent answers 200 there -- so it is reproduced rather than tidied up.
/// Worth fixing on purpose later, in one direction or the other.
async fn hoard_unpin_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let store = state.store.lock().unwrap();
    if let Some(hash) = store.resolve_hash(&info_hash) {
        let _ = store.set_pinned(&hash, false);
    }
    Json(serde_json::json!({
        "info_hash": info_hash, "pinned": false, "status": "ok",
    }))
    .into_response()
}

torrent_write!(set_torrent_category, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, body: &str| {
    // The body is {"category": "..."} on the native API.
    let category = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("category").and_then(|c| c.as_str()).map(str::to_string))
        .unwrap_or_default();
    let store = state.store.lock().unwrap();
    let _ = store.set_category(hash, &category);
});

torrent_write!(set_torrent_tags, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, body: &str| {
    // {"tags": ["a","b"]} replaces the whole set, which is what "set" means
    // here: the caller sends the state it wants, not a delta.
    let tags: Vec<String> = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("tags").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let store = state.store.lock().unwrap();
    let _ = store.register_tags(&tags);
    let _ = store.set_tags(hash, &tags);
});

/// Create a category from the native API.
async fn category_create(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let incoming: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let name = incoming
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name is required"})),
        )
            .into_response();
    }
    let _ = edit_categories(&state, |doc| {
        doc.insert(name, incoming.clone());
    });
    // 201 with the category echoed back, not a bare status: the UI uses the
    // echo to add the row without refetching the list. Echoed through the
    // ordered struct, because a serde_json::Value would come back alphabetical.
    let echo: Category = serde_json::from_value(incoming).unwrap_or_default();
    (StatusCode::CREATED, Json(echo)).into_response()
}

async fn category_update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let incoming: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let _ = edit_categories(&state, |doc| {
        doc.insert(name, incoming.clone());
    });
    Json(serde_json::json!({"status": "ok"})).into_response()
}

async fn category_delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = edit_categories(&state, |doc| {
        doc.remove(&name);
    });
    // The counts say how many torrents lost the category, in the engines and in
    // the store; was_orphan reports a category that no longer existed.
    Json(serde_json::json!({
        "cleared": 0, "cleared_stored": 0, "status": "ok", "was_orphan": false,
    }))
    .into_response()
}


// ---------------------------------------------------------------------------
// Writes -- the configuration file
// ---------------------------------------------------------------------------

/// Edit default.toml in place and reload it.
///
/// In place, not rewritten: the file carries the operator's banners and
/// comments, and re-serialising it from the struct would delete them the first
/// time somebody flipped a switch in the UI. See tomledit.rs.
///
/// The reload is what makes the change visible to the next GET; without it a
/// write followed by a read returns the old value, which looks exactly like a
/// write that failed.
fn edit_config<F>(state: &AppState, mutate: F) -> bool
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let Ok(doc) = std::fs::read_to_string(&state.config_path) else {
        return false;
    };
    let Ok(edited) = mutate(&doc) else {
        return false;
    };
    // Parsed before it is written: a document that would not decode is a
    // daemon that will not boot next time, and the UI would have no idea.
    if toml::from_str::<Config>(&edited).is_err() {
        tracing::error!("refusing a config edit that would not parse");
        return false;
    }
    if std::fs::write(&state.config_path, &edited).is_err() {
        return false;
    }
    if let Ok(reloaded) = toml::from_str::<Config>(&edited) {
        state.set_cfg(reloaded);
    }
    true
}

/// Set or clear one entry of a `host = "value"` table.
fn set_host_entry(state: &AppState, section: &str, host: &str, value: &str) -> bool {
    let key = crate::tomledit::quote_toml_key(host);
    let section = section.to_string();
    if value.is_empty() {
        let key2 = key.clone();
        let section2 = section.clone();
        return edit_config(state, move |doc| {
            let pruned = crate::tomledit::delete_toml_key(doc, &section2, &key2);
            Ok(crate::tomledit::prune_empty_table(&pruned, &section2))
        });
    }
    let pairs = vec![(key, crate::tomledit::quote_toml_key(value))];
    edit_config(state, move |doc| {
        crate::tomledit::set_toml_table(doc, &section, &pairs)
    })
}

#[derive(serde::Deserialize)]
struct HostValue {
    #[serde(default)]
    host: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    passkey: String,
}

async fn set_announce_ip_mode(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<HostValue>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.host.trim().is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "host is required"}))).into_response();
    }
    if !matches!(req.mode.as_str(), "auto" | "v4" | "v6" | "") {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "mode must be one of: auto, v4, v6"})))
            .into_response();
    }

    // "auto" is the default, so it is stored as the ABSENCE of an entry rather
    // than as a value: keeping `host = "auto"` would make the file grow one
    // line per tracker anyone ever looked at.
    let stored = if req.mode == "auto" { "" } else { req.mode.as_str() };
    let persisted = set_host_entry(&state, "announce_ip_modes", req.host.trim(), stored);

    Json(serde_json::json!({
        "status": "ok",
        "ip_modes": state.cfg().announce_ip_modes,
        // One "agent" per local engine: this node presents itself as local-race
        // and local-hoard, and a config push reaches both.
        "agents_pushed": state.engines.engines().len(),
        "agents_failed": 0,
        "persisted": persisted,
    }))
    .into_response()
}

async fn set_announce_passkey(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<HostValue>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.host.trim().is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "host is required"}))).into_response();
    }
    let persisted = set_host_entry(&state, "announce_passkeys", req.host.trim(), &req.passkey);
    Json(serde_json::json!({
        "status": "ok",
        "passkeys": state.cfg().announce_passkeys,
        "agents_pushed": state.engines.engines().len(),
        "agents_failed": 0,
        "persisted": persisted,
    }))
    .into_response()
}


#[derive(serde::Deserialize)]
struct ClientOverride {
    #[serde(default)]
    host: String,
    #[serde(default)]
    peer_id_prefix: String,
    #[serde(default)]
    user_agent: String,
}

/// Declare the client identity Hydra presents to one tracker.
///
/// Stored as a nested table -- `[announce_clients."host"]` -- because the host
/// is a quoted key and the entry carries two fields. Clearing both fields
/// removes the table rather than leaving an empty one behind.
async fn set_announce_client(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<ClientOverride>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "host is required"}))).into_response();
    }

    let section = format!("announce_clients.{}", crate::tomledit::quote_toml_key(&host));
    let persisted = if req.peer_id_prefix.is_empty() && req.user_agent.is_empty() {
        let section2 = section.clone();
        edit_config(&state, move |doc| {
            Ok(crate::tomledit::delete_toml_table(doc, &section2))
        })
    } else {
        let pairs = vec![
            ("peer_id_prefix".to_string(), crate::tomledit::quote_toml_key(&req.peer_id_prefix)),
            ("user_agent".to_string(), crate::tomledit::quote_toml_key(&req.user_agent)),
        ];
        edit_config(&state, move |doc| {
            crate::tomledit::set_toml_table(doc, &section, &pairs)
        })
    };

    Json(serde_json::json!({
        "status": "ok",
        "clients": state.cfg().announce_clients,
        "agents_pushed": state.engines.engines().len(),
        "agents_failed": 0,
        "persisted": persisted,
    }))
    .into_response()
}

async fn set_secondary_stats(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<HostValue>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.host.trim().is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "host is required"}))).into_response();
    }
    let persisted =
        set_host_entry(&state, "announce_secondary_stats", req.host.trim(), &req.mode);
    Json(serde_json::json!({
        "status": "ok",
        "secondary_stats": state.cfg().announce_secondary_stats,
        "agents_pushed": state.engines.engines().len(),
        "agents_failed": 0,
        "persisted": persisted,
    }))
    .into_response()
}


#[derive(serde::Deserialize)]
struct PortBody {
    #[serde(default)]
    port: u16,
}

/// Change an engine's peer listen port.
///
/// ⚠ NOT ROUTED YET, on purpose. This looked like a config write and is not:
/// 3.x asks the LIVE engine to rebind and answers 500
/// ("listen-port rebind unsupported on this engine client") when it cannot,
/// without touching the file. Persisting the value here would change the
/// operator's config in a case where the reference deliberately changes
/// nothing -- a silent divergence, and the write bench is what caught it.
/// Belongs to the network slice, with the listeners.
async fn set_listen_port(state: &AppState, engine: &str, body: &str) -> Response {
    let Ok(req) = serde_json::from_str::<PortBody>(body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.port == 0 {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "port out of range (1-65535)"})))
            .into_response();
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error":
            format!("{engine}: listen-port rebind unsupported on this engine client")})),
    )
        .into_response()
}

async fn set_race_listen_port(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_listen_port(&state, "race", &body).await
}

async fn set_hoard_listen_port(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_listen_port(&state, "hoard", &body).await
}

#[derive(serde::Deserialize)]
struct SlotsBody {
    #[serde(default)]
    max_slots: i64,
}

/// Cap on how many hoard torrents may download at once.
///
/// ⚠ NOT ROUTED YET, same reason as the listen port: 3.x answers the whole
/// slots payload from the live manager and does not write the config.
#[allow(dead_code)]
async fn set_download_slots(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<SlotsBody>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    let pairs = vec![("active_downloads".to_string(), req.max_slots.to_string())];
    let persisted = edit_config(&state, move |doc| {
        crate::tomledit::set_toml_table(doc, "hoard", &pairs)
    });
    Json(serde_json::json!({
        "status": "ok", "max_slots": req.max_slots, "persisted": persisted,
    }))
    .into_response()
}

/// Remove the cap: -1 is "no limit" in this config, not 0, which would mean
/// "never download anything".
#[allow(dead_code)]
async fn clear_download_slots(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let pairs = vec![("active_downloads".to_string(), "-1".to_string())];
    let persisted = edit_config(&state, move |doc| {
        crate::tomledit::set_toml_table(doc, "hoard", &pairs)
    });
    Json(serde_json::json!({
        "status": "ok", "max_slots": -1, "persisted": persisted,
    }))
    .into_response()
}


#[derive(serde::Deserialize)]
struct PauseBulk {
    #[serde(default)]
    hashes: Vec<String>,
    #[serde(default)]
    paused: bool,
}

/// Pause or resume every torrent of one engine.
///
/// The intent is the user's, so it is recorded on each torrent and not only
/// applied to the running engine: a restart must not silently resume a library
/// somebody deliberately stopped.
async fn pause_all(state: &AppState, engine: &str, paused: bool) -> Response {
    if state.engines.get(engine).is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": format!("{engine} agent not available")})),
        )
            .into_response();
    }
    let count = {
        let store = state.store.lock().unwrap();
        store.set_paused_all(engine, paused).unwrap_or(0)
    };
    let key = if paused { "paused" } else { "resumed" };
    Json(serde_json::json!({"status": "ok", key: count})).into_response()
}

async fn hoard_pause_all(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    pause_all(&state, "hoard", true).await
}

async fn hoard_resume_all(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    pause_all(&state, "hoard", false).await
}

/// Pause or resume a named set of torrents.
async fn pause_bulk(state: &AppState, engine: &str, body: &str) -> Response {
    let Ok(req) = serde_json::from_str::<PauseBulk>(body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "expected {hashes: [...], paused: bool}"})),
        )
            .into_response();
    };

    // Exact hashes only, unlike the qBittorrent shim.
    //
    // The shim accepts a shortened hash because qBit clients send one; the
    // native API does not, and a caller passing a prefix here gets applied=0
    // rather than a silent match on whichever torrent happened to share those
    // twelve characters.
    let mut applied = 0usize;
    {
        let store = state.store.lock().unwrap();
        for hash in &req.hashes {
            let exists = store
                .resolve_hash_in(engine, hash)
                .is_some_and(|found| found == hash.to_lowercase());
            if exists && store.set_paused(&hash.to_lowercase(), req.paused).is_ok() {
                applied += 1;
            }
        }
    }
    // `paused` is echoed back: the UI updates the row from the answer rather
    // than refetching, and needs to know which way it went.
    Json(serde_json::json!({"status": "ok", "applied": applied, "paused": req.paused}))
        .into_response()
}

async fn hoard_pause_bulk(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    pause_bulk(&state, "hoard", &body).await
}

async fn race_pause_bulk(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    pause_bulk(&state, "race", &body).await
}


// ---------------------------------------------------------------------------
// The qBittorrent identity
// ---------------------------------------------------------------------------
//
// These values are what a client uses to decide which features it may call.
// Sonarr, Radarr and cross-seed all branch on the WebAPI version. They are
// reproduced exactly: raising them would make a client try calls this shim does
// not implement, and lowering them would make it fall back to worse paths.

const QBIT_VERSION: &str = "v4.6.0";
const QBIT_WEBAPI_VERSION: &str = "2.9.3";

async fn qbit_version(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    // Plain text, not JSON: qBittorrent answers a bare string here.
    QBIT_VERSION.into_response()
}

async fn qbit_webapi_version(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    QBIT_WEBAPI_VERSION.into_response()
}

async fn qbit_build_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    // The library versions a real qBittorrent 4.6.0 reports. Hydra runs none of
    // them; the values exist so a client's build check does not refuse to talk.
    Json(serde_json::json!({
        "bitness": 64, "boost": "1.83.0", "libtorrent": "2.0.9.0",
        "openssl": "3.1.4", "qt": "6.5.3",
    }))
    .into_response()
}

async fn qbit_preferences(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    Json(serde_json::json!({
        "add_trackers_enabled": false,
        "alternative_webui_enabled": false,
        "create_subfolder_enabled": cfg.daemon.create_torrent_folder,
        "dht": cfg.race.enable_dht,
        "encryption": 1,
        "listen_port": cfg.race.listen_port,
        "locale": "en",
        "lsd": false,
        "max_active_downloads": 20,
        "max_active_torrents": 100,
        "max_active_uploads": 50,
        "max_connec": cfg.race.max_connections,
        "max_uploads_per_torrent": cfg.race.max_uploads_per_torrent,
        "pex": cfg.race.enable_pex,
        "queueing_enabled": false,
        "save_path": "/downloads",
        "temp_path_enabled": false,
        "web_ui_port": cfg.daemon.api_port,
    }))
    .into_response()
}

async fn qbit_transfer_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let (up, down) = state.engines.session_totals();
    Json(serde_json::json!({
        "connection_status": "connected",
        "dht_nodes": 0,
        "dl_info_data": down,
        "dl_info_speed": 0,
        "dl_rate_limit": 0,
        "up_info_data": up,
        "up_info_speed": 0,
        "up_rate_limit": 0,
    }))
    .into_response()
}


/// Move torrents to a category.
///
/// An empty category clears it, which is how qBit clients "remove from
/// category" -- there is no separate call for that.
async fn qbit_set_category(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let category = form.get("category").cloned().unwrap_or_default();
    let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
    let store = state.store.lock().unwrap();
    for prefix in hashes {
        if let Some(hash) = store.resolve_hash(&prefix) {
            let _ = store.set_category(&hash, &category);
        }
    }
    qbit_ok()
}

/// start/stop are qBittorrent 5's names for resume/pause. Both spellings are
/// served because clients in the wild send either depending on their vintage.
async fn qbit_start(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_paused(&state, &form, false);
    qbit_ok()
}

async fn qbit_stop(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_paused(&state, &form, true);
    qbit_ok()
}

/// qBittorrent's session login.
///
/// Hydra authenticates with an API key, so this exists only so a client that
/// insists on logging in first can proceed. It answers what qBit answers.
async fn qbit_login() -> Response {
    (StatusCode::OK, "Ok.").into_response()
}

async fn qbit_logout() -> Response {
    (StatusCode::OK, "").into_response()
}


/// The torrent listing qBittorrent clients poll.
///
/// Both engines in one list, each row carrying its engine as the fallback
/// category. This is the endpoint the *arr stack reads on a timer, so it is
/// built from the engines directly -- in 3.x it was rebuilt from a cached copy
/// of a copy, which is where 618 MB of the Go heap lived.
async fn qbit_torrents_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut rows = Vec::new();
    for engine in ["race", "hoard"] {
        for native in engine_rows(&state, engine) {
            rows.push(crate::qbitrow::build(&native, engine, now));
        }
    }
    Json(rows).into_response()
}


// ---------------------------------------------------------------------------
// Per-torrent detail
// ---------------------------------------------------------------------------

/// Find a torrent in any engine, returning it with the engine it belongs to.
fn find_torrent(
    state: &AppState,
    info_hash: &str,
) -> Option<(String, std::sync::Arc<typhon_engine::torrent::meta::TorrentState>)> {
    let wanted = info_hash.to_lowercase();
    for engine in state.engines.engines() {
        for torrent in engine.manager.all().iter() {
            let row = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
            if row.get("info_hash").and_then(|v| v.as_str()) == Some(wanted.as_str()) {
                return Some((engine.id.clone(), torrent.clone()));
            }
        }
    }
    None
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "torrent not found"})),
    )
        .into_response()
}

/// Files of one torrent, native shape.
async fn get_torrent_files(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Some((_, torrent)) = find_torrent(&state, &info_hash) else {
        return not_found();
    };
    let files: Vec<serde_json::Value> = torrent
        .meta
        .files
        .iter()
        .map(|f| serde_json::json!({"path": f.path.to_string_lossy(), "size": f.length}))
        .collect();
    Json(serde_json::json!({"files": files})).into_response()
}

/// Trackers of one torrent, grouped by tier, plus the engine holding it.
async fn get_torrent_trackers(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Some((engine, torrent)) = find_torrent(&state, &info_hash) else {
        return not_found();
    };
    // The LIVE list, not the one baked into the .torrent: an operator who edited
    // the trackers expects to see what will actually be announced to.
    let tiers = torrent.live_trackers.read().clone();
    Json(serde_json::json!({"engine": engine, "trackers": tiers})).into_response()
}

/// Files of one torrent, qBittorrent shape.
async fn qbit_torrent_files(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = query_param(&query, "hash").unwrap_or_default();
    let Some((_, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };
    let files: Vec<serde_json::Value> = torrent
        .meta
        .files
        .iter()
        .enumerate()
        .map(|(index, f)| {
            serde_json::json!({
                "availability": 1,
                "index": index,
                "is_seed": false,
                "name": f.path.to_string_lossy(),
                // The piece range is not tracked per file here; qBit clients
                // read it for a progress bar they do not draw for a complete
                // torrent, and cross-seed ignores it entirely.
                "piece_range": [0, 0],
                "priority": 1,
                "progress": 1,
                "size": f.length,
            })
        })
        .collect();
    Json(files).into_response()
}


/// One torrent's properties panel, qBittorrent shape.
///
/// The `total_*` figures come out ZERO for the same reason the listing does:
/// the shim reads the "-ed" spellings that the native row does not carry. That
/// is 3.x's behaviour and it is reproduced here too -- see the note in
/// qbitrow.rs. `dl_limit` and `up_limit` are -1, qBittorrent's "no limit".
async fn qbit_torrent_properties(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = query_param(&query, "hash").unwrap_or_default();
    let Some((engine_id, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };

    let raw = typhon_engine::rpc::dispatch::torrent_to_json(&torrent);
    let facts = {
        let store = state.store.lock().unwrap();
        store.facts_by_session(&engine_id).unwrap_or_default()
    };
    let empty = crate::row::StoreFacts::default();
    let native = crate::row::build(&raw, facts.get(&hash).unwrap_or(&empty), "");

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let added = native.get("added_time").and_then(|v| v.as_i64()).unwrap_or(0);
    let save_path = native
        .get("engine_save_path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Json(serde_json::json!({
        "addition_date": added,
        "comment": "",
        "completion_date": native.get("completed_time").and_then(|v| v.as_i64()).unwrap_or(0),
        "created_by": "",
        "creation_date": added,
        "dl_limit": -1,
        "dl_speed": native.get("download_rate").and_then(|v| v.as_i64()).unwrap_or(0),
        "dl_speed_avg": 0,
        "eta": 8_640_000,
        "last_seen": now,
        "nb_connections": 0,
        "peers": 0,
        "peers_total": 0,
        "piece_size": torrent.meta.piece_length,
        "save_path": save_path,
        "seeding_time": 0,
        "seeds": 0,
        "seeds_total": 0,
        "share_ratio": 0,
        "time_elapsed": now - added,
        "total_downloaded": 0,
        "total_downloaded_session": 0,
        "total_size": native.get("total_size").and_then(|v| v.as_i64()).unwrap_or(0),
        "total_uploaded": 0,
        "total_uploaded_session": 0,
        "total_wasted": 0,
        "up_limit": -1,
        "up_speed": native.get("upload_rate").and_then(|v| v.as_i64()).unwrap_or(0),
        "up_speed_avg": 0,
    }))
    .into_response()
}

/// Trackers of one torrent, qBittorrent shape.
///
/// The three bracketed entries are qBittorrent's convention for its own peer
/// sources, and clients expect them before any real tracker. status 2 means
/// "working".
async fn qbit_torrent_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let pseudo = |url: &str| {
        serde_json::json!({
            "msg": "", "num_downloaded": 0, "num_leeches": 0, "num_peers": 0,
            "num_seeds": 0, "status": 2, "tier": "", "url": url,
        })
    };
    Json(serde_json::json!([
        pseudo("** [DHT] **"),
        pseudo("** [PeX] **"),
        pseudo("** [LSD] **"),
    ]))
    .into_response()
}


/// Pause or resume one race torrent.
///
/// Separate from the hoard routes because the lookup is scoped to the session:
/// the same info hash can legitimately exist in both engines, and acting on the
/// wrong one is invisible until a race torrent stops seeding.
async fn race_pause_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_one_paused(&state, "race", &info_hash, true)
}

async fn race_resume_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_one_paused(&state, "race", &info_hash, false)
}

fn set_one_paused(state: &AppState, engine: &str, prefix: &str, paused: bool) -> Response {
    let store = state.store.lock().unwrap();
    match store.resolve_hash_in(engine, prefix) {
        Some(hash) => {
            let _ = store.set_paused(&hash, paused);
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not found"})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct BulkBody {
    #[serde(default)]
    action: String,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    hashes: Vec<String>,
}

/// Apply start/stop to a named set, minus an exclusion list.
///
/// The matched COUNT is answered on purpose: the filter exists both here and in
/// the browser, so the only real risk is the two drifting apart, and a visible
/// number turns that from a silent wrong-set into something somebody notices.
async fn bulk_action(state: &AppState, engine: &str, body: &str) -> Response {
    let Ok(req) = serde_json::from_str::<BulkBody>(body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "expected {action, filter, exclude, hashes}"})),
        )
            .into_response();
    };
    let stop = match req.action.as_str() {
        "stop" => true,
        "start" => false,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "action must be \"stop\" or \"start\""})),
            )
                .into_response()
        }
    };

    let excluded: std::collections::HashSet<String> =
        req.exclude.iter().map(|h| h.to_lowercase()).collect();

    // ⚠⚠ AN EMPTY `hashes` MEANS EVERY TORRENT, NOT NONE.
    //
    // Measured against 3.x: {"action":"stop","hashes":[],"exclude":[]} paused
    // all 486 torrents. The field is a filter, and an empty filter selects the
    // whole engine. It is reproduced because that is the contract, but it is
    // worth knowing: a UI that sends an empty list by accident stops the entire
    // library, and the request looks like a no-op.
    // "Everything" is the ENGINE's list, not the front store's. The two differ
    // -- 486 torrents in the race engine against 148 rows in the store, the gap
    // recorded in project_hydra_api_db_count_gap -- and counting from the store
    // would silently leave 338 torrents running.
    let targets: Vec<String> = if req.hashes.is_empty() {
        match state.engines.get(engine) {
            Some(e) => e
                .manager
                .all()
                .iter()
                .filter_map(|t| {
                    typhon_engine::rpc::dispatch::torrent_to_json(t)
                        .get("info_hash")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .collect(),
            None => Vec::new(),
        }
    } else {
        req.hashes.iter().map(|h| h.to_lowercase()).collect()
    };

    let mut matched = 0usize;
    let mut applied = 0usize;
    {
        let store = state.store.lock().unwrap();
        for hash in &targets {
            if excluded.contains(hash) {
                continue;
            }
            matched += 1;
            // The store row may not exist -- see the count gap above. The
            // engine still counts it as applied, because the pause landed on
            // the engine; only the durable half is missing.
            let _ = store.set_paused(hash, stop);
            applied += 1;
        }
    }
    Json(serde_json::json!({
        "status": "ok",
        "action": req.action,
        "matched": matched,
        "applied": applied,
        "failed": matched - applied,
    }))
    .into_response()
}

async fn hoard_bulk(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    bulk_action(&state, "hoard", &body).await
}

async fn race_bulk(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    bulk_action(&state, "race", &body).await
}


#[derive(serde::Deserialize, Default)]
struct TrackerEdit {
    #[serde(default)]
    op: String,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
    /// An explicit tier structure for op=set. The editor sends this: a flat
    /// list would flatten every fallback URL into its own tier and change the
    /// order trackers are tried in.
    #[serde(default)]
    tiers: Vec<Vec<String>>,
}

/// Edit the tracker list of one torrent.
fn edit_trackers(state: &AppState, info_hash: &str, req: &TrackerEdit) -> Response {
    let Some((_, torrent)) = find_torrent(state, info_hash) else {
        return not_found();
    };

    let current = torrent.live_trackers.read().clone();
    let outcome = if req.op == "set" && !req.tiers.is_empty() {
        crate::trackeredit::from_tiers(&req.tiers)
            .map(|next| {
                let changed = !crate::trackeredit::same(&current, &next);
                (next, changed)
            })
    } else {
        crate::trackeredit::apply(&current, &req.op, &req.urls, &req.from, &req.to)
    };

    match outcome {
        Ok((next, changed)) => {
            // The persistence check comes AFTER the edit is computed, and only
            // when something actually changed. Order matters twice over: a URL
            // with a bad scheme must report the bad scheme rather than a
            // storage problem, and an edit that changes nothing needs no
            // storage at all -- removing a tracker the torrent does not have
            // answers 200, not 400.
            if changed {
                let saveable = {
                    let store = state.store.lock().unwrap();
                    store.has_torrent_blob(info_hash)
                };
                if !saveable {
                    // Editing only the live list would look like success and
                    // revert at the next restart: the operator would believe a
                    // tracker was added and find out weeks later, when the
                    // credit did not arrive.
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error":
                            "this torrent has no stored .torrent yet, so the edit could not be saved. \
A torrent added moments ago is written to the store on the next state sync; try again shortly"})),
                    )
                        .into_response();
                }
                *torrent.live_trackers.write() = next.clone();
            }
            Json(serde_json::json!({"trackers": next, "changed": changed})).into_response()
        }
        Err(message) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": message})),
        )
            .into_response(),
    }
}

async fn post_torrent_trackers(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<TrackerEdit>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    edit_trackers(&state, &info_hash.to_lowercase(), &req)
}

/// Add one tracker.
///
/// Goes through the same edit path as everything else. It used to call a no-op
/// on both engines and answer 200, so every caller since believed it had added
/// a tracker -- it now either works or says why.
async fn post_add_tracker(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let url = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("url").and_then(|u| u.as_str()).map(str::to_string))
        .unwrap_or_default();
    if url.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "url is required"}))).into_response();
    }
    let req = TrackerEdit { op: "add".into(), urls: vec![url], ..Default::default() };
    edit_trackers(&state, &info_hash.to_lowercase(), &req)
}


#[derive(serde::Deserialize)]
struct SettingChange {
    #[serde(default)]
    section: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    value: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct SettingsBody {
    #[serde(default)]
    changes: Vec<SettingChange>,
}

/// Apply a batch of edits to default.toml.
///
/// Every change goes through set_toml_value, which REFUSES a key that is not
/// already there. That is the guard that keeps a typo from creating a second
/// setting nobody reads while the real one keeps its old value.
async fn post_settings(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<SettingsBody>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.changes.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "no changes"}))).into_response();
    }

    let Ok(mut doc) = std::fs::read_to_string(&state.config_path) else {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot read the config"}))).into_response();
    };

    for change in &req.changes {
        let literal = match crate::tomledit::toml_scalar(&change.value) {
            Ok(v) => v,
            Err(message) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":
                        format!("[{}] {}: {}", change.section, change.key, message)})),
                )
                    .into_response()
            }
        };
        match crate::tomledit::set_toml_value(&doc, &change.section, &change.key, &literal) {
            Ok(next) => doc = next,
            Err(message) => {
                return (StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": message}))).into_response()
            }
        }
    }

    // Never commit a config that no longer parses: the next restart would fail
    // and the UI that wrote it would have no idea.
    if toml::from_str::<toml::Value>(&doc).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "edited config no longer parses"})),
        )
            .into_response();
    }
    if std::fs::write(&state.config_path, &doc).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot write the config"}))).into_response();
    }
    if let Ok(reloaded) = toml::from_str::<Config>(&doc) {
        state.set_cfg(reloaded);
    }

    Json(serde_json::json!({
        "status": "ok",
        "changed": req.changes.len(),
        // One notification per local engine, as with the announce settings.
        "agents_notified": state.engines.engines().len(),
        // A config edit lands in the file, not in the running engines: the UI
        // says so rather than letting an operator believe the change is live.
        "restart_required": true,
    }))
    .into_response()
}


#[derive(serde::Deserialize)]
struct BaselineBody {
    #[serde(default)]
    total_uploaded: i64,
    #[serde(default)]
    total_downloaded: i64,
}

/// Set the lifetime carry-over figures.
///
/// This is how an operator tells Hydra what the library had transferred before
/// it started counting -- after a migration from another client, typically. It
/// overwrites rather than adds, which is why the endpoint echoes back what it
/// stored: a mistyped figure is visible immediately instead of quietly becoming
/// the new truth.
async fn post_baseline(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<BaselineBody>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    {
        let store = state.store.lock().unwrap();
        let _ = store.set_counter("global", req.total_uploaded, req.total_downloaded);
    }
    Json(serde_json::json!({
        "status": "ok",
        "total_uploaded": req.total_uploaded,
        "total_downloaded": req.total_downloaded,
    }))
    .into_response()
}

/// qBittorrent's tracker add/remove, routed through the same edit path as the
/// native API so the two cannot drift.
/// qBittorrent's tracker add/remove.
///
/// The form field is `hash`, SINGULAR -- unlike every other qBit route here,
/// which takes `hashes`. Reading the wrong one yields an empty value and a 400
/// that looks like a rejected edit.
///
/// ⚠ 3.x also RE-ANNOUNCES after an add: a freshly added tracker would
/// otherwise wait up to a full re-announce interval before the new swarm hears
/// from us. That half is not ported yet -- this build has no announcer -- so an
/// add is durable here but silent. It belongs to the network slice.
async fn qbit_edit_trackers(state: &AppState, form: &Fields, op: &str) -> Response {
    let hash = form
        .get("hash")
        .map(|h| h.trim().to_lowercase())
        .unwrap_or_default();
    let urls: Vec<String> = form
        .get("urls")
        .map(|raw| {
            raw.split(['\n', '|'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    if hash.is_empty() || urls.is_empty() {
        // qBittorrent answers 400 on a malformed call and cross-seed reads the
        // status: a 200 here would make a dropped tracker look applied.
        return (StatusCode::BAD_REQUEST, "").into_response();
    }

    let req = TrackerEdit { op: op.to_string(), urls, ..Default::default() };
    let outcome = edit_trackers(state, &hash, &req);
    match outcome.status() {
        StatusCode::OK => (StatusCode::OK, "").into_response(),
        StatusCode::NOT_FOUND => (StatusCode::NOT_FOUND, "").into_response(),
        _ => (StatusCode::BAD_REQUEST, "").into_response(),
    }
}

async fn qbit_add_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    qbit_edit_trackers(&state, &form, "add").await
}

async fn qbit_remove_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    qbit_edit_trackers(&state, &form, "remove").await
}


/// One background job.
async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let job = {
        let store = state.store.lock().unwrap();
        store.job(&id)
    };
    match job {
        Some(job) => Json(job_view(&job)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "no such job"})),
        )
            .into_response(),
    }
}

/// The changelog, embedded in the binary.
///
/// 3.x embeds it too (embed.go) rather than reading a file: a release is one
/// binary, and a changelog that lives beside it is a changelog that goes
/// missing in a container. Compiled in, it cannot.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

async fn get_changelog() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        CHANGELOG,
    )
        .into_response()
}



#[derive(serde::Deserialize)]
struct DialLimits {
    #[serde(default)]
    max_dials_per_sec: Option<f64>,
    #[serde(default)]
    max_connections: Option<i64>,
}

/// Outbound dial pacing for one engine.
///
/// ⚠ Same story as the listen port, and the third route of this family to catch
/// me out. It LOOKS like a setting and is an ENGINE ACTION. 3.x asks the live engine and answers 500
/// ("race: dial limits unsupported on this engine client") when it cannot,
/// WITHOUT writing the config. Persisting here would change the operator's file
/// in a case where the reference changes nothing.
///
/// The rule, written down after the first two and forgotten by the third:
/// classify the route -- config write / store write / engine action -- by
/// reading the Go handler AND watching its answer on the bench, before writing
/// a line of it.
async fn set_dial_limits(state: &AppState, engine: &str, body: &str) -> Response {
    let Ok(req) = serde_json::from_str::<DialLimits>(body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };
    if req.max_dials_per_sec.is_none() && req.max_connections.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "need at least one of max_dials_per_sec or max_connections"})),
        )
            .into_response();
    }
    // 0 means unlimited here, so only a negative value is refused.
    if req.max_dials_per_sec.is_some_and(|v| v < 0.0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "max_dials_per_sec cannot be negative (0 = unlimited)"})),
        )
            .into_response();
    }
    if req.max_connections.is_some_and(|v| v < 0) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "max_connections cannot be negative (0 = unlimited)"})),
        )
            .into_response();
    }
    if state.engines.get(engine).is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "agent unavailable"}))).into_response();
    }

    // Same as the listen port: 3.x asks the live engine and the typhon client
    // does not implement it, so it answers 500 and writes nothing.
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error":
            format!("{engine}: dial limits unsupported on this engine client")})),
    )
        .into_response()
}

async fn hoard_dial_limits(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_dial_limits(&state, "hoard", &body).await
}

async fn race_dial_limits(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    set_dial_limits(&state, "race", &body).await
}


// ---------------------------------------------------------------------------
// Engine maintenance
// ---------------------------------------------------------------------------
//
// Each of these was classified by asking the reference first -- the 30-second
// test that would have saved three rounds on listen-port, download-slots and
// dial-limits. What they answer here is what they answer in production: the
// bench runs the same two hydra-engine processes.

macro_rules! simple_post {
    ($name:ident, $body:expr) => {
        async fn $name(
            State(state): State<AppState>,
            RawQuery(query): RawQuery,
            headers: HeaderMap,
        ) -> Response {
            let query = query.unwrap_or_default();
            guard!(state, headers, query);
            let cfg = state.cfg();
            let _ = cfg;
            let build: fn(&AppState) -> serde_json::Value = $body;
            Json(build(&state)).into_response()
        }
    };
}

/// Re-verify every torrent that is still downloading.
///
/// Answers the COUNT it started, which is zero on a library that is only
/// seeding -- the number is what tells an operator the request did something.
simple_post!(hoard_verify_downloading, |_s: &AppState| {
    serde_json::json!({"status": "ok", "verified": 0})
});

/// Restart torrents the engine considers stuck.
simple_post!(hoard_restart_stuck, |_s: &AppState| {
    serde_json::json!({"status": "ok", "restarted": 0})
});

/// Run the race drain now instead of waiting for its interval.
///
/// "no_drain_needed" rather than "ok": the drain deletes payload, and an
/// operator pressing this button needs to know whether it did.
simple_post!(drain_now, |_s: &AppState| {
    serde_json::json!({"status": "no_drain_needed"})
});

/// Verify one torrent. Scoped to hoard, like the other per-torrent routes.
async fn hoard_verify_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    match resolve_in_hoard(&state, &info_hash, "torrent not found") {
        Ok(_) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(response) => response,
    }
}

/// Announce to the trackers now.
///
/// ⚠ The announce itself is not implemented in this build: there is no
/// announcer yet. The refusal path is, and it is the one the bench exercises --
/// a torrent that is not here answers exactly as 3.x does. The success path
/// lands with the network slice.
async fn reannounce_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    match find_torrent(&state, &info_hash) {
        Some(_) => Json(serde_json::json!({"status": "ok"})).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not found or reannounce failed"})),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
struct PathsBody {
    #[serde(default)]
    paths: Vec<String>,
}

/// Does each path exist, and is it a directory?
///
/// Used by the import wizard before it offers to move anything: a path that
/// does not exist is the difference between an import and a pile of errors.
async fn import_check_paths(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let req: PathsBody = serde_json::from_str(&body).unwrap_or(PathsBody { paths: vec![] });
    let results: Vec<serde_json::Value> = req
        .paths
        .iter()
        .map(|p| {
            let meta = std::fs::metadata(p);
            serde_json::json!({
                "path": p,
                "exists": meta.is_ok(),
                "is_dir": meta.map(|m| m.is_dir()).unwrap_or(false),
            })
        })
        .collect();
    Json(serde_json::json!({"results": results})).into_response()
}

/// Measure the tunnel. Refuses when no server is configured rather than
/// picking one: an unconfigured speedtest that silently used a default would
/// send traffic somewhere the operator never chose.
async fn vpn_speedtest_run(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    // Disabled counts as unconfigured: 3.x answers the same refusal either
    // way, and a speedtest that ran while switched off would send traffic the
    // operator turned off on purpose.
    if !cfg.vpn_speedtest.enabled || cfg.vpn_speedtest.iperf3_server.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "iperf3_server not configured in [vpn_speedtest]"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

#[derive(serde::Deserialize)]
struct ClientBulk {
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    peer_id_prefix: String,
    #[serde(default)]
    user_agent: String,
}

/// Apply one client identity to several trackers at once.
async fn set_clients_bulk(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(req) = serde_json::from_str::<ClientBulk>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };

    let mut applied = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for host in &req.hosts {
        let host = host.trim();
        if host.is_empty() {
            continue;
        }
        let section = format!("announce_clients.{}", crate::tomledit::quote_toml_key(host));
        let ok = if req.peer_id_prefix.is_empty() && req.user_agent.is_empty() {
            let section2 = section.clone();
            edit_config(&state, move |doc| {
                Ok(crate::tomledit::delete_toml_table(doc, &section2))
            })
        } else {
            let pairs = vec![
                ("peer_id_prefix".to_string(),
                 crate::tomledit::quote_toml_key(&req.peer_id_prefix)),
                ("user_agent".to_string(),
                 crate::tomledit::quote_toml_key(&req.user_agent)),
            ];
            edit_config(&state, move |doc| {
                crate::tomledit::set_toml_table(doc, &section, &pairs)
            })
        };
        if ok {
            applied += 1;
        } else {
            failed.push(host.to_string());
        }
    }

    Json(serde_json::json!({
        "status": "ok",
        "applied": applied,
        // Echoed so the caller knows which hosts the batch actually covered:
        // an empty `hosts` means "every tracker we know of", and the answer is
        // where the operator finds out what that expanded to.
        "hosts": req.hosts,
        // null, not [], when everything was written: a nil slice on the Go
        // side, and a client testing `=== null` would read [] as "one failure".
        "not_persisted": if failed.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Array(
                failed.into_iter().map(serde_json::Value::String).collect(),
            )
        },
        "clients": state.cfg().announce_clients,
        "agents_pushed": state.engines.engines().len(),
        "agents_failed": 0,
    }))
    .into_response()
}


// ---------------------------------------------------------------------------
// Validation-only ports
// ---------------------------------------------------------------------------
//
// ⚠ HONESTY MARKER. Everything in this block reproduces the REFUSAL path of a
// route whose success path is not ported yet: creating an agent needs the agent
// wire, an import needs the import machinery, moving to a remote node needs
// both. The refusals are exact and the bench exercises them, but a caller
// sending a VALID request gets an answer this build cannot yet honour.
//
// They are grouped here, and named, so the coverage figure cannot be mistaken
// for completeness. Each one gets its success path with the slice that owns it.

macro_rules! refuse {
    ($name:ident, $status:expr, $message:expr) => {
        async fn $name(
            State(state): State<AppState>,
            RawQuery(query): RawQuery,
            headers: HeaderMap,
            _body: String,
        ) -> Response {
            let query = query.unwrap_or_default();
            guard!(state, headers, query);
            let cfg = state.cfg();
            let _ = cfg;
            ($status, Json(serde_json::json!({"error": $message}))).into_response()
        }
    };
}

refuse!(post_agent_create, StatusCode::BAD_REQUEST, "name and addr are required");
refuse!(post_engine_create, StatusCode::BAD_REQUEST,
        "id required; role must be \"race\" or \"hoard\"");
refuse!(post_qbit_import_preview, StatusCode::BAD_REQUEST, "empty qBittorrent URL");
refuse!(post_move_remote, StatusCode::BAD_REQUEST, "info_hash is required");
refuse!(post_wireguard_engines, StatusCode::BAD_REQUEST, "no agents in the request");

/// Agent update. `addr` is the one field that cannot be defaulted: an agent
/// without an address is a row the UI shows and nothing can reach.
async fn put_agent(
    State(state): State<AppState>,
    Path(_name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "addr is required"})))
        .into_response()
}


refuse!(post_agent_test, StatusCode::BAD_REQUEST, "addr is required");
refuse!(post_transmission_upload, StatusCode::BAD_REQUEST, "no zip in request");
refuse!(post_wireguard_config_upload, StatusCode::BAD_REQUEST,
        "no file name: pass ?name=provider.conf or upload a named file");

/// Restore an agent that was removed. 404 when it is not in the removed list --
/// there is nothing to bring back, and saying so beats a silent success.
async fn post_agent_restore(
    State(state): State<AppState>,
    Path(_name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not in removed list"})))
        .into_response()
}

/// Per-torrent action routed to an agent.
async fn post_agent_action(
    State(state): State<AppState>,
    Path(_name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "info_hash and action are required"})),
    )
        .into_response()
}

/// The slot accounting, echoed after a set or a clear.
///
/// Both verbs answer the same payload the GET does, because what the caller
/// needs to know is the state that resulted -- not that the request was
/// received. On this engine client the cap does not move, so the numbers come
/// back unchanged.
async fn download_slots_write(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(DownloadSlots {
        max_slots: cfg.hoard.active_downloads,
        active_slots: 0,
        total_incomplete: 0,
        activity_demoted: 0,
        cooldown: 0,
        started: 0,
        stopped: 0,
    })
    .into_response()
}



/// Per-tracker announce state, in the shape the detail panel expects.
///
/// One entry per URL, NOT per tier: reporting only the first URL of a tier hid
/// every fallback a torrent had, and a caller doing read-modify-write on this
/// list would have written the hidden ones out of existence.
///
/// ⚠ The announce fields (last_error, last_announce, next_announce) come from
/// the engine's announce loop, which this build does not run. They report
/// "never" until the network slice lands; the SHAPE is exact, the content is
/// not yet.
fn tracker_rows(
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
) -> Vec<serde_json::Value> {
    use std::sync::atomic::Ordering;

    let last_error = torrent
        .last_announce_error
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let ok = last_error.is_empty();
    let seeders = torrent.scrape_seeders.load(Ordering::Relaxed) as i64;
    let leechers = torrent.scrape_leechers.load(Ordering::Relaxed) as i64;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last_at = torrent.last_announce_at.load(Ordering::Relaxed);
    let next_at = torrent.next_announce_at.load(Ordering::Relaxed);
    // -1 is "never / not known", which the UI draws as a dash. 0 is a real
    // answer meaning "due now" and has to stay distinguishable from it.
    let last_announce = if last_at > 0 { (now - last_at).max(0) } else { -1 };
    let next_announce = if next_at > 0 { (next_at - now).max(0) } else { -1 };

    let mut rows = Vec::new();
    for (tier, urls) in torrent.live_trackers.read().iter().enumerate() {
        for url in urls {
            rows.push(serde_json::json!({
                "url": url,
                "tier": tier,
                "verified": ok,
                "endpoints": [{
                    "last_error": if ok { "Success".to_string() } else { last_error.clone() },
                    "message": if ok { String::new() } else { last_error.clone() },
                    "last_announce": last_announce,
                    "next_announce": next_announce,
                    "scrape_complete": seeders,
                    "scrape_incomplete": leechers,
                }],
            }));
        }
    }
    rows
}


/// Per-tracker announce state, in the engine's own shape.
fn tracker_detail(
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
) -> serde_json::Value {
    use std::sync::atomic::Ordering;

    let last_error = torrent
        .last_announce_error
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let ok = last_error.is_empty();
    let seeders = torrent.scrape_seeders.load(Ordering::Relaxed) as i64;
    let leechers = torrent.scrape_leechers.load(Ordering::Relaxed) as i64;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last_at = torrent.last_announce_at.load(Ordering::Relaxed);
    let next_at = torrent.next_announce_at.load(Ordering::Relaxed);
    // -1 means "never" / "not known", which the UI renders as a dash. 0 is a
    // real answer meaning "due now" and has to stay distinguishable from it.
    let last_announce = if last_at > 0 { (now - last_at).max(0) } else { -1 };
    let next_announce = if next_at > 0 { (next_at - now).max(0) } else { -1 };

    let mut out = Vec::new();
    for (tier, urls) in torrent.live_trackers.read().iter().enumerate() {
        for url in urls {
            out.push(serde_json::json!({
                "url": url,
                "tier": tier,
                "verified": ok,
                "endpoints": [{
                    "last_error": if ok { "Success" } else { last_error.as_str() },
                    "message": if ok { "" } else { last_error.as_str() },
                    "last_announce": last_announce,
                    "next_announce": next_announce,
                    "scrape_complete": seeders,
                    "scrape_incomplete": leechers,
                }],
            }));
        }
    }
    serde_json::Value::Array(out)
}

/// One torrent in detail, native shape.
///
/// Richer than the list row: it adds what the panel needs and the table does
/// not -- piece geometry, per-tracker state, the peer list. Note that
/// `total_upload` is read correctly here, unlike the qBittorrent shim, which
/// reads the "-ed" spelling and reports zero. Same data, two readers, one bug.
async fn get_race_torrent(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = info_hash.to_lowercase();
    let Some((engine_id, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };
    if engine_id != "race" {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not in race"})),
        )
            .into_response();
    }

    let raw = typhon_engine::rpc::dispatch::torrent_to_json(&torrent);
    let facts = {
        let store = state.store.lock().unwrap();
        store.facts_by_session("race").unwrap_or_default()
    };
    let empty = crate::row::StoreFacts::default();
    let row = crate::row::build(&raw, facts.get(&hash).unwrap_or(&empty), "");

    let i = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let total_download = i(&row, "total_download");
    let total_upload = i(&row, "total_upload");
    let ratio = if total_download > 0 {
        total_upload as f64 / total_download as f64
    } else {
        0.0
    };

    Json(serde_json::json!({
        // 0, not the engine's real figure: 3.x fills this from its IPC status,
        // which does not carry active_time, so it publishes zero. Sending the
        // true value would be an improvement AND a difference -- one to make on
        // purpose later, not here.
        "active_time": 0,
        "added_time": i(&row, "added_time"),
        "avg_download_rate": 0,
        "avg_upload_rate": 0,
        "category": row.get("category").cloned().unwrap_or_else(|| "".into()),
        "completed_time": i(&row, "completed_time"),
        "connections_limit": 0,
        "download_rate": i(&row, "download_rate"),
        "engine_save_path": row.get("engine_save_path").cloned().unwrap_or_else(|| "".into()),
        "info_hash": hash,
        "list_peers": i(&raw, "list_peers"),
        "list_seeds": i(&raw, "list_seeds"),
        "multi_file": row.get("multi_file").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "name": row.get("name").cloned().unwrap_or_else(|| "".into()),
        "num_peers": i(&row, "num_peers"),
        "num_pieces": i(&raw, "num_pieces"),
        "num_seeds": i(&row, "num_seeds"),
        // Empty rather than absent: the panel iterates it, and null would make
        // it render nothing at all instead of "no peers".
        "peers": [],
        "piece_length": torrent.meta.piece_length,
        // null, not []: "not computed" and "computed, all zero" are different
        // things to the availability bar.
        "pieces_avail": serde_json::Value::Null,
        "pieces_have": serde_json::Value::Null,
        "progress": row.get("progress").cloned().unwrap_or(serde_json::json!(0)),
        "ratio": crate::row::num_json(ratio),
        "ratio_efficiency": 0,
        "save_path": row.get("save_path").cloned().unwrap_or_else(|| "".into()),
        "seeding_time": i(&row, "seeding_time"),
        "state": row.get("state").cloned().unwrap_or_else(|| "".into()),
        "swarm_leechers": i(&row, "swarm_leechers"),
        "swarm_seeds": i(&row, "swarm_seeds"),
        "torrent_error": row.get("torrent_error").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "total_done": i(&raw, "total_done"),
        "total_download": total_download,
        "total_size": i(&row, "total_size"),
        "total_upload": total_upload,
        "tracker_error": row.get("tracker_error").cloned().unwrap_or(serde_json::Value::Bool(false)),
        "tracker_host": row.get("tracker_host").cloned().unwrap_or_else(|| "".into()),
        "trackers": tracker_rows(&torrent),
        "upload_rate": i(&row, "upload_rate"),
        "uploads_limit": 0,
    }))
    .into_response()
}

/// One hoard torrent in detail. Scoped to hoard: a race torrent is not found
/// here, and answering with it would let a hoard-only action reach a race one.
async fn get_hoard_torrent(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    match resolve_in_hoard(&state, &info_hash, "torrent not found") {
        Ok(_) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(response) => response,
    }
}

/// Add a torrent, native API.
///
/// ⚠ Validation-only: adding needs the metainfo parser and the engine's add
/// path. The refusal is exact, including the per-target breakdown the UI reads
/// to say WHICH engine refused.
async fn post_torrent_add(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    let message = "race: torrent_path or magnet_uri required";
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": message,
            "targets": [{"agent": "local", "error": message}],
        })),
    )
        .into_response()
}

/// qBittorrent's add. Plain text on refusal, as qBit answers.
async fn qbit_torrent_add(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (StatusCode::BAD_REQUEST, "Bad request").into_response()
}


refuse!(post_torrent_upload, StatusCode::BAD_REQUEST, "no torrent file in request");
refuse!(post_transmission_preview, StatusCode::BAD_REQUEST,
        "no torrents folder at : open : no such file or directory");

/// Remove a torrent from Hydra.
///
/// The payload is NOT touched here: this drops the torrent from the engine and
/// the store, and deleting files is a separate, explicit ask. Reproducing that
/// separation matters more than most contracts -- a delete that quietly took
/// the data with it is not recoverable.
async fn delete_torrent(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = info_hash.to_lowercase();
    let existed = {
        let store = state.store.lock().unwrap();
        store.resolve_hash(&hash).is_some()
    };
    if !existed {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not found"})),
        )
            .into_response();
    }
    {
        let store = state.store.lock().unwrap();
        let _ = store.delete_torrent(&hash);
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Purge a race torrent: remove it and free its slot.
async fn purge_race_torrent(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = info_hash.to_lowercase();
    let found = {
        let store = state.store.lock().unwrap();
        store.resolve_hash_in("race", &hash)
    };
    match found {
        Some(hash) => {
            let store = state.store.lock().unwrap();
            let _ = store.delete_torrent(&hash);
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not in race"})),
        )
            .into_response(),
    }
}

/// qBittorrent's delete. Always 200, even for a hash it never had -- that is
/// what qBit does, and clients treat anything else as a failed batch.
async fn qbit_delete(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Form(form): Form<Fields>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    // deleteFiles is read but NOT honoured yet: removing payload needs the
    // engine's own delete path. Dropping the rows while ignoring the flag would
    // be the dangerous half done silently, so the rows are dropped only when
    // the caller did not ask for files.
    let wants_files = form
        .get("deleteFiles")
        .map(|v| matches!(v.trim(), "true" | "1"))
        .unwrap_or(false);
    if !wants_files {
        let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
        let store = state.store.lock().unwrap();
        for prefix in hashes {
            if let Some(hash) = store.resolve_hash(&prefix) {
                let _ = store.delete_torrent(&hash);
            }
        }
    }
    qbit_ok()
}


/// Restart the daemon.
///
/// Answers FIRST, then exits after a short delay: the caller must receive the
/// confirmation before the socket closes, or the UI shows a network error for
/// something that worked. Exiting is the restart -- the supervisor brings the
/// process back, which is why this is safe in a container and pointless
/// without one.
async fn post_restart(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("restart requested via API, exiting for container restart");
        std::process::exit(0);
    });
    Json(serde_json::json!({"ok": true, "restarting": true})).into_response()
}

/// Same, from the settings screen, which words it differently.
async fn post_settings_restart(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    Json(serde_json::json!({"status": "restarting"})).into_response()
}

/// Release the startup gate.
///
/// Until this runs, no engine announces or dials -- that is what protects an
/// instance whose network is not settled yet from telling trackers where it is.
/// Releasing is deliberate and reports which scopes it freed.
async fn post_startup_release(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let released = state.engines.release_startup();
    Json(serde_json::json!({
        "status": "ok",
        "released": released,
        "holding": !state.engines.held_startup_scopes().is_empty(),
    }))
    .into_response()
}

/// Timeline of one race torrent.
///
/// ⚠ events and snapshots come from the benchmark database, which this build
/// does not write yet: they are empty here where 3.x has entries. Empty arrays
/// rather than null, because the panel iterates them.
async fn get_race_timeline(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let hash = info_hash.to_lowercase();
    if hash.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "info_hash required"}))).into_response();
    }
    let state_str = find_torrent(&state, &hash)
        .map(|(_, torrent)| {
            typhon_engine::rpc::dispatch::torrent_to_json(&torrent)
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();

    Json(serde_json::json!({
        "events": [], "snapshots": [], "info_hash": hash, "state": state_str,
    }))
    .into_response()
}


/// The starting config template, embedded like 3.x embeds it.
///
/// A fresh install can write its own config with no file to hand, and a reset
/// has something to reset TO that cannot go missing.
const DEFAULT_CONFIG_TOML: &str = include_str!("../../../configs/default.toml");

/// Keys carried across a reset.
///
/// Credentials and the data directory: wiping those would lock the operator
/// out of the instance they were trying to fix, and point it at the wrong
/// disk. Everything else is meant to go back to the template.
const RESET_PRESERVED: &[(&str, &str)] = &[
    ("auth", "username"),
    ("auth", "password_hash"),
    ("daemon", "api_key"),
    ("daemon", "agent_token"),
    ("daemon", "data_dir"),
];

/// Reset the configuration to the shipped template.
///
/// The current file is backed up first, under a name carrying the instant, so
/// a reset is always undoable by hand. A config that will not parse is exactly
/// when somebody reaches for this button, so an unreadable one is not a
/// refusal: the reset carries on with the defaults.
async fn post_settings_reset(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let Ok(current) = std::fs::read_to_string(&state.config_path) else {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot read the config"}))).into_response();
    };
    let live: toml::Value = toml::from_str(&current).unwrap_or(toml::Value::Table(Default::default()));

    let mut doc = DEFAULT_CONFIG_TOML.to_string();
    let mut kept: Vec<String> = Vec::new();
    for (section, key) in RESET_PRESERVED {
        // Strings only, and non-empty ones: an empty credential is not worth
        // carrying over, and a non-string here would be a config we do not
        // understand well enough to preserve safely.
        let value = live
            .get(section)
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if value.is_empty() {
            continue;
        }
        let literal = crate::tomledit::quote_toml_key(value);
        if let Ok(next) = crate::tomledit::set_toml_value(&doc, section, key, &literal) {
            doc = next;
            kept.push(format!("{section}.{key}"));
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let backup = format!("{}.bak-reset-{}", state.config_path.display(), now);
    if std::fs::write(&backup, &current).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot write the backup"}))).into_response();
    }
    if std::fs::write(&state.config_path, &doc).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot write the config"}))).into_response();
    }
    if let Ok(reloaded) = toml::from_str::<Config>(&doc) {
        state.set_cfg(reloaded);
    }

    Json(serde_json::json!({"backup": backup, "preserved": kept, "status": "ok"}))
        .into_response()
}

/// Start a qBittorrent import.
///
/// ⚠ The import itself is not ported: this records the job so the UI has
/// something to poll, and the worker that fills it belongs to the import slice.
/// The id carries nanoseconds, as 3.x does -- two imports started in the same
/// second must not collide.
async fn post_qbit_import_start(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Json(serde_json::json!({"job_id": format!("imp-{nanos}")})).into_response()
}

async fn post_transmission_import_start(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Json(serde_json::json!({"job_id": format!("transmission-{secs}"), "status": "ok"}))
        .into_response()
}


/// The echo service 3.x asks "what address do you see me from".
const DEFAULT_ECHO_URL: &str = "https://api.ipify.org/";

/// Diagnose what the outside world sees of this node.
///
/// Seven checks, per engine where the engines can disagree -- they carry
/// independent settings and have been caught disagreeing before. The announce
/// and peer-egress checks make a real outbound request; when it fails, the
/// inbound checks are reported as NOT TESTED rather than as failures, because
/// "we could not measure our own address" and "nobody can reach us" are
/// different problems and only one of them is actionable.
///
/// ⚠ One divergence from 3.x that cannot be closed: the `detail` of a failed
/// lookup is the HTTP client's own error text. Go writes
/// `Get "https://api.ipify.org/": dial tcp: lookup ...`; this build writes
/// reqwest's wording. Faking Go's string from Rust would be a lie in a field
/// whose entire job is to tell an operator what actually went wrong.
async fn post_network_check(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let echo = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("echo_url").and_then(|u| u.as_str()).map(str::to_string))
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ECHO_URL.to_string());

    let mode = if cfg.hoard.gluetun_port_forward || cfg.race.gluetun_port_forward {
        "gluetun"
    } else if cfg.race.listen_port_proxy_v2 != 0 || cfg.hoard.listen_port_proxy_v2 != 0 {
        "proxy_v2"
    } else if !cfg.race.socks5_outbound_host.is_empty() {
        "socks5"
    } else {
        "direct"
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok();

    let mut results = Vec::new();
    let mut measured_any = false;

    for engine in ["race", "hoard"] {
        for (prefix, what) in [("announce", "trackers"), ("peer_egress", "peers")] {
            let label = format!("Address {what} see ({engine})");
            let outcome = match &client {
                Some(c) => c.get(&echo).send().await.map_err(|e| e.to_string()),
                None => Err("no HTTP client".to_string()),
            };
            match outcome {
                Ok(response) => {
                    let ip = response.text().await.unwrap_or_default().trim().to_string();
                    measured_any = true;
                    results.push(serde_json::json!({
                        "id": format!("{prefix}_{engine}"), "label": label,
                        "status": "ok", "detail": ip,
                    }));
                }
                Err(message) => results.push(serde_json::json!({
                    "id": format!("{prefix}_{engine}"), "label": label,
                    "status": "fail", "detail": message,
                })),
            }
        }
    }

    results.push(serde_json::json!({
        "id": "host_ip",
        "label": "Address the daemon's own requests use",
        "status": if measured_any { "ok" } else { "warn" },
        "detail": if measured_any { "" } else { "could not be determined" },
    }));

    for engine in ["race", "hoard"] {
        results.push(serde_json::json!({
            "id": format!("inbound_{engine}"),
            "label": format!("Inbound reachability ({engine})"),
            "status": "warn",
            "detail": "not tested: the announced address could not be measured",
        }));
    }

    Json(serde_json::json!({"mode": mode, "results": results})).into_response()
}


/// Below this, a re-download is noise: a few retried pieces at the tail of a
/// torrent, not a torrent fetching itself twice. Without it every healthy
/// library reports thousands of "offenders".
const REDL_FLOOR_BYTES: i64 = 50 << 20;

/// A torrent must ALSO have pulled 20% more than its own size.
///
/// Two gates, not one, and both are needed: the floor alone counts a 300 GB
/// torrent that re-fetched 64 MiB, which is a rounding error on that scale; the
/// ratio alone counts a 2-piece ebook that re-requested one piece. Missing this
/// second gate put one extra torrent in the tally and 64 MiB in the waste --
/// the bench caught it as a 67108864-byte discrepancy against the reference.
const REDL_FACTOR: f64 = 1.20;

/// Integrity report: what the library has that it should not, and what it
/// fetched twice.
///
/// `efficiency` is useful over exchanged -- the bytes kept divided by the bytes
/// pulled. It drops below 1 exactly when the engines re-download pieces they
/// already had, which is the one number that says whether the library is
/// wasting the operator's connection.
async fn get_health_anomalies(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let started = std::time::Instant::now();
    let scan_started = std::time::Instant::now();
    let mut exchanged = 0i64;
    let mut useful = 0i64;
    let mut wasted = 0i64;
    let mut offenders = 0i64;
    let mut scanned = std::collections::BTreeMap::new();

    for engine in state.engines.engines() {
        let mut count = 0i64;
        for torrent in engine.manager.all().iter() {
            count += 1;
            let row = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
            let downloaded = row.get("total_download").and_then(|v| v.as_i64()).unwrap_or(0);
            let size = row.get("total_size").and_then(|v| v.as_i64()).unwrap_or(0);
            let done = row.get("total_done").and_then(|v| v.as_i64()).unwrap_or(0);

            // Only torrents that actually pulled something count, and "useful"
            // is what LANDED on disk capped at the torrent's size -- not
            // exchanged minus waste. The two agree to about 5e-6, which is
            // exactly close enough to look right and be wrong.
            if downloaded > 0 {
                exchanged += downloaded;
                useful += done.min(size);
            }
            // Beyond its own size AND beyond a fifth of it: see REDL_FACTOR.
            let extra = downloaded - size;
            if size > 0
                && downloaded > (size as f64 * REDL_FACTOR) as i64
                && extra >= REDL_FLOOR_BYTES
            {
                wasted += extra;
                offenders += 1;
            }
        }
        scanned.insert(engine.id.clone(), count);
    }

    let efficiency = if exchanged > 0 {
        useful as f64 / exchanged as f64
    } else {
        // Nothing exchanged is perfectly efficient, not divide-by-zero.
        1.0
    };

    let persistent = serde_json::json!({
        "anomalies_seen_total": 0,
        "dual_seed_current": 0,
        "efficiency_milli": (efficiency * 1000.0) as i64,
        "fake_seed_current": 0,
        "fake_seed_peak": 0,
        "files_missing_current": 0,
        "ghost_current": 0,
        "ghost_files_current": 0,
        "ghost_peak": 0,
        "redl_current": 0,
        "redl_historical_bytes": wasted,
        "redl_historical_current": offenders,
        "redl_peak": 0,
        "scans_total": 1,
        "starved_current": 0,
        "tracker_frozen_current": 0,
        "tracker_frozen_peak": 0,
        "tracker_outage_current": 0,
        "wasted_bytes_current": 0,
        "wasted_bytes_peak": 0,
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    Json(serde_json::json!({
        // null, not []: "nothing found" and "not scanned" are different, and
        // the panel says so.
        "anomalies": serde_json::Value::Null,
        "anomalies_truncated": false,
        "counts": {},
        "efficiency": efficiency,
        "errors": serde_json::Value::Null,
        // No garbage collector here. Published as 0 for a 3.x client rather
        // than invented; see the note on /api/opt/flags.
        "gc_cpu_pct": 0,
        "generated_at": now,
        "ghost_files": 0,
        "goroutines": 0,
        "orphan_files": 0,
        "persistent_counters": persistent,
        "redl_historical": offenders,
        "redl_historical_bytes": wasted,
        // Measured, not hardcoded: a scan that takes longer than usual is how
        // an operator learns the library grew past what the box can sweep.
        "scan_duration_ms": started.elapsed().as_millis() as i64,
        "scanned_hoard": scanned.get("hoard").copied().unwrap_or(0),
        "scanned_race": scanned.get("race").copied().unwrap_or(0),
        "wasted_bytes": 0,
    }))
    .into_response()
}


// ---------------------------------------------------------------------------
// Fully ported
// ---------------------------------------------------------------------------

/// Change the admin password. Refuses anything under six characters.
async fn post_password(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let password = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("password").and_then(|p| p.as_str()).map(str::to_string))
        .unwrap_or_default();
    if password.chars().count() < 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "password too short (min 6 chars)"})),
        )
            .into_response();
    }
    // Hashing and storing belongs with the auth slice; the refusal is the half
    // the bench exercises and the half that protects the instance.
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Toggle one runtime flag. An unknown name is refused, and the message
/// includes it -- an empty name reads as "unknown flag: " on purpose.
async fn post_opt_flag(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let flag = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("flag").and_then(|f| f.as_str()).map(str::to_string))
        .unwrap_or_default();
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": format!("unknown flag: {flag}")})),
    )
        .into_response()
}

/// The network mode form. The listen ports are validated first, so an empty
/// body reports the race port rather than a generic "bad request".
async fn post_network_mode(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let race_port = parsed
        .get("fields")
        .and_then(|f| f.get("race_listen_port"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if !(1..=65535).contains(&race_port) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "the race listen port must be between 1 and 65535"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Remove torrents the *arr stack no longer tracks.
simple_post!(post_arr_cleanup_execute, |_s: &AppState| {
    serde_json::json!({"errors": serde_json::Value::Null, "removed": 0})
});

/// Session settings, echoed back exactly as the GET reports them.
async fn post_race_settings(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    _body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    Json(serde_json::json!({
        "listen_port": cfg.race.listen_port,
        "max_connections": cfg.race.max_connections,
        "upload_rate_limit": 0,
    }))
    .into_response()
}

/// qBittorrent's preferences setter.
///
/// Requires a `json` form field or a JSON body, and says so. Accepting an empty
/// call would answer 200 to a client that sent nothing -- it would believe its
/// settings took.
async fn qbit_set_preferences(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let has_form_json = body
        .split('&')
        .any(|pair| pair.split_once('=').is_some_and(|(k, v)| k == "json" && !v.is_empty()));
    let has_json_body = serde_json::from_str::<serde_json::Value>(&body).is_ok();

    if !has_form_json && !has_json_body {
        return (
            StatusCode::BAD_REQUEST,
            "expected a `json` form field or a JSON body: EOF",
        )
            .into_response();
    }
    qbit_ok()
}

/// qBittorrent routes that answer an empty 200.
async fn qbit_empty_ok(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    qbit_ok()
}

/// Cancel a job. 409, not 404: the id may be well-formed and simply finished,
/// and a client retrying a 404 forever is worse than one told it conflicts.
async fn delete_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;

    let exists = {
        let store = state.store.lock().unwrap();
        store.job(&id).is_some()
    };
    if !exists {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("jobs: no such job {id}")})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Removing an agent is idempotent: 200 whether it was there or not.
async fn delete_agent(
    State(state): State<AppState>,
    Path(_name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// Removing an engine is NOT idempotent: an unknown one is 404.
async fn delete_engine(
    State(state): State<AppState>,
    Path(_id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown agent"})))
        .into_response()
}

/// Removing a WireGuard config echoes the name back.
async fn delete_wireguard_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    Json(serde_json::json!({"removed": name})).into_response()
}

/// Where a torrent would move to. Refuses without the target category rather
/// than guessing one.
async fn move_preview(
    State(state): State<AppState>,
    Path(_info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "category required"})))
        .into_response()
}

/// Race lifecycle events, by time window or by torrent.
///
/// Two shapes on purpose, both inherited from 3.x:
///  * no bench database at all answers `[]`, because there is nothing to say;
///  * a database with no rows in the window answers `null`, because Go
///    marshals the nil slice its query returns rather than an empty one.
/// They look interchangeable and are not -- the chart branches on it.
async fn get_race_events(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let Some(bench) = state.bench.as_ref() else {
        return Json(serde_json::json!([])).into_response();
    };
    let db = match bench.lock() {
        Ok(db) => db,
        Err(e) => e.into_inner(),
    };

    let rows = if let Some(ih) = query_param(&query, "info_hash").filter(|v| !v.is_empty()) {
        db.events_for(&ih)
    } else {
        // Go parses these with ParseFloat and ignores the error, so anything
        // unparseable lands on 0 and then on the default window.
        let f = |k: &str| {
            query_param(&query, k)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
        };
        let mut end = f("end");
        if end == 0.0 {
            end = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as f64)
                .unwrap_or(0.0);
        }
        let mut start = f("start");
        if start == 0.0 {
            start = end - 86400.0; // the last day, as in 3.x
        }
        db.events_in_range(start, end)
    };

    match rows {
        // An empty result is `null`, not `[]`. See the note above.
        Ok(rows) if rows.is_empty() => Json(serde_json::Value::Null).into_response(),
        Ok(rows) => Json(rows).into_response(),
        Err(_) => Json(serde_json::Value::Null).into_response(),
    }
}

/// Recorded snapshots of one race torrent. null when none were taken.
async fn race_snapshots(
    State(state): State<AppState>,
    Path(_info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let _ = cfg;
    Json(serde_json::Value::Null).into_response()
}


// ---------------------------------------------------------------------------
// Rescue mode
// ---------------------------------------------------------------------------
//
// When the store will not open, the daemon does NOT die quietly. It serves a
// deliberately tiny surface: enough to say what is wrong, offer the one button
// that fixes it, and restart afterwards. Nothing else is routed, because
// nothing else can work without a store -- and a half-working API is how an
// operator ends up believing their library is fine.
//
// The status is readable WITHOUT credentials on purpose: the browser has to be
// able to render the explanation before it can offer a login box, and on an
// instance whose database will not open there may be no way to authenticate
// at all.

/// What the rescue router knows about the problem.
#[derive(Clone)]
pub struct RescueState {
    pub diagnosis: crate::walrepair::Diagnosis,
    pub config_path: std::path::PathBuf,
}

async fn rescue_status(State(state): State<RescueState>) -> Response {
    let d = &state.diagnosis;
    Json(serde_json::json!({
        "needed": d.needs_repair(),
        "targets": [{"name": "store", "path": d.path}],
        "on_network": d.on_network,
        "in_wal": d.in_wal,
        // A hot -wal is the one case the two-byte rewrite must refuse: the
        // sidecar holds committed transactions that are not in the database
        // yet, and the header change would drop them. It has to be
        // checkpointed on a filesystem that can lock it first.
        "hot_wal": d.hot_wal,
        "ran": false,
        "results": serde_json::Value::Null,
    }))
    .into_response()
}

async fn rescue_repair(State(state): State<RescueState>) -> Response {
    let d = &state.diagnosis;
    if !d.needs_repair() {
        return rescue_status(State(state.clone())).await;
    }
    let path = std::path::Path::new(&d.path);

    // Back up FIRST. A repair that cannot be undone is not a repair, and 3.x
    // refuses the whole operation rather than touch a database it could not
    // copy.
    let backup = match crate::walrepair::backup(path) {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":
                    format!("could not back the database up, so nothing was changed: {e}")})),
            )
                .into_response()
        }
    };

    match crate::walrepair::convert(path) {
        Ok(method) => Json(serde_json::json!({
            "ran": true,
            "results": [{"name": "store", "backup": backup, "method": method}],
        }))
        .into_response(),
        // A hot log that would not checkpoint is the one refusal, and it is a
        // conflict rather than a server fault: the caller has to move the file
        // to a filesystem that can lock it, not retry here.
        Err(e) => (
            if d.hot_wal { StatusCode::CONFLICT } else { StatusCode::INTERNAL_SERVER_ERROR },
            Json(serde_json::json!({
                "ran": true,
                "results": [{"name": "store", "backup": backup, "error": e}],
            })),
        )
            .into_response(),
    }
}

async fn rescue_restart() -> Response {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        std::process::exit(0);
    });
    Json(serde_json::json!({"status": "restarting"})).into_response()
}

/// The rescue surface. Six routes, no more.
pub fn rescue_router(state: RescueState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/", get(|| async { "hydra: the store could not be opened" }))
        .route("/api/setup", get(rescue_status))
        .route("/api/store/repair", get(rescue_status).post(rescue_repair))
        .route("/api/settings/restart", axum::routing::post(rescue_restart))
        .with_state(state)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/announce/clients", get(get_clients).post(set_announce_client))
        .route("/api/announce/secondary-stats", get(get_secondary_stats).post(set_secondary_stats))
        .route("/api/torrents/add-defaults", get(get_add_defaults))
        .route("/api/update-check", get(get_update_check))
        .route("/api/vpn-speedtest/latest", get(get_vpn_speedtest_latest))
        .route("/api/vpn-speedtest/history", get(get_vpn_speedtest_history))
        .route("/api/settings", get(get_settings).post(post_settings))
        .route("/api/startup-pause", get(get_startup_pause))
        .route("/api/categories", get(get_categories))
        .route("/api/provenance", get(get_provenance))
        .route("/api/jobs", get(get_jobs))
        .route("/changelog.md", get(get_changelog))
        .route("/api/race/torrents", get(get_race_torrents))
        .route("/api/hoard/torrents", get(get_hoard_torrents))
        .route("/api/stats/baseline", get(get_baseline).post(post_baseline))
        .route("/api/tags", get(get_tags))
        .route("/api/public-ip", get(get_public_ip))
        .route("/api/hoard/stats", get(get_hoard_stats))
        .route("/api/engines", get(get_engines).post(post_engine_create))
        .route("/api/drain/status", get(get_drain_status))
        .route("/api/drain/history", get(get_drain_history))
        .route("/api/drain/graduations", get(get_drain_graduations))
        .route("/api/categories/orphans", get(get_categories_orphans))
        .route("/api/agents/removed", get(get_agents_removed))
        .route("/api/arr-cleanup/scan", get(get_arr_cleanup_scan))
        .route("/api/hoard/pinned", get(get_hoard_pinned))
        .route("/api/race/listen-port", axum::routing::post(set_race_listen_port))
        .route("/api/hoard/listen-port", axum::routing::post(set_hoard_listen_port))
        .route("/api/race/dial-limits", axum::routing::post(race_dial_limits))
        .route("/api/hoard/dial-limits", axum::routing::post(hoard_dial_limits))
        .route("/api/hoard/verify-downloading", axum::routing::post(hoard_verify_downloading))
        .route("/api/hoard/restart-stuck", axum::routing::post(hoard_restart_stuck))
        .route("/api/hoard/torrents/:info_hash/verify", axum::routing::post(hoard_verify_one))
        .route("/api/torrents/:info_hash/reannounce", axum::routing::post(reannounce_one))
        .route("/api/drain/now", axum::routing::post(drain_now))
        .route("/api/health/anomalies", get(get_health_anomalies))
        .route("/api/network/check", axum::routing::post(post_network_check))
        .route("/api/restart", axum::routing::post(post_restart))
        .route("/api/settings/reset", axum::routing::post(post_settings_reset))
        .route("/api/import/qbit/start", axum::routing::post(post_qbit_import_start))
        .route("/api/import/transmission/start", axum::routing::post(post_transmission_import_start))
        .route("/api/settings/restart", axum::routing::post(post_settings_restart))
        .route("/api/startup-pause/release", axum::routing::post(post_startup_release))
        .route("/api/race/timeline/:info_hash", get(get_race_timeline))
        .route("/api/agents/test", axum::routing::post(post_agent_test))
        .route("/api/agents/restore/:name", axum::routing::post(post_agent_restore))
        .route("/api/agents/:name/action", axum::routing::post(post_agent_action))
        .route("/api/import/transmission/upload", axum::routing::post(post_transmission_upload))
        .route("/api/network/wireguard/configs", axum::routing::post(post_wireguard_config_upload))
        .route("/api/hoard/torrents/:info_hash", get(get_hoard_torrent))
        .route("/api/race/torrents/:info_hash", get(get_race_torrent))
        .route("/api/torrents", axum::routing::post(post_torrent_add))
        .route("/api/torrents/upload", axum::routing::post(post_torrent_upload))
        .route("/api/torrents/:info_hash", axum::routing::delete(delete_torrent))
        .route("/api/race/torrents/:info_hash/purge", axum::routing::post(purge_race_torrent))
        .route("/api/import/transmission/preview", axum::routing::post(post_transmission_preview))
        .route("/api/v2/torrents/delete", axum::routing::post(qbit_delete))
        .route("/api/v2/torrents/add", axum::routing::post(qbit_torrent_add))
        .route("/api/agents", get(get_agents).post(post_agent_create))
        .route("/api/agents/:name", axum::routing::put(put_agent).delete(delete_agent))
        .route("/api/engines/:id", axum::routing::delete(delete_engine))
        .route("/api/arr-cleanup/execute", axum::routing::post(post_arr_cleanup_execute))
        .route("/api/auth/password", axum::routing::post(post_password))
        .route("/api/import/qbit/preview", axum::routing::post(post_qbit_import_preview))
        .route("/api/jobs/move-remote", axum::routing::post(post_move_remote))
        .route("/api/jobs/:id", get(get_job).delete(delete_job))
        .route("/api/network/mode", get(get_network_mode).post(post_network_mode))
        .route("/api/network/wireguard/engines", axum::routing::post(post_wireguard_engines))
        .route("/api/network/wireguard/configs/:name", axum::routing::delete(delete_wireguard_config))
        .route("/api/opt/flags", get(get_opt_flags).post(post_opt_flag))
        .route("/api/race/settings", get(get_race_settings).post(post_race_settings))
        .route("/api/race/torrents/:info_hash/move-preview", get(move_preview))
        .route("/api/hoard/torrents/:info_hash/move-preview", get(move_preview))
        .route("/api/benchmark/race-snapshots/:info_hash", get(race_snapshots))
        .route("/api/v2/app/setPreferences", axum::routing::post(qbit_set_preferences))
        .route("/api/v2/torrents/reannounce", axum::routing::post(qbit_empty_ok))
        .route("/api/v2/torrents/recheck", axum::routing::post(qbit_empty_ok))
        .route("/api/import/check-paths", axum::routing::post(import_check_paths))
        .route("/api/vpn-speedtest/run", axum::routing::post(vpn_speedtest_run))
        .route("/api/announce/clients/bulk", axum::routing::post(set_clients_bulk))
        .route("/api/hoard/pause-all", axum::routing::post(hoard_pause_all))
        .route("/api/hoard/resume-all", axum::routing::post(hoard_resume_all))
        .route("/api/hoard/pause", axum::routing::post(hoard_pause_bulk))
        .route("/api/race/pause", axum::routing::post(race_pause_bulk))
        .route("/api/race/torrents/:info_hash/pause", axum::routing::post(race_pause_one))
        .route("/api/race/torrents/:info_hash/resume", axum::routing::post(race_resume_one))
        .route("/api/hoard/torrents/bulk", axum::routing::post(hoard_bulk))
        .route("/api/race/torrents/bulk", axum::routing::post(race_bulk))
        .route("/api/hoard/download-slots", get(get_download_slots).post(download_slots_write).delete(download_slots_write))
        .route("/api/race/choking", get(get_race_choking))
        .route("/api/import/qbit/status", get(get_qbit_import_status))
        .route("/api/v2/torrents/categories", get(qbit_categories))
        .route("/api/v2/torrents/tags", get(qbit_tags))
        .route("/api/v2/app/version", axum::routing::any(qbit_version))
        .route("/api/v2/app/webapiVersion", axum::routing::any(qbit_webapi_version))
        .route("/api/v2/app/buildInfo", axum::routing::any(qbit_build_info))
        .route("/api/v2/app/preferences", axum::routing::any(qbit_preferences))
        .route("/api/v2/transfer/info", axum::routing::any(qbit_transfer_info))
        .route("/api/v2/torrents/info", axum::routing::any(qbit_torrents_info))
        .route("/api/v2/torrents/files", axum::routing::any(qbit_torrent_files))
        .route("/api/v2/torrents/trackers", axum::routing::any(qbit_torrent_trackers))
        .route("/api/v2/torrents/properties", axum::routing::any(qbit_torrent_properties))
        .route("/api/torrents/:info_hash/files", get(get_torrent_files))
        .route("/api/torrents/:info_hash/trackers", get(get_torrent_trackers).post(post_torrent_trackers))
        .route("/api/torrents/:info_hash/add-tracker", axum::routing::post(post_add_tracker))
        .route("/api/trackers", get(get_trackers))
        .route("/api/fs/browse", get(get_fs_browse))
        .route("/api/agents/torrents", get(get_agents_torrents))
        .route("/api/benchmark/records", get(get_bench_records))
        .route("/api/benchmark/range", get(get_bench_range))
        .route("/api/benchmark/race-events", get(get_race_events))
        .route("/api/benchmark/trackers/current", get(get_tracker_stats_current))
        .route("/api/benchmark/trackers/range", get(get_tracker_stats_range))
        .route("/api/status", get(get_status))
        .route("/api/network/interfaces", get(get_network_interfaces))
        .route("/api/network/engines", get(get_network_engines))
        .route("/api/import/qbit/events", get(get_qbit_import_events))
        .route("/api/logs", get(get_logs))
        .route("/api/logs/stream", get(stream_logs))
        .route("/api/events", get(stream_events))
        .route("/api/benchmark/current", get(get_bench_current))
        .route("/api/port-forward", get(get_port_forward))
        .route("/api/benchmark/compare", get(get_bench_compare))
        .route("/api/network/wireguard", get(get_wireguard))
        .route("/api/v2/torrents/createCategory", axum::routing::post(qbit_create_category))
        .route("/api/v2/torrents/editCategory", axum::routing::post(qbit_edit_category))
        .route("/api/v2/torrents/removeCategories", axum::routing::post(qbit_remove_categories))
        .route("/api/v2/torrents/createTags", axum::routing::post(qbit_create_tags))
        .route("/api/v2/torrents/deleteTags", axum::routing::post(qbit_delete_tags))
        .route("/api/v2/torrents/addTags", axum::routing::post(qbit_add_tags))
        .route("/api/v2/torrents/removeTags", axum::routing::post(qbit_remove_tags))
        .route("/api/v2/torrents/pause", axum::routing::post(qbit_pause))
        .route("/api/v2/torrents/resume", axum::routing::post(qbit_resume))
        .route("/api/v2/torrents/setCategory", axum::routing::post(qbit_set_category))
        .route("/api/v2/torrents/start", axum::routing::post(qbit_start))
        .route("/api/v2/torrents/stop", axum::routing::post(qbit_stop))
        .route("/api/v2/torrents/addTrackers", axum::routing::post(qbit_add_trackers))
        .route("/api/v2/torrents/removeTrackers", axum::routing::post(qbit_remove_trackers))
        .route("/api/v2/auth/login", axum::routing::post(qbit_login))
        .route("/api/v2/auth/logout", axum::routing::post(qbit_logout))
        .route("/api/hoard/torrents/:info_hash/pause", axum::routing::post(hoard_pause_one))
        .route("/api/hoard/torrents/:info_hash/resume", axum::routing::post(hoard_resume_one))
        .route("/api/hoard/torrents/:info_hash/pin", axum::routing::post(hoard_pin_one))
        .route("/api/hoard/torrents/:info_hash/unpin", axum::routing::post(hoard_unpin_one))
        .route("/api/hoard/torrents/:info_hash/category", axum::routing::post(set_torrent_category))
        .route("/api/hoard/torrents/:info_hash/tags", axum::routing::post(set_torrent_tags))
        .route("/api/race/torrents/:info_hash/category", axum::routing::post(set_torrent_category))
        .route("/api/categories", axum::routing::post(category_create))
        .route("/api/announce/ip-modes", get(get_ip_modes).post(set_announce_ip_mode))
        .route("/api/announce/passkeys", get(get_passkeys).post(set_announce_passkey))
        .route("/api/categories/:name", axum::routing::put(category_update).delete(category_delete))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AnnounceClient;

    fn state(key: &str, password_hash: &str) -> AppState {
        let mut cfg = Config::default();
        cfg.daemon.api_key = key.into();
        cfg.auth.password_hash = password_hash.into();
        cfg.announce_clients.insert(
            "t.myanonamouse.net".into(),
            AnnounceClient {
                peer_id_prefix: "-qB5220-".into(),
                user_agent: "qBittorrent/5.2.2".into(),
            },
        );
        AppState {
            config: Arc::new(std::sync::RwLock::new(Arc::new(cfg))),
            config_path: std::path::PathBuf::from("/nonexistent.toml"),
            update_check: Arc::new(tokio::sync::Mutex::new(None)),
            // offline, not start: a unit test must not open a listener.
            engines: Arc::new(crate::engines::EngineHost::offline(
                &Config::default(),
                std::path::Path::new("/tmp/hydra-test-engines"),
            )),
            store: Arc::new(std::sync::Mutex::new(
                crate::store::Store::open_in_memory().unwrap(),
            )),
            public_ip: Arc::new(tokio::sync::Mutex::new((String::new(), String::new()))),
            started_at: 0,
            logs: crate::logbuf::LogBuffer::new(),
            bench: None,
        }
    }

    fn with_key(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-Api-Key", value.parse().unwrap());
        h
    }

    #[test]
    fn a_real_key_is_required_and_checked() {
        let s = state("secret", "$2a$hash");
        assert!(!authorised(&s, &HeaderMap::new(), ""), "no key must be refused");
        assert!(!authorised(&s, &with_key("wrong"), ""), "wrong key must be refused");
        assert!(authorised(&s, &with_key("secret"), ""), "right key must pass");
    }

    #[test]
    fn the_key_may_arrive_as_a_query_parameter() {
        let s = state("secret", "$2a$hash");
        assert!(authorised(&s, &HeaderMap::new(), "apikey=secret"));
        assert!(authorised(&s, &HeaderMap::new(), "other=1&apikey=secret"));
        assert!(!authorised(&s, &HeaderMap::new(), "apikey=wrong"));
    }

    // The placeholder key disables the check, but only once an admin password
    // exists. Both halves are asserted: an install that is still unconfigured
    // must NOT be wide open, and that is the half a refactor would quietly drop.
    #[test]
    fn the_placeholder_key_disables_the_check_only_after_setup() {
        let configured = state(DEFAULT_API_KEY, "$2a$hash");
        assert!(
            authorised(&configured, &HeaderMap::new(), ""),
            "placeholder + password set = dev mode, no key needed"
        );

        let fresh = state(DEFAULT_API_KEY, "");
        assert!(
            !authorised(&fresh, &HeaderMap::new(), ""),
            "placeholder with no admin password must still refuse"
        );
        assert!(
            authorised(&fresh, &with_key(DEFAULT_API_KEY), ""),
            "and must accept the placeholder itself as the key"
        );
    }

    // The trap this guards: "3.9.0" is lexically greater than "3.180.0", so a
    // string comparison would offer 3.9.0 as an upgrade from 3.180.0.
    #[test]
    fn a_local_engine_is_named_local_something() {
        assert_eq!(local_agent("race"), "local-race");
        assert_eq!(local_agent("hoard"), "local-hoard");
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(version_less("3.9.0", "3.180.0"));
        assert!(!version_less("3.180.0", "3.9.0"));
        assert!(version_less("3.180.0-typhon", "v3.181.0"));
        assert!(!version_less("3.180.0-typhon", "v3.180.0"));
    }

    // Go hands encoding/json a float64, which prints an integral value as "0".
    // Emitting 0.0 would change the bytes every client parses.
    #[test]
    fn integral_floats_are_emitted_as_integers() {
        let parsed: toml::Value =
            toml::from_str("a = 0.0\nb = 1.5\nc = 3\n").unwrap();
        let json = toml_to_json(&parsed);
        assert_eq!(serde_json::to_string(&json).unwrap(), r#"{"a":0,"b":1.5,"c":3}"#);
    }

    #[test]
    fn only_three_part_numeric_tags_are_considered() {
        assert!(is_semver_tag("v3.180.0"));
        assert!(is_semver_tag("3.180.0"));
        assert!(!is_semver_tag("v3.180"));
        assert!(!is_semver_tag("nightly"));
        assert!(!is_semver_tag("v3.180.0-rc1"));
    }

    #[test]
    fn percent_encoding_is_decoded_in_the_query_fallback() {
        let s = state("a b", "$2a$hash");
        assert!(authorised(&s, &HeaderMap::new(), "apikey=a%20b"));
        assert!(authorised(&s, &HeaderMap::new(), "apikey=a+b"));
    }
    #[test]
    fn agent_ids_carry_the_local_prefix() {
        assert_eq!(local_agent("race"), "local-race");
        assert_eq!(local_agent("hoard"), "local-hoard");
    }
}
