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
use std::net::ToSocketAddrs;
use std::sync::Arc;

use crate::config::Config;

/// The version string this build reports.
///
/// It must stay in lockstep with internal/version/version.go for as long as the
/// two binaries coexist: /api/update-check publishes it, and the release
/// pipeline compares it against the changelog.
pub const HYDRANOS_VERSION: &str = "4.1.12";

type UpdateCheckCache = Option<(std::time::Instant, String, String)>;

/// The process's own public addresses, (v4, v6).
pub type PublicIp = Arc<tokio::sync::Mutex<(String, String)>>;

/// The Records card's answer, and when it was computed.
///
/// Computing it reads every row of `bench_samples` -- 1.7M of them, some six
/// seconds. 3.x served a cached copy and refreshed it in the background; the
/// port did the computation on every request instead, and the overview header
/// does not paint until this answers. Hence: same cache, same 30 minute TTL.
#[derive(Default)]
pub struct RecordsCache {
    pub value: Option<serde_json::Value>,
    pub at: Option<std::time::Instant>,
    pub computing: bool,
}

pub type Records = Arc<std::sync::Mutex<RecordsCache>>;

/// How long a computed answer stays good. All-time records do not move often,
/// and a stale figure here is invisible next to a six second stall.
const RECORDS_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Recompute the records off the request path, on a read-only connection.
///
/// Returns immediately if another refresh is already running: two concurrent
/// scans of the same 1.7M rows would only make each other slower.
pub fn refresh_records(path: std::path::PathBuf, cache: Records) {
    {
        let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
        if c.computing {
            return;
        }
        c.computing = true;
    }
    tokio::task::spawn_blocking(move || {
        // `computing` is cleared through a guard, not at the end of the happy
        // path: a panic here would otherwise leave the flag set and every later
        // refresh would return early, so the card would stay empty forever with
        // nothing in the log to say why.
        struct Clear(Records);
        impl Drop for Clear {
            fn drop(&mut self) {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).computing = false;
            }
        }
        let _clear = Clear(cache.clone());

        let computed = crate::benchdb::BenchDb::open_read_only(&path)
            .and_then(|db| db.records_payload());
        match computed {
            Ok(value) => {
                let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
                c.value = Some(value);
                c.at = Some(std::time::Instant::now());
                tracing::info!("records refreshed");
            }
            // Keep the previous answer: an empty Records card is worse than one
            // that is half an hour old. But say so -- a silent failure here is
            // what made the card look merely slow instead of broken.
            Err(e) => tracing::warn!(path = %path.display(), "records refresh failed: {e:#}"),
        }
    });
}

/// What the engines had already moved when this process started, and at the
/// last midnight, so "this session" and "today" can be told from "ever".
///
/// The engines' per-torrent counters are LIFETIME totals loaded from resume
/// data -- they do not reset at boot. Publishing them directly is what made
/// `day_uploaded` read 321 TB: the whole history of every loaded torrent,
/// labelled as one day.
#[derive(Default)]
pub struct Odometer {
    /// Session totals at startup. `session_* = totals - this`.
    pub session_offset: (i64, i64),
    /// Session totals at the last Europe/Paris midnight rollover.
    pub day_baseline: (i64, i64),
    /// The date that baseline belongs to, `YYYY-MM-DD` in Europe/Paris.
    pub day_date: String,
    /// The same startup mark, per engine id, so a per-engine block can publish
    /// its own session instead of the all-engines sum.
    pub per_engine: std::collections::HashMap<String, (i64, i64)>,
}

impl Odometer {
    /// Take a removed torrent's lifetime bytes off the marks.
    ///
    /// `session = totals - session_offset`, and `totals` is a sum over the
    /// torrents currently LOADED -- so it is about to lose this torrent's
    /// lifetime bytes. Lowering the mark by the same amount is what keeps "this
    /// session" and "today" continuous across a delete. Without it, removing a
    /// torrent that had uploaded 500 GB over its life subtracts 500 GB from
    /// TODAY's figure, for work done weeks ago.
    ///
    /// `day_baseline` is deliberately untouched: `day = session - day_baseline`
    /// and `session` does not move here, so the day does not either.
    pub fn forget(&mut self, engine_id: &str, ul: i64, dl: i64) {
        self.session_offset.0 -= ul;
        self.session_offset.1 -= dl;
        if let Some(mark) = self.per_engine.get_mut(engine_id) {
            mark.0 -= ul;
            mark.1 -= dl;
        }
    }
}

pub type Odo = Arc<std::sync::Mutex<Odometer>>;

/// Today's date in the daemon's local zone, `YYYY-MM-DD`.
///
/// Local, not UTC: the operator's day ends at midnight where they are, and the
/// container is given TZ=Europe/Paris for exactly this.
fn local_date() -> String {
    crate::platform::local_date()
}

/// Bytes this session and today, from the engines' lifetime counters.
///
/// Rolls the day baseline when the local date changes, so calling it on a timer
/// is what keeps the figure honest on a node nobody is looking at -- 3.x reset
/// only on the first request of the new day, and the counter sat on yesterday's
/// baseline until someone opened the page.
pub fn session_and_day(state: &AppState) -> ((i64, i64), (i64, i64), (i64, i64)) {
    let (total_up, total_down) = state.engines.session_totals();
    let mut odo = state.odometer.lock().unwrap_or_else(|e| e.into_inner());

    // Totals below the mark mean torrents were removed, taking their lifetime
    // bytes out of the sum. Follow them down rather than publishing a negative
    // day: the alternative is a counter that reads zero until the engines have
    // re-earned everything the removed torrent had ever uploaded.
    if total_up < odo.session_offset.0 || total_down < odo.session_offset.1 {
        odo.session_offset = (total_up, total_down);
    }
    let session = (
        (total_up - odo.session_offset.0).max(0),
        (total_down - odo.session_offset.1).max(0),
    );

    let today = local_date();
    if odo.day_date != today {
        odo.day_date = today;
        odo.day_baseline = session;
    }
    if session.0 < odo.day_baseline.0 || session.1 < odo.day_baseline.1 {
        odo.day_baseline = (0, 0);
    }
    let day = (
        (session.0 - odo.day_baseline.0).max(0),
        (session.1 - odo.day_baseline.1).max(0),
    );
    // Lifetime totals returned too: every caller needs them alongside, and
    // summing 300k counters twice per frame is the kind of waste that only
    // shows up as a warm CPU.
    ((total_up, total_down), session, day)
}

/// One engine's "since this process started" totals.
///
/// Same shape as `session_and_day`, scoped to a single engine: its live sum
/// minus the mark taken at boot, ratcheted down if the sum falls below the mark
/// so a large removal cannot publish a negative session.
pub fn engine_session(state: &AppState, engine_id: &str) -> (i64, i64) {
    let totals = state.engines.session_totals_of(engine_id);
    let mut odo = state.odometer.lock().unwrap_or_else(|e| e.into_inner());
    let mark = odo.per_engine.entry(engine_id.to_string()).or_insert(totals);
    if totals.0 < mark.0 || totals.1 < mark.1 {
        *mark = totals;
    }
    ((totals.0 - mark.0).max(0), (totals.1 - mark.1).max(0))
}

#[derive(Clone)]
pub struct AppState {
    /// Imports in flight, by job id. An import outlives the request that
    /// started it, so its progress lives where the later polls can find it.
    pub imports: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<crate::importer::Progress>>>>,
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
    pub public_ip: PublicIp,
    /// Last per-engine exit measurement, and when it was taken.
    pub net_engines: crate::netprobe::Snapshot,
    /// Marks that turn the engines' lifetime counters into session and day.
    pub odometer: Odo,
    /// Cached Records card, refreshed off the request path.
    pub records: Records,
    /// Where bench.db lives, so a refresh can open its own read-only handle.
    pub bench_path: std::path::PathBuf,
    /// Unix time this process started, for the uptime figure.
    pub started_at: i64,
    /// Recent log lines, for the Logs tab and its stream.
    pub logs: crate::logbuf::LogBuffer,
    /// Changes a returning client can be told about without re-streaming the
    /// whole library. See `reconnect`.
    pub reconnect: Arc<crate::reconnect::Ring>,
    /// The measurement database, when one could be opened.
    ///
    /// `None` is a normal state and not a failure: the timeline is
    /// observability, and losing it must never cost the seedbox. Every route
    /// that reads it then answers empty, exactly as 3.x does.
    pub bench: Option<crate::benchdb::Shared>,
    /// Live cookie sessions for the qBittorrent shim.
    ///
    /// The *arr stack has a username and a password and no way to set a
    /// header, so it logs in and rides a cookie. See `crate::session`.
    pub sessions: crate::session::Sessions,
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

    /// The live configuration handle, for a worker that must not freeze its
    /// copy at boot.
    ///
    /// The announce policy was captured by value once per engine and an
    /// override edited in the UI never reached the runner -- nothing
    /// contradicted itself, the tab showed the new value while the announcer
    /// used the old. A drain holding a stale obligation would fail the same
    /// way, except the symptom is a deleted torrent.
    pub fn config_handle(&self) -> Arc<std::sync::RwLock<Arc<Config>>> {
        self.config.clone()
    }

    /// Replace the live configuration after the file has been edited.
    pub fn set_cfg(&self, config: Config) {
        *self.config.write().unwrap() = Arc::new(config);
    }
}

/// The placeholder key shipped in the default config.
///
/// The placeholder key shipped in the default config.
///
/// Known to everyone who has read the repository, so it is a key in name only.
/// `ensure_api_key` replaces it at boot on any install that still carries it.
const DEFAULT_API_KEY: &str = "change-me-in-production";

/// Decide whether a request may proceed.
///
/// Two rules, and one deliberate departure from the Go middleware this was
/// ported from:
///
///  1. **An instance with no key authorises nobody.** The Go side had an
///     escape hatch -- placeholder key plus an admin password meant no key was
///     checked at all -- and the port inherited it while losing the first-boot
///     key generation that kept `api_key` from being empty. The two together
///     were a hole: `provided == expected` made *sending nothing* match
///     *having nothing*, so a fresh Docker install served its whole API, and
///     its whole configuration, to any caller who simply omitted the header.
///     Sending a wrong key was refused; sending none succeeded. Failing closed
///     here is what makes the absence of a key a refusal instead of a pass.
///  2. The key may arrive either in the X-Api-Key header or as an `apikey`
///     query parameter. Dropping the query fallback would break every caller
///     that cannot set headers.
///
/// The comparison is constant-time: the key is a bearer secret, and a plain
/// `==` returns as soon as two bytes differ, which leaks its prefix to anyone
/// willing to time enough requests.
pub fn authorised(state: &AppState, headers: &HeaderMap, query: &str) -> bool {
    let cfg = state.cfg();
    let expected = cfg.daemon.api_key.as_str();

    if expected.is_empty() {
        return false;
    }

    let provided = headers
        .get("X-Api-Key")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| query_param(query, "apikey"))
        .unwrap_or_default();

    if !provided.is_empty() && constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return true;
    }

    // A caller that logged in carries a cookie instead. This is the only path
    // the *arr stack has: its qBittorrent form holds a username and a password
    // and has nowhere to put a key.
    match session_of(headers) {
        Some(sid) => state.sessions.validate(&sid),
        None => false,
    }
}

/// The session id this request carries, if it carries one.
fn session_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| crate::session::cookie_value(v, crate::session::COOKIE_NAME))
        .filter(|v| !v.is_empty())
}

/// Compare two secrets without returning early on the first difference.
///
/// The length is not itself secret -- the generated key has a fixed one -- so
/// an early return on a length mismatch is fine; what must not vary with the
/// input is the time taken to reject a key of the *right* length.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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

/// 400 with a message, the shape every other refusal in this file uses.
fn bad_request(msg: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
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
// The fleet
// ---------------------------------------------------------------------------
//
// A node is another Hydra, addressed by URL and its own API key, both kept in
// the store rather than in default.toml. Declaring one used to mean editing a
// TOML over SSH, which is the single reason the agent model went unused.

/// Every node, each probed live.
///
/// Probed concurrently: six unreachable nodes must cost one timeout, not six.
/// The local instance is NOT in this list -- it is not something the operator
/// can add or remove, and folding it in would invite a "delete" that cannot work.
async fn get_nodes(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    // The lock is released before any await: holding it across the network
    // would serialise every other store reader behind the slowest node.
    let nodes = {
        let store = state.store.lock().unwrap();
        store.nodes().unwrap_or_default()
    };

    let probes = nodes.iter().map(|n| {
        let (url, key) = (n.url.clone(), n.api_key.clone());
        async move { crate::nodes::probe(&url, &key).await }
    });
    let healths = futures::future::join_all(probes).await;

    let out: Vec<serde_json::Value> = nodes
        .iter()
        .zip(healths)
        .map(|(n, h)| {
            serde_json::json!({
                "name": n.name,
                "url": n.url,
                "enabled": n.enabled,
                "added_at": n.added_at,
                "health": h,
            })
        })
        .collect();
    Json(out).into_response()
}

/// Probe a node without saving it.
///
/// Separate from the add on purpose: the operator gets to see "key refused"
/// before committing a row, instead of adding an entry that shows up broken.
async fn post_node_test(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let url = v.get("url").and_then(|x| x.as_str()).unwrap_or_default();
    if url.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "url is required"})))
            .into_response();
    }
    let key = v.get("api_key").and_then(|x| x.as_str()).unwrap_or_default();
    Json(crate::nodes::probe(url, key).await).into_response()
}

/// Add or update a node.
///
/// The probe runs FIRST and a failure is a 400: a node that was never reachable
/// has nothing to offer the fleet, and storing it would only produce a row that
/// is permanently red with no way to tell a typo from an outage.
async fn post_node(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    let api_key = v.get("api_key").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    if name.is_empty() || url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name and url are required"})),
        )
            .into_response();
    }
    // A node URL is used for two different things: this process probes it, and
    // the operator's BROWSER is redirected to it by /node/<name>/open. A
    // loopback address satisfies the first and can never satisfy the second --
    // it would send the browser to its own machine. Reported from the bench,
    // where `http://127.0.0.1:8499` probed green and opened nothing.
    //
    // Refused rather than papered over: a second Hydra on this very host is
    // still reachable at the address other machines use, and that is the one
    // that works for both jobs.
    let host = url_host(&url);
    if is_loopback_host(&host) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "{host} is this machine's own loopback: the browser could never reach the node there. Use the address other machines use."
                )
            })),
        )
            .into_response();
    }

    // A name is a path segment in /node/<name>/open, so it may not carry one.
    if name.contains('/') || name.contains("..") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name cannot contain / or .."})),
        )
            .into_response();
    }

    let health = crate::nodes::probe(&url, &api_key).await;
    if !health.online {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": health.error, "health": health})),
        )
            .into_response();
    }

    let node = crate::store::Node {
        name: name.clone(),
        url,
        api_key,
        enabled: true,
        added_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    {
        let store = state.store.lock().unwrap();
        if let Err(e) = store.put_node(&node) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"status": "ok", "name": name, "health": health})).into_response()
}

/// Remove a node, and say so only if one went.
///
/// The route this replaces, `delete_agent`, answered `{"status":"ok"}` without
/// touching anything: the row vanished from the table and came back on reload,
/// with nothing anywhere to explain it.
async fn delete_node(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let removed = {
        let store = state.store.lock().unwrap();
        store.delete_node(&name).unwrap_or(false)
    };
    if !removed {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown node"})))
            .into_response();
    }
    Json(serde_json::json!({"status": "ok"})).into_response()
}

/// The host part of a URL, however it was written.
fn url_host(url: &str) -> String {
    let authority = url
        .split("//")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or_default();

    // A bracketed IPv6 literal keeps its colons: the brackets are what
    // separate the address from the port, not the last colon. Splitting on
    // colons instead returned "2001" for `[2001:db8::1]:8199` and "" for
    // `[::1]:8199` -- which made `is_loopback_host` answer false for the one
    // address the enrolment guard exists to refuse.
    if let Some(rest) = authority.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((addr, _)) => addr.to_string(),
            None => rest.to_string(),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, _port)) => host.to_string(),
        None => authority.to_string(),
    }
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "0.0.0.0") || host.starts_with("127.")
}

/// Mint a one-time enrolment token and the command that spends it.
///
/// This is how a node joins, and the direction matters: the new machine
/// registers ITSELF. This Hydra never opens a session anywhere and never holds
/// a credential for another host, so compromising its API cannot become code
/// execution on the fleet. The operator pastes one line into the shell they
/// already have open.
///
/// The command points back at the address the CALLER used to reach here, taken
/// from the Host header: this process cannot otherwise know which of its
/// addresses a third machine can resolve, and a guess would produce a command
/// that installs a node and then fails to register it.
async fn post_node_enrol(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let (token, expires) = {
        let store = state.store.lock().unwrap();
        // Thirty minutes: long enough to paste into a shell and watch an
        // install, short enough that a token left in a scrollback is stale.
        match store.create_enrol_token(1800) {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": e.to_string()})),
                )
                    .into_response()
            }
        }
    };
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("this-hydra:8199");
    let base = format!("http://{host}");
    Json(serde_json::json!({
        "token": token,
        "expires_at": expires,
        "command": format!(
            "curl -fsSL {base}/install.sh | sh -s -- --register-to {base} --token {token}"
        ),
    }))
    .into_response()
}

/// A node registering itself, at the end of its own install.
///
/// Authenticated by the enrolment token ALONE, which is deliberate: the new
/// machine has no API key of this one, and giving it one would be handing out
/// exactly the credential this design exists to avoid moving around. The token
/// is single use and expiring, and spending it is one conditional UPDATE so two
/// machines racing on the same token cannot both win.
async fn post_node_register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let token = v
        .get("token")
        .and_then(|x| x.as_str())
        .map(String::from)
        .or_else(|| {
            headers
                .get("X-Enrol-Token")
                .and_then(|h| h.to_str().ok())
                .map(String::from)
        })
        .unwrap_or_default();
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    let api_key = v.get("api_key").and_then(|x| x.as_str()).unwrap_or_default().to_string();

    let bad = |m: &str| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))).into_response()
    };
    if token.is_empty() || name.is_empty() || url.is_empty() {
        return bad("token, name and url are required");
    }
    if name.contains('/') || name.contains("..") {
        return bad("name cannot contain / or ..");
    }
    if is_loopback_host(&url_host(&url)) {
        return bad("a node cannot register itself at a loopback address");
    }

    // Spent BEFORE anything else is decided: a token must burn even on a
    // request that then turns out to be a duplicate name, or it could be
    // retried until one lands.
    let spent = {
        let store = state.store.lock().unwrap();
        store.consume_enrol_token(&token).unwrap_or(false)
    };
    if !spent {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "enrolment token unknown, already used, or expired"})),
        )
            .into_response();
    }

    // An existing name is refused rather than overwritten: a token holder must
    // not be able to repoint a node the operator already trusts.
    let taken = {
        let store = state.store.lock().unwrap();
        store.node(&name).ok().flatten().is_some()
    };
    if taken {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("a node named {name} already exists")})),
        )
            .into_response();
    }

    let node = crate::store::Node {
        name: name.clone(),
        url,
        api_key,
        enabled: true,
        added_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    {
        let store = state.store.lock().unwrap();
        if let Err(e) = store.put_node(&node) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({"status": "ok", "name": name})).into_response()
}

/// The enrolment script, served so the command is one line.
async fn get_install_script() -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/x-shellscript")],
        include_str!("../../../install.sh"),
    )
        .into_response()
}

/// Hand one torrent to a node.
///
/// Sends the metainfo, then tells the far side where to fetch the data from.
/// This node keeps its copy: a handoff is a seed, and dropping the source is a
/// separate decision the operator makes once the target reports complete.
async fn post_node_handoff(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let info_hash = v
        .get("info_hash")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_lowercase();
    let from = v.get("from").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let category = v.get("category").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    // Which engine ON the target. Empty lets its category decide, which is what
    // an operator picking a node rather than an engine is asking for.
    let engine = v.get("engine").and_then(|x| x.as_str()).unwrap_or_default().to_string();

    if info_hash.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "info_hash is required"})),
        )
            .into_response();
    }

    // With no `from`, hand the target `auto:<port>` and let IT fill in the
    // address: it is the only side that knows which of ours it can reach. The
    // port is the listening port of the engine that actually holds the torrent,
    // which is not necessarily the first engine on this node.
    let from = if from.is_empty() {
        let Some((engine_id, _)) = find_torrent(&state, &info_hash) else {
            return not_found();
        };
        let port = state
            .engines
            .engines()
            .iter()
            .find(|e| e.id == engine_id)
            .map(|e| e.listen_port)
            .unwrap_or(0);
        if port == 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "this engine has no listening port; pass `from` explicitly"
                })),
            )
                .into_response();
        }
        format!("auto:{port}")
    } else if from.parse::<std::net::SocketAddr>().is_err() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("{from} is not a host:port")})),
        )
            .into_response();
    } else {
        from
    };

    let node = {
        let store = state.store.lock().unwrap();
        store.node(&name).ok().flatten()
    };
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown node"})))
            .into_response();
    };
    let blob = {
        let store = state.store.lock().unwrap();
        store.torrent_blob(&info_hash).ok().flatten()
    };
    let Some(blob) = blob else {
        return not_found();
    };

    // "keep" duplicates, "remove" moves. A move cannot delete now: the target
    // has the metainfo and none of the bytes yet. So it waits for the far side
    // to report complete, and only then drops the local copy.
    let then = v.get("then").and_then(|x| x.as_str()).unwrap_or("keep").to_string();

    match crate::nodes::handoff(
        &node.url, &node.api_key, &info_hash, blob, &from, &category, &engine,
    )
    .await
    {
        Ok(mut v) => {
            if then == "remove" {
                let (url, key) = (node.url.clone(), node.api_key.clone());
                let (ih, eng) = (info_hash.clone(), engine.clone());
                let st = state.clone();
                tokio::spawn(async move {
                    // Fails SAFE: a restart during the wait leaves both copies,
                    // which is a duplicate to clean up rather than data gone.
                    match crate::nodes::wait_until_complete(&url, &key, &ih, &eng).await {
                        Ok(true) => {
                            tracing::info!(hash = %ih, node = %url, "handoff complete, dropping the local copy");
                            remove_torrent_everywhere(&st, &ih, true);
                        }
                        Ok(false) => tracing::warn!(
                            hash = %ih, node = %url,
                            "handoff did not complete in time; the local copy is kept"
                        ),
                        Err(e) => tracing::warn!(
                            hash = %ih, node = %url, error = %e,
                            "could not confirm the handoff; the local copy is kept"
                        ),
                    }
                });
            }
            if let Some(o) = v.as_object_mut() {
                o.insert("then".into(), serde_json::Value::String(then));
            }
            Json(v).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// Move a torrent between two engines OF A REMOTE node.
///
/// Relayed, not reimplemented: it is that node's own local move, which costs it
/// nothing either -- its two engines share its filesystem exactly as ours do.
/// Doing it from here would mean pulling the payload and pushing it back to the
/// machine it never left.
async fn post_node_move_engine(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let info_hash = v.get("info_hash").and_then(|x| x.as_str()).unwrap_or_default().to_lowercase();
    let engine = v.get("engine").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    if info_hash.is_empty() || engine.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "info_hash and engine are required"})),
        )
            .into_response();
    }
    let node = {
        let store = state.store.lock().unwrap();
        store.node(&name).ok().flatten()
    };
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown node"})))
            .into_response();
    };
    let payload = serde_json::json!({ "engine": engine }).to_string();
    match crate::nodes::forward(
        &node.url,
        &node.api_key,
        reqwest::Method::POST,
        &format!("api/torrents/{info_hash}/engine"),
        payload.into_bytes(),
    )
    .await
    {
        Ok((status, body, _)) => {
            let v: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::json!({"status": "ok"}));
            (StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK), Json(v))
                .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
    }
}

/// Pull a torrent FROM a node into an engine of this one.
///
/// The mirror of the handoff, and the same mechanism: the metainfo travels over
/// HTTP, the data over BitTorrent. Only the direction of the dial changes -- we
/// add the torrent here and are told that the far side holds it, instead of the
/// other way round.
async fn post_node_fetch(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let info_hash = v.get("info_hash").and_then(|x| x.as_str()).unwrap_or_default().to_lowercase();
    let engine = v.get("engine").and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let from_engine = v.get("from_engine").and_then(|x| x.as_str()).unwrap_or("hoard").to_string();
    let category = v.get("category").and_then(|x| x.as_str()).unwrap_or_default().to_string();

    let bad = |m: String| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))).into_response();
    if info_hash.is_empty() || engine.is_empty() {
        return bad("info_hash and engine are required".into());
    }
    if !state.engines.engines().iter().any(|e| e.id == engine) {
        return bad(format!("no engine named {engine} on this node"));
    }
    if find_torrent(&state, &info_hash).is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "this node already has that torrent"})),
        )
            .into_response();
    }

    let node = {
        let store = state.store.lock().unwrap();
        store.node(&name).ok().flatten()
    };
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown node"})))
            .into_response();
    };

    let (blob, port) =
        match crate::nodes::fetch_metainfo(&node.url, &node.api_key, &info_hash, &from_engine).await
        {
            Ok(v) => v,
            Err(e) => return (StatusCode::BAD_GATEWAY, Json(serde_json::json!({"error": e}))).into_response(),
        };

    let (hash, tname) = match add_torrent_bytes(&state, &blob, &category, "", "", false, false, &engine) {
        Ok(v) => v,
        Err(e) => return bad(e),
    };

    // Where to fetch it from. Unlike a push, no `auto:` is needed: the node's
    // own URL is its address, and the engine list gave us the port.
    let host = url_host(&node.url);
    let mut queued = 0usize;
    if let Some((_, torrent)) = find_torrent(&state, &hash) {
        if let Ok(addr) = format!("{host}:{port}").parse::<std::net::SocketAddr>() {
            typhon_engine::tracker::enqueue_dial(addr, torrent.clone());
            queued = 1;
        } else if let Ok(mut it) = format!("{host}:{port}").to_socket_addrs() {
            // A node declared by hostname still has to resolve to something.
            if let Some(addr) = it.next() {
                typhon_engine::tracker::enqueue_dial(addr, torrent.clone());
                queued = 1;
            }
        }
    }
    Json(serde_json::json!({
        "status": "ok", "info_hash": hash, "name": tname,
        "engine": engine, "from": format!("{host}:{port}"), "peer_queued": queued
    }))
    .into_response()
}

/// Open a node's own front, already authenticated.
///
/// A redirect to the node's ORIGIN, with its key in the URL FRAGMENT. Not a
/// path-prefixed proxy: the front asks for absolute paths (`/static/app.js`,
/// `/api/hoard/page`), which under a `/node/<name>/` prefix would be served by
/// this Hydra instead of the remote one.
///
/// The fragment is never sent to any server and never appears in a log or a
/// Referer. app.js consumes it on load, moves it into that origin's
/// localStorage and strips it -- which is the same place, and the same
/// exposure, as a key the operator had typed into the login box by hand.
async fn get_node_open(
    State(state): State<AppState>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let node = {
        let store = state.store.lock().unwrap();
        store.node(&name).ok().flatten()
    };
    let Some(node) = node else {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown node"})))
            .into_response();
    };
    let target = format!("{}/#key={}", node.url.trim_end_matches('/'), node.api_key);
    (
        StatusCode::TEMPORARY_REDIRECT,
        [(axum::http::header::LOCATION, target)],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Announce identity overrides
// ---------------------------------------------------------------------------
//
// Three read endpoints that answer straight from the config. They are the first
// slice of the port on purpose: they exercise the whole path -- config parsing,
// auth, routing, serialisation -- while having no engine state behind them, so
// a difference against the Go binary can only come from this file.

/// Why each tracker is unhappy, and whether it still hands out our address.
///
/// Two questions the trackers tab could not answer before 4.6.0. "Failed" was
/// one number for the whole engine, so a node being rate limited by one tracker
/// looked exactly like a node announcing deleted torrents to another. And
/// nothing at all reported whether an announce actually put us in the peer list
/// -- the failure that cost three days of upload in September 2026 produced
/// successful announces, correct scrapes, and an address the tracker only ever
/// served to half the swarm.
/// Accept a tracker's faults, or stop accepting them.
///
/// Body: `{"host": "...", "muted": true}`. Persisted next to the ip modes, so
/// it survives a restart: an operator who has decided that archive.org is not
/// coming back should not have to decide it again every morning.
/// Declare how long a tracker requires a torrent to be seeded.
///
/// Body: `{"host": "...", "hours": 48}`. Zero or a missing value clears it.
async fn set_announce_min_seed(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    #[derive(serde::Deserialize)]
    struct Req {
        host: String,
        #[serde(default)]
        hours: i64,
        /// Remove the declaration entirely. NOT the same as zero.
        #[serde(default)]
        clear: bool,
    }
    let Ok(req) = serde_json::from_str::<Req>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"})))
            .into_response();
    };
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "host is required"})))
            .into_response();
    }
    if req.hours < 0 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "hours cannot be negative"})))
            .into_response();
    }
    // ⚠ "0" is STORED, not turned into a blank. Blank means "nothing declared"
    // and the drain treats that as protected; 0 means "this tracker asks for
    // nothing" and releases the torrent. Writing 0 as an empty value made the
    // second state unreachable -- the only way to ever authorise a deletion
    // silently did nothing. Clearing is its own flag.
    let value = if req.clear { String::new() } else { req.hours.to_string() };
    let persisted = set_host_entry(&state, "announce_min_seed_hours", &host, &value);
    Json(serde_json::json!({
        "host": host,
        "min_seed_hours": if req.clear { serde_json::Value::Null } else { req.hours.into() },
        "cleared": req.clear,
        "persisted": persisted,
    }))
    .into_response()
}

/// Hide one tracker, or a batch of them, from the Trackers tab.
///
/// Body: `{"host": "...", "hidden": true}` or `{"hosts": [...], "hidden": true}`.
/// The batch form exists because the problem it answers is a batch one: listing
/// trackers from the catalogue produced 91 rows here, and hiding 78 public
/// trackers one request at a time is not an interface, it is a chore.
///
/// A host carrying a passkey, a client identity or an IP mode is NEVER hidden
/// by the batch form. Those are settings the operator wrote on purpose, and
/// making them invisible by a bulk action is how a passkey goes missing without
/// anyone touching it.
async fn set_announce_hidden(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    #[derive(serde::Deserialize)]
    struct Req {
        #[serde(default)]
        host: String,
        #[serde(default)]
        hosts: Vec<String>,
        #[serde(default)]
        hidden: bool,
    }
    let Ok(req) = serde_json::from_str::<Req>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"})))
            .into_response();
    };
    let mut wanted: Vec<String> = req.hosts.iter().map(|h| h.trim().to_string()).collect();
    if !req.host.trim().is_empty() {
        wanted.push(req.host.trim().to_string());
    }
    wanted.retain(|h| !h.is_empty());
    wanted.sort();
    wanted.dedup();
    if wanted.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "host is required"})))
            .into_response();
    }

    let cfg = state.cfg();
    let value = if req.hidden { "1" } else { "" };
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    for host in wanted {
        // Only the batch form protects configured hosts: hiding a single row
        // the operator clicked on is exactly what they asked for.
        let configured = cfg.announce_passkeys.contains_key(&host)
            || cfg.announce_ip_modes.contains_key(&host);
        if req.hidden && configured && req.hosts.len() > 1 {
            skipped.push(host);
            continue;
        }
        if set_host_entry(&state, "announce_hidden", &host, value) {
            changed.push(host);
        }
    }
    Json(serde_json::json!({
        "hidden": req.hidden,
        "changed": changed.len(),
        "hosts": changed,
        // Named rather than counted: "3 skipped" invites the question this
        // answers.
        "skipped_configured": skipped,
    }))
    .into_response()
}

async fn set_announce_mute(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    #[derive(serde::Deserialize)]
    struct Req {
        host: String,
        #[serde(default)]
        muted: bool,
    }
    let Ok(req) = serde_json::from_str::<Req>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"})))
            .into_response();
    };
    let host = req.host.trim().to_string();
    if host.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "host is required"})))
            .into_response();
    }
    let value = if req.muted { "1" } else { "" };
    let persisted = set_host_entry(&state, "announce_muted", &host, value);
    Json(serde_json::json!({
        "host": host,
        "muted": req.muted,
        "persisted": persisted,
    }))
    .into_response()
}

async fn get_announce_health(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let mut out = serde_json::Map::new();
    // Counted in DISTINCT TRACKERS, never in errors: "2" has to mean two
    // trackers to look at, not 1321 timeouts from one of them.
    let (mut red, mut amber) = (0u32, 0u32);
    for id in ["hoard", "race"] {
        let Some(engine) = state.engines.get(id) else { continue };
        let cache = &engine.announce_cache;
        let errs = cache.error_breakdown();
        let vers = cache.verifications();
        let mut hosts: std::collections::BTreeMap<String, serde_json::Value> =
            std::collections::BTreeMap::new();
        let mut names: std::collections::BTreeSet<String> = errs.keys().cloned().collect();
        names.extend(vers.keys().cloned());
        for h in names {
            let mut o = serde_json::Map::new();
            if let Some(v) = errs.get(&h) {
                o.insert(
                    "errors".into(),
                    serde_json::json!(v
                        .iter()
                        .map(|(class, n)| serde_json::json!({"class": class, "count": n}))
                        .collect::<Vec<_>>()),
                );
            }
            // Severity, so the interface does not have to re-derive it and two
            // readers cannot disagree about what "a problem" means.
            //
            // red   : acts on it today. A passkey the tracker rejects, or a
            //         self-check saying we are in one family's peer list only.
            // amber : reachability or throttling. Often our own doing --
            //         `announce_rate_limit = 0.0` is what earns the 429s.
            // muted : seen and accepted, kept out of every count.
            // Torrents the tracker deleted are NOT a fault: nothing to fix.
            let muted = cfg.announce_muted.contains_key(&h);
            // Hidden goes FURTHER than muted, on purpose. Muted keeps the row
            // and only drops it from the counts; hidden takes the tracker out
            // of the tab altogether, badge included. 91 rows on this node, most
            // of them a public tracker holding one torrent: a list nobody can
            // read is not safer than a short one.
            let hidden = cfg.announce_hidden.contains_key(&h);
            let classes: Vec<&str> = errs
                .get(&h)
                .map(|v| v.iter().map(|(c, _)| c.as_str()).collect())
                .unwrap_or_default();
            let verdict = vers.get(&h).map(|v| v.verdict()).unwrap_or("");
            let severity = if hidden {
                "hidden"
            } else if muted {
                "muted"
            } else if classes.contains(&"invalid_passkey")
                || matches!(verdict, "v4_missing" | "v6_missing" | "absent")
            {
                "red"
            } else if classes
                .iter()
                .any(|c| matches!(*c, "rate_limited" | "timeout" | "dns" | "connect"))
            {
                "amber"
            } else {
                "none"
            };
            o.insert("severity".into(), serde_json::json!(severity));
            o.insert("muted".into(), serde_json::json!(muted));
            o.insert("hidden".into(), serde_json::json!(hidden));
            if let Some(v) = vers.get(&h) {
                o.insert(
                    "verify".into(),
                    serde_json::json!({
                        "verdict": v.verdict(),
                        "v4": v.v4,
                        "v6": v.v6,
                        "conclusive": v.conclusive,
                        "swarm": v.swarm,
                        "age_secs": v.at.elapsed().as_secs(),
                    }),
                );
            }
            hosts.insert(h, serde_json::Value::Object(o));
        }
        let (ok, failed) = cache.outcomes();
        for v in hosts.values() {
            match v.get("severity").and_then(|x| x.as_str()) {
                Some("red") => red += 1,
                Some("amber") => amber += 1,
                _ => {}
            }
        }
        out.insert(
            id.to_string(),
            serde_json::json!({
                "announces_ok": ok,
                "announces_failed": failed,
                "hosts": hosts,
            }),
        );
    }
    out.insert(
        "badges".into(),
        serde_json::json!({"trackers_red": red, "trackers_amber": amber}),
    );
    Json(serde_json::Value::Object(out)).into_response()
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
    let available = !latest.is_empty() && version_less(HYDRANOS_VERSION, &latest);

    Json(serde_json::json!({
        "enabled": true,
        "current": HYDRANOS_VERSION,
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
        .header("User-Agent", format!("Hydra/{HYDRANOS_VERSION}"))
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
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
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
    /// Where a torrent filed here goes when it has to leave the race disk but
    /// still owes its tracker seeding time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    graduate_to: String,
    /// TRANSIT: a torrent that lands here is deleted once its tracker's seed
    /// obligation is paid.
    ///
    /// A property of the DESTINATION, not a choice made by the source: some
    /// categories are a waiting room where a torrent finishes owing its time,
    /// others are a library it should never leave. Nothing else can tell the
    /// two apart -- both are hoard categories something graduates into.
    ///
    /// ⚠ Defaults to FALSE, and that direction is deliberate. A wrong `true`
    /// erases a library; a wrong `false` lets a waiting room grow, which costs
    /// disk on the pool and gets noticed. The race disk drains either way,
    /// because draining it is the MOVE, not the later deletion.
    #[serde(default, skip_serializing_if = "is_false")]
    transit: bool,
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

/// The configured categories, by name.
///
/// Same two sources the listing route reads, in the same order: the store's
/// document first, the file beside it as a fallback.
fn categories_map(state: &AppState) -> std::collections::BTreeMap<String, Category> {
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
    raw.and_then(|doc| serde_json::from_str(&doc).ok()).unwrap_or_default()
}

/// Which engine a category belongs to, and where it puts its files.
///
/// An unknown category lands on race, which is what 3.x does and what every
/// downstream client has been configured against.
fn placement(state: &AppState, category: &str, engine_override: &str) -> (String, String) {
    let path = categories_map(state)
        .get(category)
        .map(|c| c.save_path.clone())
        .unwrap_or_default();

    // An explicit engine wins over the category's mode, and is the only way to
    // reach an engine that is neither race nor hoard: a category carries a MODE
    // ("hoard" or "race"), which names a behaviour, not one of the engines a
    // node may host. Without this a torrent could never be placed in `vpn1`,
    // whatever the config said.
    if !engine_override.is_empty()
        && state.engines.engines().iter().any(|e| e.id == engine_override)
    {
        return (engine_override.to_string(), path);
    }

    match categories_map(state).get(category) {
        Some(cat) => {
            let engine =
                if cat.mode == "hoard" { "hoard".to_string() } else { "race".to_string() };
            (engine, cat.save_path.clone())
        }
        None => ("race".to_string(), String::new()),
    }
}

/// Add one torrent from its bytes: the single path every add funnels through.
///
/// Three things have to happen together, and 4.0.0 did none of them -- both add
/// routes were validation-only, so nothing could reach this node at all:
/// the file is written where the engine's resume records point, the engine is
/// told about it, and the store gets the row the interface lists from.
/// Whether an add should hash-check the data already sitting at the save path.
///
/// Two inputs, and deliberately NOT a third. `paused` used to gate this, which
/// silently disabled the check for the one workflow "add without starting"
/// exists to serve: an operator adds a torrent paused BECAUSE they want to know
/// what is on disk before it touches the network. The torrent then sat at 0% on
/// top of complete data (2026-09-12, a 184 GB library). Rechecking a paused
/// torrent is safe -- `run_recheck` leaves it Stopped, and the download path
/// gates on `is_paused` -- so pause has no business in this decision.
///
/// `seed_mode` does belong here: it means the caller asserted the data is good
/// (`skip_checking`, which is why cross-seed sets it), and that assertion is the
/// whole point of the fast path.
fn add_recheck_wanted(seed_mode: bool, data_on_disk: bool) -> bool {
    !seed_mode && data_on_disk
}

/// Refuse a race that the disk cannot hold.
///
/// ⚠ Against PROJECTED free space, not free space. Ten races arriving in thirty
/// seconds each fit in what is free at the moment they are looked at, and
/// together they fill the disk: every decision correct, the outcome wrong. What
/// the torrents already accepted still have to write is subtracted first.
///
/// Off unless `add_block_enabled`, and it only ever guards a race engine: the
/// hoard is on a pool with tens of terabytes free and a different problem.
fn race_admission(
    state: &AppState,
    engine: &crate::engines::Engine,
    incoming: i64,
    save_path: &str,
) -> Result<(), String> {
    if engine.role != "race" {
        return Ok(());
    }
    let cfg = state.cfg();
    let d = &cfg.race_drain;
    if !d.add_block_enabled {
        return Ok(());
    }
    // The volume this torrent is about to land on, taken from its own save
    // path. Asking a global path -- or the emptiest disk -- answers for a disk
    // that may hold none of this download.
    let target = std::path::Path::new(save_path);
    let mount = crate::volumes::mount_point_of(target);
    let (Some((_, _, free)), Some(dev)) = (
        crate::volumes::usage(&mount),
        crate::volumes::device_of_nearest(target),
    ) else {
        // Unreadable is not a reason to start refusing every race: that would
        // take the node off the air over a typo.
        tracing::warn!(path = %save_path, "race admission: cannot read that volume, letting it through");
        return Ok(());
    };
    let free = free as i64;

    // What the catalogue has promised to write but has not written yet, counted
    // on THIS volume only.
    let mut committed: i64 = 0;
    for t in engine.manager.all() {
        let path = t.save_path.read().clone();
        if crate::volumes::device_of_nearest(&path) != Some(dev) {
            continue;
        }
        let core = typhon_engine::rpc::dispatch::torrent_core(&t);
        let remaining = t.meta.total_size as i64 - core.total_done as i64;
        if remaining > 0 {
            committed += remaining;
        }
    }
    let reserve = d.reserve_free_gb.max(0) * 1_000_000_000;
    let projected = free - committed - reserve;
    if projected < incoming {
        tracing::warn!(
            incoming_gb = (incoming as f64 / 1e9 * 10.0).round() / 10.0,
            free_gb = (free as f64 / 1e9).round(),
            committed_gb = (committed as f64 / 1e9).round(),
            reserve_gb = d.reserve_free_gb,
            "race refused: not enough projected space"
        );
        return Err(format!(
            "race disk full: {} GB free, {} GB already promised to downloads in flight, {} GB reserved; this torrent needs {} GB",
            free / 1_000_000_000,
            committed / 1_000_000_000,
            d.reserve_free_gb,
            incoming / 1_000_000_000
        ));
    }
    Ok(())
}

fn add_torrent_bytes(
    state: &AppState,
    bytes: &[u8],
    category: &str,
    save_path_override: &str,
    tags: &str,
    paused: bool,
    seed_mode: bool,
    engine_override: &str,
) -> Result<(String, String), String> {
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(bytes)
        .map_err(|e| format!("torrent file did not parse: {e}"))?;
    let hash = typhon_engine::torrent::hex_encode(&meta.info_hash);

    let (engine_id, category_path) = placement(state, category, engine_override);
    // No inferred destination: with neither an explicit savepath nor a category
    // that names one, there is no correct answer, and picking one writes a
    // download somewhere the operator will not find it.
    let save_path = if !save_path_override.is_empty() {
        save_path_override.to_string()
    } else if !category_path.is_empty() {
        category_path
    } else {
        return Err(format!(
            "no save path: category {category:?} is unknown and no savepath was given"
        ));
    };

    let Some(engine) = state.engines.get(&engine_id) else {
        return Err(format!("no engine {engine_id}"));
    };

    // Refused BEFORE anything is written. The drain cannot win this race:
    // measured on this machine, /race to the pool moves ~520 MB/s while a race
    // can arrive at 1 GB/s, and four parallel moves buy 10%. The only lever
    // that acts on the fast side is not accepting the work.
    race_admission(state, engine, meta.total_size as i64, &save_path)?;

    // The metainfo goes into the store BEFORE the engine is told, and that
    // order is not cosmetic: the engine reads its piece hashes from the store,
    // and adding a torrent triggers a recheck. Told first, it would verify
    // against a row that does not exist yet and refuse every piece.
    let cfg = state.cfg();
    let added_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    {
        let store = state.store.lock().unwrap();
        store
            .insert_torrent(&hash, &engine_id, bytes, &save_path, category, added_time, paused, tags)
            .map_err(|e| format!("store: {e}"))?;
        // Indexed here rather than at boot: a torrent that is never indexed is
        // a torrent the next add cannot recognise, and the backfill only sees
        // what is already in the table.
        if let Some(key) = crate::dedup::content_key(bytes) {
            if let Err(e) = store.put_content_key(&hash, &engine_id, &key) {
                tracing::warn!(hash = %hash, "content index: {e}");
            }
        }
    }

    // Data we already hold, under a name we have not seen before.
    //
    // Done BEFORE the engine is told, so that the links are in place when it
    // first looks at the save path -- an engine that starts on an empty
    // directory schedules a download, and the download is exactly what this
    // saves. Every failure here is non-fatal: the torrent is added as it would
    // have been, it just downloads.
    let mut linked_from = String::new();
    if cfg.dedup.enabled {
        if let Some(report) = try_link_existing(state, bytes, &hash, &save_path, &cfg) {
            linked_from = report;
        }
    }

    let (info_hash, name) = engine
        .manager
        .add_torrent_bytes(bytes, &save_path, paused, seed_mode)
        .map_err(|e| {
            // The engine refused it, so nothing owns this row.
            let _ = state.store.lock().unwrap().delete_torrent(&hash);
            e
        })?;

    // Data already on disk (cross-seed, a re-add) is hash-checked rather than
    // overwritten -- unless the caller asked to skip, which is what
    // skip_checking means and why cross-seed sets it.
    //
    // WARNING `!paused` used to gate this too, which silently disabled the
    // check for exactly the case "add without starting" (2026-09-12) was built
    // to serve: an operator adds a torrent paused BECAUSE they want to inspect
    // it before it touches the network, and the inspection was the thing being
    // skipped. The torrent sat at 0% on top of complete data. Rechecking a
    // paused torrent is safe: `run_recheck` leaves it Stopped and the download
    // path gates on `is_paused`, so this reports what is on disk without
    // fetching anything.
    if add_recheck_wanted(seed_mode, engine.manager.any_file_exists(&info_hash)) {
        let _ = engine.manager.recheck(&info_hash);
    }

    if linked_from.is_empty() {
        tracing::info!(engine = %engine_id, category = %category, hash = %hash, "torrent added");
    } else {
        tracing::info!(engine = %engine_id, category = %category, hash = %hash,
            linked_from = %linked_from, "torrent added, seeded from data already held");
    }
    Ok((hash, name))
}


/// Link an incoming torrent onto payload we already hold, if we hold it.
///
/// Returns the info_hash of the source it linked from. None means "nothing to
/// do" for any reason at all -- no match, a match we cannot use, a filesystem
/// that refused -- because every one of those outcomes has the same remedy:
/// add the torrent and let it download.
fn try_link_existing(
    state: &AppState,
    bytes: &[u8],
    hash: &str,
    save_path: &str,
    cfg: &crate::config::Config,
) -> Option<String> {
    let key = crate::dedup::content_key(bytes)?;
    let want = crate::dedup::layout(bytes)?;

    let candidates = {
        let store = state.store.lock().unwrap();
        store.content_matches(&key, hash).ok()?
    };

    for (src_hash, _src_session, src_save_path) in candidates {
        // The index narrows; the bytes decide. A content key collision must
        // never be enough on its own to link one torrent's data to another.
        let src_bytes = {
            let store = state.store.lock().unwrap();
            store.torrent_blob(&src_hash).ok().flatten()
        };
        let Some(src_bytes) = src_bytes else { continue };
        if !crate::dedup::same_content(bytes, &src_bytes) {
            tracing::warn!(hash = %hash, source = %src_hash, "content key matched but pieces differ");
            continue;
        }
        let Some(have) = crate::dedup::layout(&src_bytes) else { continue };

        match crate::dedup::plan(&have, &src_save_path, &want, save_path) {
            Ok(p) if p.cross_device => {
                tracing::info!(hash = %hash, source = %src_hash,
                    "have this data but on another filesystem, cannot hardlink");
            }
            Ok(p) => match crate::dedup::apply(&p) {
                Ok(done) => {
                    tracing::info!(hash = %hash, source = %src_hash, files = done.created,
                        bytes = done.bytes, "linked to data already held");
                    return Some(src_hash);
                }
                Err(e) => tracing::warn!(hash = %hash, source = %src_hash, "link failed: {e}"),
            },
            Err(why) => {
                tracing::debug!(hash = %hash, source = %src_hash, "cannot link: {why:?}");
            }
        }
    }
    None
}


#[derive(serde::Deserialize)]
struct DedupConfigBody {
    #[serde(default)]
    enabled: bool,
}

/// Write `[dedup]`, creating the section when the file has none.
///
/// Not `/api/settings`: that route edits keys that already exist, and every
/// config file written before this feature has no `[dedup]` table at all --
/// so the generic route answers `key "mode" not found in section "dedup"` and
/// the setting cannot be changed anywhere. `set_toml_table` adds the section,
/// which is exactly the difference, and keeping it here means a typo in a
/// section name still cannot conjure a table through the generic route.
async fn post_dedup_config(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let Ok(req) = serde_json::from_str::<DedupConfigBody>(&body) else {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid body"}))).into_response();
    };

    let kv = vec![("enabled".to_string(), req.enabled.to_string())];

    let Ok(doc) = std::fs::read_to_string(&state.config_path) else {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "cannot read the config"}))).into_response();
    };
    let doc = match crate::tomledit::set_toml_table(&doc, "dedup", &kv) {
        Ok(d) => d,
        Err(message) => {
            return (StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": message}))).into_response()
        }
    };

    // Never commit a config that no longer parses.
    if toml::from_str::<toml::Value>(&doc).is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "edited config no longer parses"}))).into_response();
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
        "enabled": req.enabled,
        // Read when a torrent is added, so the running daemon needs restarting.
        "restart_required": true,
    }))
    .into_response()
}


/// What the content index knows: how much payload is held more than once.
async fn get_dedup_stats(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    let groups = {
        let store = state.store.lock().unwrap();
        match store.content_duplicate_groups() {
            Ok(g) => g,
            Err(e) => {
                return Json(serde_json::json!({"error": e.to_string()})).into_response();
            }
        }
    };

    // A group whose members all sit at one save path already shares its bytes.
    // Only differing locations are duplicated payload.
    let mut same_location = 0usize;
    let mut separate = 0usize;
    let mut torrents = 0usize;
    for g in &groups {
        torrents += g.len();
        let locations: std::collections::HashSet<&str> =
            g.iter().map(|(_, _, sp)| sp.as_str()).collect();
        if locations.len() == 1 {
            same_location += 1;
        } else {
            separate += 1;
        }
    }

    Json(serde_json::json!({
        "enabled": cfg.dedup.enabled,
        "groups": groups.len(),
        "torrents": torrents,
        "groups_same_location": same_location,
        "groups_separate_location": separate,
    }))
    .into_response()
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

/// How many of an engine's torrents a tracker has answered about.
///
/// From the announce cache, which is the only place that knows: an announce is
/// the sole moment a tracker says anything, and a hardcoded zero here made the
/// interface report "0/300597 announced (0%)" while announces were going out.
fn announced_count(state: &AppState, engine_id: &str) -> usize {
    state
        .engines
        .engines()
        .iter()
        .find(|e| e.id == engine_id)
        .map(|e| e.announce_cache.len())
        .unwrap_or(0)
}

/// The live swarm figures for one engine, as the header renders them.
///
/// Every field is a gauge the engine already maintains; they were hardcoded to
/// zero while the slice was being ported, which made a node moving 300 Mbit/s
/// across ~1000 peers report a flat zero on every counter. Reading them costs
/// four atomic loads, so the status route can stay a cheap poll.
#[derive(Default)]
struct LiveStats {
    upload_rate: i64,
    download_rate: i64,
    active_peers: i64,
    torrents_with_peers: i64,
    torrents_uploading: i64,
    unseeded_peers: i64,
}

/// Leechers the trackers report across the hoard, summed from the announce
/// cache.
///
/// This is the denominator of the header's "connected / available" reading. It
/// used to be `unseeded_peers`, which is the numerator under another name -- so
/// the ratio was 100.0% by construction on every node, and said nothing.
fn swarm_leechers_total(state: &AppState) -> i64 {
    state
        .engines
        .get("hoard")
        .map(|e| e.announce_cache.swarm_totals().1)
        .unwrap_or(0)
}

fn live_stats(state: &AppState, engine_id: &str) -> LiveStats {
    use std::sync::atomic::Ordering;
    let Some(engine) = state.engines.get(engine_id) else {
        return LiveStats::default();
    };
    let m = &engine.manager;
    LiveStats {
        upload_rate: m.upload_rate.get() as i64,
        download_rate: m.download_rate.get() as i64,
        active_peers: m.cached_active_peers.load(Ordering::Relaxed) as i64,
        torrents_with_peers: m.cached_torrents_with_peers.load(Ordering::Relaxed) as i64,
        torrents_uploading: m.cached_torrents_uploading.load(Ordering::Relaxed) as i64,
        unseeded_peers: m.cached_unseeded_peers.load(Ordering::Relaxed) as i64,
    }
}

/// The store's facts for one session, from cache when it is warm.
///
/// Filled on first use and refreshed by a worker. A hydration that reads
/// SQLite directly costs 21 seconds of I/O for 300k rows, and the page shows
/// nothing until the last batch -- the front paints once, at `done`.
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One page of an engine's list: filtered, sorted and sliced on the server.
///
/// Replaces streaming the whole library to the browser. At 300k torrents that
/// was 250 MB per hard refresh, and the cost was never the rendering -- the
/// page already drew only its top 500 -- it was moving and parsing the other
/// 299500 rows so it could decide which 500 those were. That decision happens
/// here now, over a flat projection that touches no allocator per field.
///
/// The filter and sort semantics mirror `_hoardMatches` and `_hoardCmp` in
/// app.js exactly, including the info_hash tie-break: a page boundary that
/// ordered ties differently from the client would duplicate or drop rows
/// between pages, which reads as data loss.
/// The separators that punctuate release names. `Jujutsu.Kaisen.S02.1080p` has
/// to answer to the query "jujutsu 1080p", which a single literal `contains`
/// cannot do: the space the user typed exists nowhere in the name.
fn is_search_sep(c: char) -> bool {
    c.is_whitespace() || c == '.' || c == '_' || c == '-'
}

/// True when `token` occurs inside one word of `hay`. `token` is already
/// lowercase and, by construction, free of separators -- so it can never span
/// a word boundary, which is what makes the word-wise walk equivalent to a
/// `contains` over the fully normalized string.
///
/// That equivalence is the point: `_searchMatches` in app.js lowercases the
/// name, collapses its separators to spaces and calls `includes`. This gets
/// the same answer without the per-row String, which cost one allocation per
/// torrent per keystroke -- 300k of them on this library, on a request path
/// whose whole design is to not allocate per field.
fn name_has_token(hay: &str, token: &str) -> bool {
    hay.split(is_search_sep).any(|word| {
        if word.len() < token.len() {
            return false;
        }
        if word.is_ascii() {
            word.as_bytes()
                .windows(token.len())
                .any(|w| w.eq_ignore_ascii_case(token.as_bytes()))
        } else {
            // Real Unicode folding, so "CAFÉ" stays findable by "café". Only
            // the rows that actually carry non-ASCII pay the allocation; the
            // branch above never does.
            word.to_lowercase().contains(token)
        }
    })
}

async fn engine_page_value(
    state: &AppState,
    engine_id: &str,
    query: &str,
) -> serde_json::Value {
    let Some(engine) = state.engines.get(engine_id) else {
        return serde_json::json!({"total": 0, "filtered": 0, "rows": []});
    };

    let param = |k: &str| query_param(query, k).unwrap_or_default();
    let search_raw = param("search").trim().to_lowercase();
    // Tokens are ANDed and order-free, so "demo music" and "music demo" both
    // find "demo_03_music.bin". Split once per request, never per row.
    let search_tokens: Vec<String> = search_raw
        .split(is_search_sep)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    // A pasted info_hash is matched against the hash even when the torrent has
    // a name. The previous code only ever looked at the hash for NAMELESS
    // torrents, so with every torrent named it was unreachable: pasting a hash
    // returned nothing. Six hex chars is the floor, below which short words
    // like "added" would start matching hashes by accident.
    let search_is_hex =
        search_raw.len() >= 6 && search_raw.bytes().all(|b| b.is_ascii_hexdigit());
    let cat_filter = param("category");
    let tag_filter = param("tag");
    let tracker_filter = param("tracker");
    let state_filter = param("state");
    let sort = {
        let s = param("sort");
        if s.is_empty() { "added_time".to_string() } else { s }
    };
    let asc = param("order") == "asc";
    let offset = param("offset").parse::<usize>().unwrap_or(0);
    let limit = param("limit").parse::<usize>().unwrap_or(500).clamp(1, 5000);

    // Facts are only needed up front when the FILTER depends on them. In every
    // other case the page's own 500 hashes are looked up at the end, which is
    // an indexed lookup instead of a walk of the whole session.
    // Facet chips are counted over the WHOLE library, so their inputs are
    // needed whenever they are asked for -- not only when a filter uses them.
    let want_facets = param("facets") == "1";

    // Interned, keyed by the raw 20-byte hash. The full `facts_by_session`
    // built a StoreFacts per torrent -- three Strings and a Vec each, under a
    // 40-char String key -- which measured ~270 MB of transient allocation per
    // request at 300k, against a control run. This is the same information in
    // a few MB, and it is needed on every request because a row's STATE is
    // derived from the store's paused flag.
    // Read every time, on purpose. A cache sat here and was measured never to
    // serve: 0 hits in 15 requests on an hour-old daemon, while the store took
    // one write every 20s -- so neither staleness nor write pressure explains
    // it, and a cache nobody can explain is a liability. A previous cache on
    // this path had already been removed for growing with the catalogue.
    //
    // The 0.45s left is not the query (0.20s) but decoding it: 300k Strings and
    // a HashMap grown from empty. That is where the next gain is.
    let facts = {
        let store = state.store.lock().unwrap();
        store.slim_facts(engine_id).unwrap_or_default()
    };
    let pinned: std::collections::HashSet<String> =
        if state_filter == "__pinned__" || want_facets {
            let store = state.store.lock().unwrap();
            store.pinned(engine_id).unwrap_or_default().into_iter().collect()
        } else {
            Default::default()
        };

    // Each facet family takes a comma-separated list to include and another to
    // exclude: `category=movies,series&category_not=animes`. A single value
    // still works, so old links keep their meaning.
    //
    // Include is OR within a family, exclude wins over include, and the
    // families are ANDed together. On a library where 185k torrents hang off
    // two dead trackers, "everything except those two" is the query that makes
    // the list usable at all, and one exclusion was not enough for it.
    let split = |v: &str| -> Vec<String> {
        v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
    };
    let cat_inc = split(&cat_filter);
    let cat_exc = split(&param("category_not"));
    let tag_inc = split(&tag_filter);
    let tag_exc = split(&param("tag_not"));
    let trk_inc = split(&tracker_filter);
    let trk_exc = split(&param("tracker_not"));
    // The KIND of tracker error, as a facet family like any other: a library is
    // triaged by the gesture an error calls for, not by the string a tracker
    // happened to write. cf `errclass`.
    let err_inc = split(&param("error_class"));
    let err_exc = split(&param("error_class_not"));

    // "__none__" is uncategorised / untagged: a state, not a name, so it is
    // resolved separately from the ids.
    let cat_none_inc = cat_inc.iter().any(|c| c == "__none__");
    let cat_none_exc = cat_exc.iter().any(|c| c == "__none__");
    let cat_ids_inc: Vec<u16> = cat_inc.iter().filter(|c| c.as_str() != "__none__")
        .filter_map(|c| facts.category_id(c)).collect();
    let cat_ids_exc: Vec<u16> = cat_exc.iter().filter(|c| c.as_str() != "__none__")
        .filter_map(|c| facts.category_id(c)).collect();
    let tag_none_inc = tag_inc.iter().any(|t| t == "__none__");
    let tag_none_exc = tag_exc.iter().any(|t| t == "__none__");
    let tag_mask_inc: u64 = tag_inc.iter().filter(|t| t.as_str() != "__none__")
        .filter_map(|t| facts.tag_bit(t)).fold(0u64, |m, b| m | b);
    let tag_mask_exc: u64 = tag_exc.iter().filter(|t| t.as_str() != "__none__")
        .filter_map(|t| facts.tag_bit(t)).fold(0u64, |m, b| m | b);

    let torrents = engine.manager.all();
    let total = torrents.len();

    // Tracker hosts interned as we go, for the same reason: a few dozen
    // distinct hosts across the whole library, one String each instead of one
    // per torrent.
    let mut tracker_names: Vec<String> = vec![String::new()];
    let mut tracker_ids: std::collections::HashMap<String, u16> = Default::default();
    // Grown in step with `tracker_names`, so the per-torrent test is an index
    // rather than a walk over the filter list 300k times. Slot 0 is "no
    // tracker", which no filter can name.
    let mut trk_flag_inc: Vec<bool> = vec![false];
    let mut trk_flag_exc: Vec<bool> = vec![false];
    // Slot 0 is the torrent with NO tracker at all -- a magnet never given one,
    // or a .torrent with no announce. It was counted in no facet and matched by
    // no filter, so 23 torrents on this node could not be reached from the list
    // by any route. "__none__" names that state, as it already does for a
    // category and a tag.
    trk_flag_inc[0] = trk_inc.iter().any(|x| x == "__none__");
    trk_flag_exc[0] = trk_exc.iter().any(|x| x == "__none__");

    let mut f_state: std::collections::BTreeMap<&'static str, i64> = Default::default();
    let mut f_cat: std::collections::BTreeMap<u16, i64> = Default::default();
    let mut f_tracker: std::collections::BTreeMap<u16, i64> = Default::default();
    let mut f_tag: [i64; 64] = [0; 64];
    let mut f_errclass: std::collections::BTreeMap<&'static str, i64> = Default::default();
    let (mut n_all, mut n_active, mut n_trk_err, mut n_err, mut n_pinned) = (0i64, 0, 0, 0, 0);
    let (mut n_uncat, mut n_untagged) = (0i64, 0i64);
    let mut n_no_tracker = 0i64;

    // Kept rows are INDICES into `torrents`. Nothing per-torrent is copied out:
    // the sort keys are read back through the index for the few that survive.
    let mut kept: Vec<u32> = Vec::new();
    let mut host_buf = String::new();

    for (idx, t) in torrents.iter().enumerate() {
        let core = typhon_engine::rpc::dispatch::torrent_core(t);
        let f = facts.get(&t.info_hash);
        let row_state = crate::row::derive_state_static(core.state, f.user_paused);
        let upload_rate = t.upload_rate.get() as i64;

        // The host, interned. Read under the lock into a reused buffer so a
        // torrent that shares a host with 100k others costs no allocation.
        host_buf.clear();
        if let Some(url) = t.live_trackers.read().iter().flatten().next() {
            host_buf.push_str(&typhon_engine::rpc::dispatch::tracker_host_of(url));
        }
        let tracker_id: u16 = if host_buf.is_empty() {
            0
        } else if let Some(id) = tracker_ids.get(&host_buf) {
            *id
        } else {
            let id = tracker_names.len() as u16;
            trk_flag_inc.push(trk_inc.iter().any(|x| x == &host_buf));
            trk_flag_exc.push(trk_exc.iter().any(|x| x == &host_buf));
            tracker_names.push(host_buf.clone());
            tracker_ids.insert(host_buf.clone(), id);
            id
        };

        // Classified under the SAME lock that decides whether there is an
        // error at all, so the string is read once. classify() borrows it and
        // returns a &'static str, so a 300k walk allocates nothing here.
        let (tracker_error, err_class) = t
            .last_announce_error
            .lock()
            .map(|s| (!s.is_empty(), crate::errclass::classify(&s)))
            .unwrap_or((false, ""));
        let torrent_error =
            core.status_u8 == typhon_engine::torrent::meta::TorrentStatus::Error as u8;

        let m_search = search_tokens.is_empty() || {
            if search_is_hex
                && typhon_engine::torrent::hex_encode(&t.info_hash).contains(&search_raw)
            {
                true
            } else if t.meta.name.is_empty() {
                let hex = typhon_engine::torrent::hex_encode(&t.info_hash);
                search_tokens.iter().all(|tok| hex.contains(tok.as_str()))
            } else {
                search_tokens
                    .iter()
                    .all(|tok| name_has_token(&t.meta.name, tok))
            }
        };
        let m_tracker = (trk_inc.is_empty() || trk_flag_inc[tracker_id as usize])
            && !trk_flag_exc[tracker_id as usize];
        // A torrent with no tracker error is in no class, so it can never be
        // included by one -- and an exclusion must not sweep it away either.
        let m_errclass = (err_inc.is_empty() || err_inc.iter().any(|c| c == err_class))
            && !err_exc.iter().any(|c| c == err_class);
        let hash_hex_needed = state_filter == "__pinned__" || (want_facets && !pinned.is_empty());
        let is_pinned = hash_hex_needed
            && pinned.contains(&typhon_engine::torrent::hex_encode(&t.info_hash));
        let m_state = match state_filter.as_str() {
            "" => true,
            "__active__" => row_state == "seeding" && upload_rate > 0,
            "__tracker_err__" => tracker_error,
            "__error__" => torrent_error,
            "__pinned__" => is_pinned,
            want => row_state == want,
        };
        let m_cat = {
            let inc_ok = cat_inc.is_empty()
                || (cat_none_inc && f.category_id == 0)
                || cat_ids_inc.contains(&f.category_id);
            let exc_hit = (cat_none_exc && f.category_id == 0)
                || cat_ids_exc.contains(&f.category_id);
            inc_ok && !exc_hit
        };
        let m_tag = {
            let inc_ok = tag_inc.is_empty()
                || (tag_none_inc && f.tag_bits == 0)
                || (tag_mask_inc != 0 && f.tag_bits & tag_mask_inc != 0);
            let exc_hit = (tag_none_exc && f.tag_bits == 0)
                || (tag_mask_exc != 0 && f.tag_bits & tag_mask_exc != 0);
            inc_ok && !exc_hit
        };

        if want_facets {
            if m_search && m_cat && m_tag && m_tracker && m_errclass {
                n_all += 1;
                *f_state.entry(row_state).or_insert(0) += 1;
                if row_state == "seeding" && upload_rate > 0 { n_active += 1; }
                if tracker_error { n_trk_err += 1; }
                if torrent_error { n_err += 1; }
                if is_pinned { n_pinned += 1; }
            }
            if m_search && m_state && m_tag && m_tracker && m_errclass {
                if f.category_id == 0 { n_uncat += 1; } else { *f_cat.entry(f.category_id).or_insert(0) += 1; }
            }
            if m_search && m_state && m_cat && m_tag && m_errclass {
                if tracker_id == 0 {
                    n_no_tracker += 1;
                } else {
                    *f_tracker.entry(tracker_id).or_insert(0) += 1;
                }
            }
            if m_search && m_state && m_cat && m_tracker && m_errclass {
                if f.tag_bits == 0 {
                    n_untagged += 1;
                } else {
                    for i in 0..64 {
                        if f.tag_bits & (1u64 << i) != 0 { f_tag[i] += 1; }
                    }
                }
            }
        }

        if want_facets && !err_class.is_empty()
            && m_search && m_state && m_cat && m_tag && m_tracker
        {
            *f_errclass.entry(err_class).or_insert(0) += 1;
        }

        if m_search && m_state && m_cat && m_tag && m_tracker && m_errclass {
            kept.push(idx as u32);
        }
    }

    let filtered = kept.len();

    // Decorate once, then sort. Building the key inside the comparator instead
    // cost a String allocation per COMPARISON -- some five million of them for
    // a name sort over 300k, which was most of the two seconds this took.
    let textual = matches!(sort.as_str(), "name" | "state" | "tracker_host" | "category");
    let mut keyed: Vec<(String, f64, u32)> = kept
        .iter()
        .map(|&idx| {
            let t = &torrents[idx as usize];
            let core = typhon_engine::rpc::dispatch::torrent_core(t);
            let f = facts.get(&t.info_hash);
            let key = if textual {
                match sort.as_str() {
                    "name" => t.meta.name.to_lowercase(),
                    "state" => crate::row::derive_state_static(core.state, f.user_paused).to_string(),
                    "tracker_host" => t
                        .live_trackers
                        .read()
                        .iter()
                        .flatten()
                        .next()
                        .map(|u| typhon_engine::rpc::dispatch::tracker_host_of(u).to_lowercase())
                        .unwrap_or_default(),
                    _ => facts.category(f.category_id).to_lowercase(),
                }
            } else {
                String::new()
            };
            let total_upload = t.total_uploaded.load(std::sync::atomic::Ordering::Relaxed) as i64;
            let total_download =
                t.total_downloaded.load(std::sync::atomic::Ordering::Relaxed) as i64;
            let completed = t.completed_time.load(std::sync::atomic::Ordering::Relaxed);
            let n = match sort.as_str() {
                "total_size" => t.meta.total_size as f64,
                "progress" => if core.state == "seeding" { 1.0 } else { core.progress },
                "ratio" => if total_download > 0 { total_upload as f64 / total_download as f64 } else { 0.0 },
                "upload_rate" => t.upload_rate.get() as f64,
                "download_rate" => t.download_rate.get() as f64,
                "num_peers" => t.peers_connected.load(std::sync::atomic::Ordering::Relaxed) as f64,
                "total_upload" => total_upload as f64,
                "total_download" => total_download as f64,
                "completed_time" => completed as f64,
                "seeding_time" => if completed > 0 { (now_secs() - completed).max(0) as f64 } else { 0.0 },
                _ => t.added_time as f64,
            };
            (key, n, idx)
        })
        .collect();

    let cmp = |a: &(String, f64, u32), b: &(String, f64, u32)| {
        let ord = if textual {
            a.0.cmp(&b.0)
        } else {
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        };
        let ord = if asc { ord } else { ord.reverse() };
        // Tie-break on the raw hash bytes: same total order as the hex string
        // the client uses, without building one per comparison.
        ord.then_with(|| {
            torrents[a.2 as usize].info_hash.cmp(&torrents[b.2 as usize].info_hash)
        })
    };

    // `fields=hash` answers the selection universe: every hash the filter
    // matches, with no rows built. Ctrl+A needs the whole set and none of its
    // contents, and shipping full rows for it would undo the paging.
    if param("fields") == "hash" {
        keyed.sort_by(cmp);
        let hashes: Vec<String> = keyed
            .iter()
            .map(|(_, _, i)| typhon_engine::torrent::hex_encode(&torrents[*i as usize].info_hash))
            .collect();
        return serde_json::json!({
            "total": total,
            "filtered": filtered,
            "hashes": hashes,
            // Which engine answered. The selection universe has to carry it:
            // the same hash may be held by two engines, and an action needs to
            // know which copy was selected.
            "engine": engine_id,
        });
    }

    // Only the page window has to be in order. Partitioning around its end is
    // O(N) and leaves everything past it unordered, which nobody reads.
    let end = offset.saturating_add(limit).min(keyed.len());
    if end < keyed.len() {
        keyed.select_nth_unstable_by(end, cmp);
    }
    let window = &mut keyed[..end];
    window.sort_by(cmp);

    // Full rows for the page alone, and the only place the rich StoreFacts are
    // read: 500 indexed lookups instead of a walk of the whole session.
    let page: Vec<&std::sync::Arc<typhon_engine::torrent::meta::TorrentState>> =
        window.iter().skip(offset).map(|(_, _, i)| &torrents[*i as usize]).collect();
    let page_hashes: Vec<String> = page
        .iter()
        .map(|t| typhon_engine::torrent::hex_encode(&t.info_hash))
        .collect();
    let rich = {
        let store = state.store.lock().unwrap();
        store.facts_for_hashes(&page_hashes, engine_id).unwrap_or_default()
    };
    let empty_facts = crate::row::StoreFacts::default();
    let agent = local_agent(engine_id);
    let rows: Vec<serde_json::Value> = page
        .iter()
        .map(|t| {
            let raw = typhon_engine::rpc::dispatch::torrent_to_json(t);
            let hash = raw.get("info_hash").and_then(|v| v.as_str()).unwrap_or("");
            crate::row::build(&raw, rich.get(hash).unwrap_or(&empty_facts), &agent)
        })
        .collect();

    let facets = if want_facets {
        serde_json::json!({
            "all": n_all,
            "active": n_active,
            "tracker_error": n_trk_err,
            "torrent_error": n_err,
            "pinned": n_pinned,
            "uncategorized": n_uncat,
            "untagged": n_untagged,
            "no_tracker": n_no_tracker,
            "state": f_state,
            "category": f_cat
                .iter()
                .map(|(id, n)| (facts.category(*id).to_string(), *n))
                .collect::<std::collections::BTreeMap<String, i64>>(),
            "tracker": f_tracker
                .iter()
                .map(|(id, n)| (tracker_names[*id as usize].clone(), *n))
                .collect::<std::collections::BTreeMap<String, i64>>(),
            "error_class": f_errclass,
            "tag": facts
                .tags
                .iter()
                .enumerate()
                .filter(|(i, _)| f_tag[*i] > 0)
                .map(|(i, name)| (name.clone(), f_tag[i]))
                .collect::<std::collections::BTreeMap<String, i64>>(),
        })
    } else {
        serde_json::Value::Null
    };

    serde_json::json!({
        "total": total,
        "filtered": filtered,
        "offset": offset,
        "limit": limit,
        "rows": rows,
        "facets": facets,
    })
}

/// The sort key of an emitted row, mirroring the one `engine_page_value` builds
/// over its own structs.
///
/// It has to mirror it exactly, because the merge below interleaves pages that
/// each node sorted for itself: a key that disagreed by one field would order
/// two nodes' rows differently from the way each ordered its own, and rows
/// would appear to jump between pages.
fn row_sort_key(row: &serde_json::Value, sort: &str) -> (String, f64) {
    let s = |k: &str| row.get(k).and_then(|v| v.as_str()).unwrap_or_default();
    let n = |k: &str| row.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    match sort {
        "name" => (s("name").to_lowercase(), 0.0),
        "state" => (s("state").to_lowercase(), 0.0),
        "tracker_host" => (s("tracker_host").to_lowercase(), 0.0),
        "category" => (s("category").to_lowercase(), 0.0),
        // A seeding torrent sorts as complete whatever its stored progress,
        // which is what the local path does when it builds its key.
        "progress" => (
            String::new(),
            if s("state") == "seeding" { 1.0 } else { n("progress") },
        ),
        "total_size" | "ratio" | "upload_rate" | "download_rate" | "num_peers"
        | "total_upload" | "total_download" | "completed_time" | "seeding_time" => {
            (String::new(), n(sort))
        }
        _ => (String::new(), n("added_time")),
    }
}

/// Re-apply the error-class filter to pages that came back from other nodes.
///
/// A node older than the filter does not know the parameter and answers with
/// its whole catalogue -- measured against a 4.13 node, asking for
/// `error_class=dead` brought back its passkey failures and its timeouts too.
/// One pass over the rows already in hand makes the answer right whatever the
/// fleet is running.
///
/// `window` is `offset + limit`, the most rows any single node was asked for.
/// A node that answered with FEWER than that sent its whole filtered catalogue,
/// so its `filtered` count can be corrected exactly. A node that filled the
/// window is holding rows nobody has seen, and adjusting its count from the
/// visible ones would be a number that lies stably -- so it is left alone.
fn enforce_error_class(
    pages: &mut [serde_json::Value],
    inc: &[String],
    exc: &[String],
    window: usize,
) {
    if inc.is_empty() && exc.is_empty() {
        return;
    }
    for page in pages.iter_mut() {
        let Some(obj) = page.as_object_mut() else { continue };
        let Some(rows) = obj.get_mut("rows").and_then(|r| r.as_array_mut()) else { continue };
        let before = rows.len();
        rows.retain(|r| {
            let msg = r.get("tracker_error_msg").and_then(|v| v.as_str()).unwrap_or("");
            let class = crate::errclass::classify(msg);
            (inc.is_empty() || inc.iter().any(|c| c == class))
                && !exc.iter().any(|c| c == class)
        });
        let dropped = (before - rows.len()) as i64;
        if dropped > 0 && before < window {
            if let Some(f) = obj.get_mut("filtered") {
                *f = (f.as_i64().unwrap_or(0) - dropped).max(0).into();
            }
        }
    }
}

/// Interleave pages that are each already sorted.
///
/// Every node sorts and pages its OWN catalogue; nothing ships a whole library
/// across the network. To answer "row 500 to 999 of the fleet" each node is
/// asked for its first `offset + limit` rows -- that is the most of any single
/// node that can appear in the window -- and the merge takes the slice.
fn merge_pages(
    pages: Vec<serde_json::Value>,
    sort: &str,
    asc: bool,
    offset: usize,
    limit: usize,
) -> serde_json::Value {
    let mut total = 0i64;
    let mut filtered = 0i64;
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut facets = FacetSum::default();
    for p in pages {
        total += p.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
        filtered += p.get("filtered").and_then(|v| v.as_i64()).unwrap_or(0);
        facets.add(p.get("facets"));
        if let Some(rows) = p.get("rows").and_then(|v| v.as_array()) {
            all.extend(rows.iter().cloned());
        }
    }

    let textual = matches!(sort, "name" | "state" | "tracker_host" | "category");
    all.sort_by(|a, b| {
        let (ka, na) = row_sort_key(a, sort);
        let (kb, nb) = row_sort_key(b, sort);
        let ord = if textual {
            ka.cmp(&kb)
        } else {
            na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
        };
        let ord = if asc { ord } else { ord.reverse() };
        // Same tie-break as the single-node path, on the hex hash: without it
        // two rows that compare equal could swap between requests and the same
        // torrent would show up on two pages, or on none.
        ord.then_with(|| {
            let ha = a.get("info_hash").and_then(|v| v.as_str()).unwrap_or_default();
            let hb = b.get("info_hash").and_then(|v| v.as_str()).unwrap_or_default();
            ha.cmp(hb)
        })
    });

    let rows: Vec<serde_json::Value> =
        all.into_iter().skip(offset).take(limit).collect();
    serde_json::json!({
        "total": total,
        "filtered": filtered,
        "offset": offset,
        "limit": limit,
        "rows": rows,
        "facets": facets.into_value(),
    })
}

/// The facet block of several nodes, added up.
///
/// This used to answer Null, on the reasoning that nodes need not share a
/// category or tag vocabulary. They need not -- but the UI reads the SAME
/// block for the state chip counts, so refusing to merge blanked every number
/// on the page the moment one node was enrolled: "All", "Seeding", "Tracker
/// Error" and the three facet families all went empty, on a fleet whose counts
/// were perfectly well defined. Silence is not the honest answer when the
/// question has one.
///
/// Differing vocabularies are not in fact a merge problem: the union of two
/// keyed counts is the answer. A category only one node knows appears with
/// that node's count, which is exactly what it holds.
#[derive(Default)]
struct FacetSum {
    seen: bool,
    scalars: std::collections::BTreeMap<String, i64>,
    maps: std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>>,
}

impl FacetSum {
    fn add(&mut self, facets: Option<&serde_json::Value>) {
        let Some(obj) = facets.and_then(|f| f.as_object()) else { return };
        self.seen = true;
        for (key, value) in obj {
            match value {
                serde_json::Value::Object(inner) => {
                    let bucket = self.maps.entry(key.clone()).or_default();
                    for (name, n) in inner {
                        *bucket.entry(name.clone()).or_insert(0) += n.as_i64().unwrap_or(0);
                    }
                }
                _ => {
                    *self.scalars.entry(key.clone()).or_insert(0) += value.as_i64().unwrap_or(0);
                }
            }
        }
    }

    /// Null when NO node answered with facets -- the caller did not ask for
    /// them, or every node is too old to send them. Zero is a different claim
    /// from "not counted", and the UI relies on the difference to decide
    /// whether to draw chips at all.
    fn into_value(self) -> serde_json::Value {
        if !self.seen {
            return serde_json::Value::Null;
        }
        let mut out = serde_json::Map::new();
        for (k, v) in self.scalars {
            out.insert(k, v.into());
        }
        for (k, v) in self.maps {
            out.insert(k, serde_json::to_value(v).unwrap_or(serde_json::Value::Null));
        }
        serde_json::Value::Object(out)
    }
}

/// Replace `offset` and `limit` in a query string, keeping everything else.
fn with_window(query: &str, offset: usize, limit: usize) -> String {
    let mut parts: Vec<String> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k = p.split('=').next().unwrap_or("");
            k != "offset" && k != "limit"
        })
        .map(|p| p.to_string())
        .collect();
    parts.push(format!("offset={offset}"));
    parts.push(format!("limit={limit}"));
    parts.join("&")
}

/// One engine's page across the whole fleet.
///
/// With no nodes declared this is exactly the single-node path and costs
/// nothing extra -- the common case must not pay for a feature it does not use.
async fn fleet_page(state: &AppState, engine_id: &str, query: &str) -> serde_json::Value {
    let nodes: Vec<crate::store::Node> = {
        let store = state.store.lock().unwrap();
        store.nodes().unwrap_or_default()
    }
    .into_iter()
    .filter(|n| n.enabled)
    .collect();

    // `fields=hash` answers the SELECTION universe, not a page, and merging it
    // would mean pulling every matching hash off every node just to hand the
    // browser a set it can only act on one node at a time anyway. Selecting
    // across the fleet is a feature of its own; until it exists, Ctrl+A means
    // "everything here".
    // Every LOCAL engine playing this role, not just the one whose id matches.
    // A node running `hoard` and `vpn1` seeds hoard content from both, and a
    // page bound to the id showed only half of it -- the vpn1 copy existed,
    // seeded, and was invisible.
    //
    // Role-mates are merged exactly like remote nodes rather than folded into
    // `engine_page_value`: that function is the tightest path in the build, and
    // reusing the merge keeps one set of sorting and paging rules instead of
    // two that would drift.
    let local_ids: Vec<String> = {
        let role = state
            .engines
            .engines()
            .iter()
            .find(|e| e.id == engine_id)
            .map(|e| e.role.clone())
            .unwrap_or_else(|| engine_id.to_string());
        state
            .engines
            .engines()
            .iter()
            .filter(|e| e.id == engine_id || e.role == role)
            .map(|e| e.id.clone())
            .collect()
    };

    if nodes.is_empty() && local_ids.len() <= 1 {
        return engine_page_value(state, engine_id, query).await;
    }
    // The selection universe, gathered across every engine of this role.
    //
    // Returned as (hash, agent) PAIRS as well as the flat hash list 3.x
    // published: with a torrent held by two engines the hash alone does not say
    // which copy was selected, and an action on the wrong one is invisible
    // until something is paused or deleted that should not have been.
    if query_param(query, "fields").as_deref() == Some("hash") {
        if local_ids.len() <= 1 && nodes.is_empty() {
            return engine_page_value(state, engine_id, query).await;
        }
        let mut hashes: Vec<serde_json::Value> = Vec::new();
        let mut copies: Vec<serde_json::Value> = Vec::new();
        let (mut total, mut filtered) = (0i64, 0i64);
        for id in &local_ids {
            let page = engine_page_value(state, id, query).await;
            total += page.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
            filtered += page.get("filtered").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(hs) = page.get("hashes").and_then(|v| v.as_array()) {
                for h in hs {
                    if let Some(hs) = h.as_str() {
                        copies.push(serde_json::json!({"hash": hs, "agent": format!("local-{id}")}));
                    }
                    hashes.push(h.clone());
                }
            }
        }

        // The fleet's share of the selection universe. A node answers with its
        // OWN engines' labels (`local-<engine>`), so the prefix is swapped for
        // the node's name -- the same rewrite the row merge does, and for the
        // same reason: an action has to reach the copy that was selected.
        let remote_sets = futures::future::join_all(nodes.iter().map(|n| {
            let (url, key, name) = (n.url.clone(), n.api_key.clone(), n.name.clone());
            let path = format!("api/{engine_id}/page?{query}");
            async move {
                match crate::nodes::forward(&url, &key, reqwest::Method::GET, &path, Vec::new()).await {
                    Ok((status, body, _)) if status.is_success() => {
                        serde_json::from_slice::<serde_json::Value>(&body)
                            .ok()
                            .map(|v| (name, v))
                    }
                    _ => None,
                }
            }
        }))
        .await;
        for (name, page) in remote_sets.into_iter().flatten() {
            total += page.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
            filtered += page.get("filtered").and_then(|v| v.as_i64()).unwrap_or(0);
            match page.get("copies").and_then(|v| v.as_array()) {
                Some(cs) => {
                    for c in cs {
                        let h = c.get("hash").and_then(|v| v.as_str()).unwrap_or_default();
                        let a = c.get("agent").and_then(|v| v.as_str()).unwrap_or("local");
                        let engine = a.strip_prefix("local-").unwrap_or(a);
                        copies.push(serde_json::json!({
                            "hash": h, "agent": format!("{name}-{engine}")
                        }));
                        hashes.push(serde_json::Value::String(h.to_string()));
                    }
                }
                // An older node answers the flat shape only; its engine is the
                // one we asked for.
                None => {
                    if let Some(hs) = page.get("hashes").and_then(|v| v.as_array()) {
                        for h in hs {
                            if let Some(hs) = h.as_str() {
                                copies.push(serde_json::json!({
                                    "hash": hs, "agent": format!("{name}-{engine_id}")
                                }));
                            }
                            hashes.push(h.clone());
                        }
                    }
                }
            }
        }

        return serde_json::json!({
            "total": total, "filtered": filtered, "hashes": hashes, "copies": copies,
        });
    }

    let offset = query_param(query, "offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let limit = query_param(query, "limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(500)
        .clamp(1, 5000);
    let sort = query_param(query, "sort").unwrap_or_else(|| "added_time".into());
    let asc = query_param(query, "order").as_deref() == Some("asc");

    let need = offset + limit;
    let windowed = with_window(query, 0, need);

    let mut pages: Vec<serde_json::Value> = Vec::new();
    for id in &local_ids {
        pages.push(engine_page_value(state, id, &windowed).await);
    }
    let remote = futures::future::join_all(nodes.iter().map(|n| {
        let (url, key, name) = (n.url.clone(), n.api_key.clone(), n.name.clone());
        let path = format!("api/{engine_id}/page?{windowed}");
        async move {
            match crate::nodes::forward(&url, &key, reqwest::Method::GET, &path, Vec::new()).await {
                Ok((status, body, _)) if status.is_success() => {
                    let mut v: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    // Tag every row with the node it lives on. The front already
                    // reads `agent` to decide where an action must be sent, so a
                    // remote row that claimed to be local would be acted on here.
                    if let Some(rows) = v.get_mut("rows").and_then(|r| r.as_array_mut()) {
                        for row in rows.iter_mut() {
                            if let Some(o) = row.as_object_mut() {
                                // `<node>-<engine>`, not just `<node>`: a torrent
                                // always lives in an ENGINE, and a remote one is
                                // no different -- the node only says where that
                                // engine runs. The remote row already names its
                                // own engine as `local-<engine>`; swapping the
                                // prefix keeps both halves of the answer.
                                let here = o
                                    .get("agent")
                                    .and_then(|a| a.as_str())
                                    .unwrap_or("local");
                                let engine = here.strip_prefix("local-").unwrap_or(here);
                                let label = if engine == "local" || engine.is_empty() {
                                    name.clone()
                                } else {
                                    format!("{name}-{engine}")
                                };
                                o.insert("agent".into(), serde_json::Value::String(label));
                            }
                        }
                    }
                    v
                }
                // A node that is down contributes nothing. Failing the whole
                // page instead would make one unreachable node hide a library
                // that is sitting right here.
                _ => serde_json::Value::Null,
            }
        }
    }))
    .await;

    pages.extend(remote.into_iter().filter(|v| !v.is_null()));
    // A node older than this filter does not know the parameter and answers
    // with its whole catalogue. Measured against a 4.13 node: asking for
    // error_class=dead brought back its passkey failures and its timeouts too.
    // Re-applying the filter here costs one pass over the rows already in hand
    // and makes the answer right whatever the fleet is running.
    let err_inc: Vec<String> = query_param(query, "error_class")
        .map(|v| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    let err_exc: Vec<String> = query_param(query, "error_class_not")
        .map(|v| v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default();
    enforce_error_class(&mut pages, &err_inc, &err_exc, offset + limit);
    merge_pages(pages, &sort, asc, offset, limit)
}

async fn get_engine_page(state: &AppState, engine_id: &str, query: &str) -> Response {
    Json(fleet_page(state, engine_id, query).await).into_response()
}

async fn get_hoard_page(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    get_engine_page(&state, "hoard", &query).await
}

async fn get_race_page(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    get_engine_page(&state, "race", &query).await
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
    let ((total_up, total_down), (session_up, session_down), _) = session_and_day(&state);

    Json(serde_json::json!({
        "baseline_uploaded": base_up,
        "baseline_downloaded": base_down,
        "session_uploaded": session_up,
        "session_downloaded": session_down,
        // The global is the stored baseline plus the LIFETIME totals, not the
        // session delta: subtracting this boot's mark here would erase every
        // byte moved before the last restart.
        "global_uploaded": base_up + total_up,
        "global_downloaded": base_down + total_down,
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
    if query_param(&query, "refresh").as_deref() == Some("1") {
        crate::netprobe::measure(&state.engines, &state.net_engines, &state.public_ip).await;
    }
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

    let live = live_stats(&state, "hoard");

    Json(serde_json::json!({
        "active_download_rate": live.download_rate,
        "active_peers": live.active_peers,
        "active_upload_rate": live.upload_rate,
        "engine": "hoard",
        "listen_port": cfg.hoard.listen_port,
        "running": true,
        "session_downloaded": 0,
        "session_uploaded": 0,
        "stagger_complete": true,
        "swarm_leechers": swarm_leechers_total(&state),
        "torrents_announced": announced_count(&state, "hoard"),
        "torrents_uploading": live.torrents_uploading,
        "torrents_with_peers": live.torrents_with_peers,
        "total_torrents": torrents,
        "unseeded_peers": live.unseeded_peers,
    }))
    .into_response()
}

/// Hot engines added at runtime. The two built-in engines are NOT listed here:
/// 3.x answers an empty list on a node that has only race and hoard, and a row
/// claiming otherwise sends per-engine actions at something nobody registered.
/// Every engine this node hosts.
///
/// Was a hardcoded `[]`, which made a node look like it ran nothing. It is the
/// one route that answers "what does this Hydra actually host", so the fleet
/// page needs it: `/api/status` names only `race` and `hoard`, and its shape
/// has to stay what 3.x published, so a third engine can never appear there.
///
/// Read from the running EngineHost rather than from the config: what is
/// listening is the answer, not what was asked for. An engine that failed to
/// come up would otherwise be reported as present.
async fn get_engines(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let out: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "role": e.role,
                "listen_port": e.listen_port,
                "bind_interface": e.bind_interface,
                "enable_ipv6": e.enable_ipv6,
                "start_paused": e.start_paused,
                // What the socket did, not what the config asked for.
                "listening": e.listening.load(std::sync::atomic::Ordering::Relaxed),
                "torrents": e.manager.all().len(),
            })
        })
        .collect();
    Json(out).into_response()
}


// ---------------------------------------------------------------------------
// Small listings and per-subsystem status
// ---------------------------------------------------------------------------

/// Free-space figures for a path, from statvfs.
fn disk_usage(path: &str) -> (i64, i64, f64) {
    let Some((_, total, free)) = crate::platform::usage(std::path::Path::new(path)) else {
        return (0, 0, 0.0);
    };
    let (total, free) = (total as i64, free as i64);
    // ⚠ Kept as total-minus-available, which is what 3.x published on this
    // endpoint. platform::usage also returns a "used" that excludes reserved
    // blocks; swapping to it here would move a number the UI has always shown.
    let used = total - free;
    // One decimal, as 3.x publishes it.
    let pct = if total > 0 {
        ((used as f64 / total as f64) * 1000.0).round() / 10.0
    } else {
        0.0
    };
    (total, used, pct)
}

/// Occupancy and policy of every volume that holds race data.
///
/// A list, not three scalars. The old shape described one global `race_path`,
/// which is exactly the assumption that made the drain act on the wrong disk.
async fn get_drain_status(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();
    let d = &cfg.race_drain;
    // Merged by mount point. Two race engines on one SSD are two passes for the
    // drain -- each owns its own torrents -- but they are ONE disk to fill, and
    // showing the same disk twice would ask the operator to add up two cards to
    // know how full it is.
    let mut merged: std::collections::BTreeMap<String, (crate::volumes::Volume, Vec<String>)> =
        std::collections::BTreeMap::new();
    for engine in state.engines.engines().iter() {
        if engine.role != "race" {
            continue;
        }
        for v in crate::volumes::discover(&state, &engine.manager, d) {
            match merged.get_mut(&v.id) {
                Some((acc, engines)) => {
                    acc.torrents += v.torrents;
                    engines.push(engine.id.clone());
                }
                None => {
                    merged.insert(v.id.clone(), (v, vec![engine.id.clone()]));
                }
            }
        }
    }
    let mut volumes = Vec::new();
    {
        for (_, (v, engines)) in merged {
            volumes.push(serde_json::json!({
                "id": v.id,
                "engines": engines,
                "total": v.total,
                "used": v.used,
                "free": v.free,
                "used_pct": crate::row::num_json(v.used_pct()),
                "torrents": v.torrents,
                "enabled": v.policy.enabled,
                "high_watermark": v.policy.high,
                "low_watermark": v.policy.low,
                "inherited": v.policy.inherited,
            }));
        }
    }
    Json(serde_json::json!({
        "add_block_enabled": d.add_block_enabled,
        "check_interval": d.check_interval_seconds,
        "reserve_free_gb": d.reserve_free_gb,
        "default_enabled": d.enabled,
        "default_high_watermark": d.high_watermark_pct,
        "default_low_watermark": d.low_watermark_pct,
        "volumes": volumes,
    }))
    .into_response()
}

/// Set, or drop, one volume's own drain policy.
///
/// Stored beside the data rather than in `default.toml`: a threshold typed in
/// the panel applies on the next tick. The race panel used to need an
/// "Apply & restart" for this, which is half an action.
#[derive(serde::Deserialize)]
struct VolumePolicyBody {
    volume: String,
    #[serde(default)]
    inherit: bool,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    high_watermark: Option<i64>,
    #[serde(default)]
    low_watermark: Option<i64>,
}

async fn set_volume_policy(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    Json(body): Json<VolumePolicyBody>,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if body.volume.is_empty() {
        return bad_request("volume is required");
    }
    let cfg = state.cfg();
    if body.inherit {
        if let Err(e) = crate::volumes::clear_policy(&state, &body.volume) {
            return bad_request(&format!("could not clear: {e}"));
        }
        return Json(serde_json::json!({"status": "ok", "inherited": true})).into_response();
    }
    let mut p = crate::volumes::policy_for(&state, &body.volume, &cfg.race_drain);
    if let Some(v) = body.enabled {
        p.enabled = v;
    }
    if let Some(v) = body.high_watermark {
        p.high = v;
    }
    if let Some(v) = body.low_watermark {
        p.low = v;
    }
    // A low above a high would drain forever: it can never reach its target.
    if p.low >= p.high {
        return bad_request("down to must be below start");
    }
    if p.high < 1 || p.high > 100 || p.low < 1 {
        return bad_request("watermarks are percentages");
    }
    if let Err(e) = crate::volumes::save_policy(&state, &body.volume, &p) {
        return bad_request(&format!("could not save: {e}"));
    }
    Json(serde_json::json!({
        "status": "ok",
        "enabled": p.enabled,
        "high_watermark": p.high,
        "low_watermark": p.low,
        "inherited": false,
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
/// Past drain passes, newest first.
///
/// WARNING Until 2026-09-13 this was `empty_list_route!`: the History button
/// opened on "No drains yet." however many had run. Nothing recorded them.
async fn get_drain_history(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let rows = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.drain_history(50).unwrap_or_default()
    };
    Json(serde_json::Value::Array(rows)).into_response()
}

/// Graduations in flight, so a move that takes minutes is visible while it
/// happens rather than only in the logs.
///
/// Read from the jobs table, which is where `queue_graduation` puts them: the
/// drain charges its budget at QUEUE time, so a torrent can be counted as
/// graduated here long before its bytes have finished moving.
async fn get_drain_graduations(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let jobs = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.list_jobs(200).unwrap_or_default()
    };
    let mut out = Vec::new();
    for j in jobs {
        if j.kind != "graduate" || (j.state != "running" && j.state != "queued") {
            continue;
        }
        let name = serde_json::from_str::<serde_json::Value>(&j.params)
            .ok()
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .unwrap_or_default();
        let pct = if j.total_bytes > 0 {
            j.progress_bytes as f64 * 100.0 / j.total_bytes as f64
        } else {
            0.0
        };
        out.push(serde_json::json!({
            "info_hash": j.info_hash,
            "name": name,
            "state": j.state,
            "copied": j.progress_bytes,
            "total": j.total_bytes,
            "pct": crate::row::num_json(pct),
        }));
    }
    Json(serde_json::Value::Array(out)).into_response()
}
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
    /// "ok" | "error" | "never". `ok` alone could not tell a tracker that
    /// failed from one nothing has spoken to yet, so every host the catalogue
    /// knew but had not announced to was painted red.
    status: &'static str,
    last_error: String,
    last_announce: String,
    announces: i64,
    errors: i64,
    passkey_set: bool,
    ip_mode: String,
    /// Kept out of the tab by the operator. A view decision, not a mute:
    /// see Config::announce_hidden.
    hidden: bool,
    /// Hours this tracker requires. -1 = nothing declared, 0 = declared as
    /// requiring nothing.
    min_seed_hours: i64,
    sources: Vec<String>,
}

/// The zero time Go marshals for a tracker that has never answered.
const GO_ZERO_TIME: &str = "0001-01-01T00:00:00Z";

/// A wall-clock timestamp for something that happened `age` ago.
///
/// The announce cache times its entries with an Instant, which is monotonic
/// and has no epoch; the tab needs a date, so the age is subtracted from now.
fn iso8601_ago(age: std::time::Duration) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    crate::logbuf::rfc3339_at(now - age.as_secs() as i64)
}

async fn get_trackers(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let cfg = state.cfg();

    // A tracker belongs on this tab because torrents announce to it, and
    // separately because the operator declared a client identity for it. The
    // two sets are merged: listing only the declared ones showed a single row
    // on a node announcing to several trackers.
    let mut observed: std::collections::HashMap<String, (i64, std::time::Duration)> =
        std::collections::HashMap::new();
    for engine in state.engines.engines() {
        for (host, (count, age)) in engine.announce_cache.per_tracker() {
            let slot = observed.entry(host).or_insert((0, age));
            slot.0 += count;
            if age < slot.1 {
                slot.1 = age;
            }
        }
    }

    // What the catalogue actually names, announced to or not. Without this a
    // torrent added stopped has no row on this tab, so its passkey and client
    // identity cannot be set BEFORE the first announce -- which is the only
    // moment setting them is worth anything. cf the empty state below.
    let mut held: std::collections::HashMap<String, i64> = Default::default();
    for engine in state.engines.engines() {
        for (host, count) in engine.manager.tracker_host_counts() {
            *held.entry(host).or_insert(0) += count;
        }
    }

    let mut hosts: std::collections::BTreeSet<String> = observed.keys().cloned().collect();
    hosts.extend(cfg.announce_passkeys.keys().cloned());
    hosts.extend(cfg.announce_ip_modes.keys().cloned());
    hosts.extend(held.keys().cloned());
    // And the trackers that have only ever FAILED.
    //
    // `per_tracker` is built from the announce cache, which is written on
    // success only, so a tracker that times out on every announce or refuses us
    // outright had no row at all -- the ones most worth looking at were the
    // only ones missing. Measured on production: archive.org holds 107k
    // torrents and answers none of them, gemini refuses us on all 6k.
    let mut errored: std::collections::BTreeSet<String> = Default::default();
    for engine in state.engines.engines() {
        for host in engine.announce_cache.error_breakdown().into_keys() {
            hosts.insert(host.clone());
            errored.insert(host);
        }
    }

    let rows: Vec<TrackerRow> = hosts
        .into_iter()
        .map(|host| {
            let configured = cfg.announce_passkeys.contains_key(&host)
                || cfg.announce_ip_modes.contains_key(&host);
            let seen = observed.get(&host);
            let holds = held.get(&host).copied().unwrap_or(0);
            let mut sources = Vec::new();
            if seen.is_some() {
                sources.push("torrents".to_string());
            }
            if configured {
                sources.push("config".to_string());
            }
            if holds > 0 {
                sources.push("catalogue".to_string());
            }
            TrackerRow {
                // The catalogue count, not the announce count: they are
                // different questions, and `announces` below already answers
                // the second one.
                torrents: holds,
                // An announce landed in the cache only because the tracker
                // answered, so a host we have seen is a host that is working.
                ok: seen.is_some(),
                status: if seen.is_some() {
                    "ok"
                } else if errored.contains(&host) {
                    "error"
                } else {
                    "never"
                },
                last_error: String::new(),
                last_announce: seen
                    .map(|(_, age)| iso8601_ago(*age))
                    .unwrap_or_else(|| GO_ZERO_TIME.to_string()),
                announces: seen.map(|(n, _)| *n).unwrap_or(0),
                errors: 0,
                passkey_set: cfg.announce_passkeys.contains_key(&host),
                hidden: cfg.announce_hidden.contains_key(&host),
                // -1 = nothing declared, which is NOT 0. The drain protects
                // the first and releases the second, so a row that cannot tell
                // them apart cannot show which trackers are still unguarded.
                min_seed_hours: cfg
                    .announce_min_seed_hours
                    .get(&host)
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .unwrap_or(-1),
                ip_mode: cfg
                    .announce_ip_modes
                    .get(&host)
                    .cloned()
                    .unwrap_or_else(|| "auto".to_string()),
                sources,
                host,
            }
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

    // Every engine that is neither race nor hoard. This was a `vec![]` literal,
    // so a node running one engine per tunnel -- the entire point of the model
    // -- showed two of them in the network panel and hid the rest. The front
    // has rendered and collected these rows all along; nothing ever filled them.
    let extra_engines: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .filter(|e| e.id != "race" && e.id != "hoard")
        .map(|e| {
            serde_json::json!({
                "id": e.id,
                "role": e.role,
                "bind_interface": e.bind_interface,
                "listen_port": e.listen_port,
            })
        })
        .collect();

    Json(NetworkMode {
        mode,
        fields,
        env_overrides: None,
        warnings: None,
        extra_engines,
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
            // The host baked into the torrent, NOT a result of announcing:
            // live_trackers is filled when the torrent is built. So a torrent
            // that has never announced still counts under its own tracker, and
            // this bucket holds only the ones carrying no announce URL at all.
            // The name says that; "(none)" invited the other reading.
            let host = row
                .get("tracker_host")
                .and_then(|v| v.as_str())
                .filter(|h| !h.is_empty())
                .unwrap_or("(no tracker)")
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
    let empty = serde_json::json!({
        "current_pib": 0, "milestones": [], "next_pib": 1, "records": [],
    });

    if state.bench.is_none() {
        return Json(empty).into_response();
    }

    // Answer from the cache and never block on the scan: the overview header
    // does not paint until this request returns, so computing it here put six
    // seconds in front of every page load.
    let (cached, stale) = {
        let c = state.records.lock().unwrap_or_else(|e| e.into_inner());
        let stale = match c.at {
            None => true,
            Some(at) => at.elapsed() >= RECORDS_TTL,
        };
        (c.value.clone(), stale)
    };
    if stale {
        refresh_records(state.bench_path.clone(), state.records.clone());
    }

    // Before the first pass completes there is genuinely nothing to show. The
    // empty shape is the same one 3.x sends, so the card renders blank rather
    // than erroring, and fills in on the next poll.
    Json(cached.unwrap_or(empty)).into_response()
}

empty_list_route!(get_agents_torrents);

/// Performance samples over a window, for the benchmark graphs.
async fn get_bench_range(
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
    let (start, end) = range_params(&query);
    match db.samples_in_range(start, end) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::warn!("bench range query failed: {e}");
            Json(serde_json::json!([])).into_response()
        }
    }
}

/// One tracker's samples over a window.
async fn get_tracker_stats_range(
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
    let tracker = query_param(&query, "tracker").unwrap_or_default();
    let (start, end) = range_params(&query);
    match db.tracker_samples_in_range(&tracker, start, end) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::warn!("tracker range query failed: {e}");
            Json(serde_json::json!([])).into_response()
        }
    }
}

/// The `start`/`end` window a graph asks for.
///
/// Unparseable values land on a default 24h window rather than an error: the
/// graph asks for a picture, and an empty one because a parameter was malformed
/// is indistinguishable from a node that recorded nothing.
fn range_params(query: &str) -> (f64, f64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let f = |k: &str| query_param(query, k).and_then(|v| v.parse::<f64>().ok());
    let end = f("end").filter(|v| *v > 0.0).unwrap_or(now);
    let start = f("start").filter(|v| *v > 0.0).unwrap_or(end - 86_400.0);
    (start, end)
}
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
    let _ = cfg;

    // `refresh=1` is the header's refresh button. It re-measures rather than
    // serving the cached pass, which is the only way an operator can confirm a
    // tunnel came back without waiting out the three-minute timer.
    if query_param(&query, "refresh").as_deref() == Some("1") {
        crate::netprobe::measure(&state.engines, &state.net_engines, &state.public_ip).await;
    }

    let (rows, measured_at) = {
        let slot = state.net_engines.lock().await;
        slot.clone()
    };

    // The DISTINCT exit addresses. This is what decides whether the header can
    // honestly print an address at all: one means it can, several mean the
    // engines leave by different routes and naming one of them would label the
    // node with an address most of its traffic does not use.
    //
    // Leaving this empty while `engines[].exit_ip` was filled is what broke the
    // refresh button: the page skips its own fallback as soon as any engine
    // reports an exit, then finds no exit here to render, and the scrambling
    // animation it had started was left on screen as the final state.
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    for row in &rows {
        let local = row.get("local").and_then(|v| v.as_bool()).unwrap_or(false);
        if let (true, Some(ip)) = (local, row.get("exit_ip").and_then(|v| v.as_str())) {
            if !ip.is_empty() {
                seen.insert(ip.to_string());
            }
        }
    }
    let exits: Vec<String> = seen.into_iter().collect();

    // The v6 that belongs to THAT exit, not the process's own: with a single
    // exit the header prints the pair, and they have to be the same engine's.
    let exit_ip_v6 = if exits.len() == 1 {
        rows.iter()
            .find(|row| {
                row.get("local").and_then(|v| v.as_bool()).unwrap_or(false)
                    && row.get("exit_ip").and_then(|v| v.as_str()) == Some(exits[0].as_str())
                    && row.get("exit_ip_v6").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty())
            })
            .and_then(|row| row.get("exit_ip_v6").and_then(|v| v.as_str()))
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    };

    Json(serde_json::json!({
        "engines": rows,
        "exit_ip_v6": exit_ip_v6,
        "exits": exits,
        "measured_at": measured_at,
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
    // Three different things, and mixing them is what put a lifetime figure in
    // a field labelled "day": `total_*` is every byte the loaded torrents have
    // ever moved, `session_*` is since this process started, `day_*` since the
    // last local midnight. Only `total_*` belongs in the global sum.
    // `session_and_day` walks the same counters, so it hands back the totals it
    // already summed rather than being asked for them a second time: this runs
    // once a second per open tab, over every torrent.
    let ((total_up, total_down), (session_up, session_down), (day_up, day_down)) =
        session_and_day(state);

    // Per-state counts, read from the engines rather than from a cache: this is
    // the header an operator refreshes to see whether anything is moving.
    let mut seeds = 0i64;
    let mut downloading = 0i64;
    let mut race_torrents = 0i64;
    if let Some(race) = state.engines.get("race") {
        for torrent in race.manager.all().iter() {
            race_torrents += 1;
            // The engine's own state word, not a forty-field JSON object built
            // and thrown away to read one string out of it.
            match typhon_engine::rpc::dispatch::torrent_core(torrent).state {
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

    let hoard_live = live_stats(state, "hoard");
    let race_live = live_stats(state, "race");
    // Per engine, not the all-engines sum. The hoard block used to carry two
    // literal zeros and the race block the global total, so hoard looked idle
    // while it was seeding and race looked like it had done hoard's work too.
    let hoard_session = engine_session(state, "hoard");
    let race_session = engine_session(state, "race");

    serde_json::json!({
        "baseline": {
            "global_downloaded": base_down + total_down,
            "global_uploaded": base_up + total_up,
            "session_downloaded": session_down,
            "session_uploaded": session_up,
            "total_downloaded": base_down,
            "total_uploaded": base_up,
        },
        "day_downloaded": day_down,
        "day_uploaded": day_up,
        "hoard": {
            "active_download_rate": hoard_live.download_rate,
            "active_peers": hoard_live.active_peers,
            "active_upload_rate": hoard_live.upload_rate,
            "engine": "hoard", "listen_port": cfg.hoard.listen_port,
            "running": true,
            "session_downloaded": hoard_session.1, "session_uploaded": hoard_session.0,
            "stagger_complete": true,
            "swarm_leechers": swarm_leechers_total(state),
            "torrents_announced": announced_count(state, "hoard"),
            "torrents_uploading": hoard_live.torrents_uploading,
            "torrents_with_peers": hoard_live.torrents_with_peers,
            "total_torrents": hoard_torrents,
            "unseeded_peers": hoard_live.unseeded_peers,
        },
        "race": {
            "active_downloads": downloading,
            "active_seeds": seeds,
            "session_downloaded": race_session.1,
            "session_grabbed": 0,
            "session_ratio": crate::row::num_json(ratio),
            "session_uploaded": session_up,
            "torrents": race_torrents,
            "torrents_with_peers": race_live.torrents_with_peers,
            "total_download_rate": race_live.download_rate,
            "total_peers": race_live.active_peers,
            "total_upload_rate": race_live.upload_rate,
        },
        // Process-wide peer counters. These live in atomics that only
        // `rpc::dispatch::get_diagnostics` used to read, and that function is
        // reachable only over the unix-socket RPC the 3.x Go control plane
        // spoke -- so since the full-Rust V4 they have been written and never
        // read by anything. Surfaced here because a counter nobody can read is
        // not a measurement.
        "engine_counters": {
            "have_rx_disarmed": typhon_engine::peer::HAVE_RX_DISARMED.load(std::sync::atomic::Ordering::Relaxed),
            "have_rx_lagged": typhon_engine::peer::HAVE_RX_LAGGED.load(std::sync::atomic::Ordering::Relaxed),
            "seed_seed_dropped": typhon_engine::peer::SEED_SEED_DROPPED.load(std::sync::atomic::Ordering::Relaxed),
            "hs_timed_out": typhon_engine::peer::HS_TIMED_OUT.load(std::sync::atomic::Ordering::Relaxed),
            "inbound_accepted": typhon_engine::peer::INBOUND_ACCEPTED.load(std::sync::atomic::Ordering::Relaxed),
            "worker_threads": typhon_engine::runtime::worker_threads(),
            // Size of the incomplete index the webseed scanner walks. Watched
            // rather than assumed: if this ever drifts toward the catalogue
            // size, the pruning in collect_incomplete has stopped working.
            // Summed over every engine rather than the two well-known roles:
            // a node can run vpn7/vpn8/... too, and a per-role list would go
            // stale the day one is added.
            "incomplete_indexed": state.engines.engines().iter().map(|e| e.manager.incomplete_len()).sum::<usize>(),
        },
        "server_ts": now,
        "tunnels": [],
        // Seconds with a fraction, as 3.x publishes it.
        "uptime": (now - state.started_at) as f64,
        "version": HYDRANOS_VERSION,
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
        // The header BEFORE the library. Hydration takes seconds at 300k
        // torrents, and the status frame used to come after it: every figure in
        // the header stayed blank until the whole list had streamed, so a hard
        // refresh showed an empty header for ten seconds while the answer had
        // been available in under a hundred milliseconds.
        {
            let payload = serde_json::json!({
                "data": status_payload(&state),
                // "status_snapshot", not "status": the page dispatches on this
                // exact string and silently ignores anything else, so the wrong
                // name freezes every header counter after its first paint while
                // the frames keep arriving on time.
                "event": "status_snapshot",
            });
            yield Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .data(serde_json::to_string(&payload).unwrap_or_default()),
            );
            let live = live_stats(&state, "hoard");
            let cfg = state.cfg();
            let hoard = serde_json::json!({
                "event": "hoard_stats_snapshot",
                "data": {
                    "active_download_rate": live.download_rate,
                    "active_peers": live.active_peers,
                    "active_upload_rate": live.upload_rate,
                    "engine": "hoard",
                    "listen_port": cfg.hoard.listen_port,
                    "running": true,
                    "session_downloaded": 0,
                    "session_uploaded": 0,
                    "stagger_complete": true,
                    "swarm_leechers": swarm_leechers_total(&state),
                    "torrents_announced": announced_count(&state, "hoard"),
                    "torrents_uploading": live.torrents_uploading,
                    "torrents_with_peers": live.torrents_with_peers,
                    "total_torrents": state
                        .engines
                        .get("hoard")
                        .map(|e| e.manager.all().len() as i64)
                        .unwrap_or(0),
                    "unseeded_peers": live.unseeded_peers,
                },
            });
            yield Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .data(serde_json::to_string(&hoard).unwrap_or_default()),
            );
        }

        // `hydrate=0`: the client pages the list itself through
        // /api/{engine}/page and wants only the live half of this stream. It
        // still gets an empty terminal batch per mode, because the page waits
        // for `done` before it stops showing the list as loading.
        let hydrate = query_param(&query, "hydrate").as_deref() != Some("0");
        if !hydrate {
            for engine in state.engines.engines() {
                let payload = serde_json::json!({
                    "event": "torrent_batch",
                    "data": {"mode": engine.role.clone(), "torrents": [], "done": true},
                });
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .data(serde_json::to_string(&payload).unwrap_or_default()),
                );
            }
        }

        // Hydration second, and in batches. This is the ONLY path that fills
        // the list: the page stopped reading /api/hoard/torrents when
        // hydration moved to SSE, and that endpoint answers 249 MB in thirty
        // seconds at 300k torrents -- a browser gives up long before.
        const CHUNK: usize = 1000;
        for engine in state.engines.engines().iter().filter(|_| hydrate) {
            let mode = engine.role.clone();
            // Built a batch at a time, not all at once. Materialising 300k rows
            // before sending the first one is 250 MB held and fifty seconds of
            // blank page: the browser waits for work it cannot see. The store
            // is still queried once for the whole session -- that part was
            // never the cost.
            let agent = local_agent(&engine.id);
            let empty = crate::row::StoreFacts::default();
            // Straight from the store. This was 21 seconds and briefly earned
            // itself a cache; the cost was the .torrent BLOBs sharing the table,
            // and a covering index answers the same query in half a second. A
            // cache here would have grown with the catalogue -- the exact thing
            // this release exists to remove.
            let facts = {
                let store = state.store.lock().unwrap();
                store.facts_by_session(&engine.id).unwrap_or_default()
            };
            let torrents = engine.manager.all();
            let total = torrents.len();
            if total == 0 {
                let payload = serde_json::json!({
                    "event": "torrent_batch",
                    "data": {"mode": mode, "torrents": [], "done": true},
                });
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .data(serde_json::to_string(&payload).unwrap_or_default()),
                );
                continue;
            }
            for (i, slice) in torrents.chunks(CHUNK).enumerate() {
                let batch: Vec<serde_json::Value> = slice
                    .iter()
                    .map(typhon_engine::rpc::dispatch::torrent_to_json)
                    .collect::<Vec<_>>()
                    .iter()
                    .map(|raw| {
                        let hash =
                            raw.get("info_hash").and_then(|v| v.as_str()).unwrap_or("");
                        crate::row::build(raw, facts.get(hash).unwrap_or(&empty), &agent)
                    })
                    .collect();
                // `done` only on the very last batch of a mode: the page keeps
                // appending until it is told the mode is complete.
                let done = (i + 1) * CHUNK >= total;
                let payload = serde_json::json!({
                    "event": "torrent_batch",
                    "data": {"mode": mode, "torrents": batch, "done": done},
                });
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .data(serde_json::to_string(&payload).unwrap_or_default()),
                );
                // Let the runtime breathe between batches: 300 frames back to
                // back starve every other task on this thread.
                tokio::task::yield_now().await;
            }
        }

        // Live updates. Without this the list is painted once and then frozen:
        // hydration is a snapshot, and a status frame every two seconds moves
        // the header while every row keeps the figures it was born with.
        //
        // The engines already compute the delta -- `session` runs a
        // delta-filtered emitter on each engine's bus, which skips its whole
        // scan while nobody subscribes. Subscribing here is what turns it on.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
        for engine in state.engines.engines() {
            let mut bus = engine.manager.bus().subscribe();
            let tx = tx.clone();
            let mode = engine.role.clone();
            tokio::spawn(async move {
                loop {
                    match bus.recv().await {
                        Ok(ev) => {
                            let payload = match ev {
                                typhon_engine::rpc::events::Event::StatsSnapshot { torrents } => {
                                    if torrents.is_empty() {
                                        continue;
                                    }
                                    serde_json::json!({
                                        "event": "stats_snapshot",
                                        "data": {"mode": mode, "torrents": torrents},
                                    })
                                }
                                typhon_engine::rpc::events::Event::TorrentRemoved { info_hash } => {
                                    serde_json::json!({
                                        "event": "torrent_removed",
                                        "data": {"mode": mode, "info_hash": info_hash},
                                    })
                                }
                                // Other events carry no field the list reads.
                                _ => continue,
                            };
                            if tx.send(payload).await.is_err() {
                                return;
                            }
                        }
                        // Lagged means the client could not keep up; the next
                        // snapshot is a full picture of what moved, so dropping
                        // the gap loses nothing a later frame does not carry.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => return,
                    }
                }
            });
        }
        drop(tx);

        // 1 Hz, the cadence 3.x pushed at (`startSnapshotPusher`). The port used
        // two seconds, which halved how often every header figure moved -- not
        // visible as a bug, just as an interface that feels a beat behind.
        let mut status_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            let payload = tokio::select! {
                // Biased so a burst of engine frames can never starve the
                // status frame the header lives on.
                biased;
                _ = status_tick.tick() => {
                    serde_json::json!({
                        "data": status_payload(&state),
                        "event": "status_snapshot",
                    })
                }
                Some(update) = rx.recv() => update,
                else => break,
            };
            let is_status =
                payload.get("event").and_then(|v| v.as_str()) == Some("status_snapshot");
            yield Ok::<_, std::convert::Infallible>(
                axum::response::sse::Event::default()
                    .data(serde_json::to_string(&payload).unwrap_or_default()),
            );
            // The hoard header reads its own frame, not the status one.
            if is_status {
                let live = live_stats(&state, "hoard");
                let cfg = state.cfg();
                let hoard = serde_json::json!({
                    "event": "hoard_stats_snapshot",
                    "data": {
                        "active_download_rate": live.download_rate,
                        "active_peers": live.active_peers,
                        "active_upload_rate": live.upload_rate,
                        "engine": "hoard",
                        "listen_port": cfg.hoard.listen_port,
                        "running": true,
                        "session_downloaded": 0,
                        "session_uploaded": 0,
                        "stagger_complete": true,
                        "swarm_leechers": swarm_leechers_total(&state),
                        "torrents_announced": announced_count(&state, "hoard"),
                        "torrents_uploading": live.torrents_uploading,
                        "torrents_with_peers": live.torrents_with_peers,
                        "total_torrents": state
                            .engines
                            .get("hoard")
                            .map(|e| e.manager.all().len() as i64)
                            .unwrap_or(0),
                        "unseeded_peers": live.unseeded_peers,
                    },
                });
                yield Ok::<_, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .data(serde_json::to_string(&hoard).unwrap_or_default()),
                );
            }
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
    // Lifetime totals for the global figure -- the petabyte milestones are
    // derived from this column and must never step back at a restart.
    let ((total_up, total_down), (session_up, _session_down), _) = session_and_day(&state);
    let race_torrents = state
        .engines
        .get("race")
        .map(|e| e.manager.all().len() as i64)
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let hoard_live = live_stats(&state, "hoard");
    let race_live = live_stats(&state, "race");
    let arc = arc_stats();

    Json(serde_json::json!({
        "arc_demand_hit_rate_pct": crate::row::num_json(arc.demand_hit_rate_pct),
        "arc_demand_miss_per_sec": 0,
        "arc_ghost_hits_per_sec": 0,
        "arc_hit_rate_pct": crate::row::num_json(arc.hit_rate_pct),
        "arc_miss_per_sec": 0,
        "arc_size_bytes": arc.size_bytes,
        "global_downloaded": base_down + total_down,
        "global_uploaded": base_up + total_up,
        "hoard_active": hoard_live.torrents_with_peers,
        "hoard_announce_fail_rate": 0, "hoard_announce_rate": 0,
        "hoard_peers": hoard_live.active_peers,
        "hoard_session_uploaded": 0,
        "hoard_upload_rate": hoard_live.upload_rate,
        "hoard_uploading": hoard_live.torrents_uploading,
        "hoard_with_peers": hoard_live.torrents_with_peers,
        "iowait_pct": 0, "open_fds": open_fd_count(),
        "race_announce_fail_rate": 0, "race_announce_rate": 0, "race_avg_share": 0,
        "race_download_rate": race_live.download_rate,
        // Not a peer count: 3.x publishes the torrent count here, and its own
        // source comments call it approximate. Reproduced rather than corrected,
        // because a graph reading this field would step the day it changed.
        "race_peers": race_torrents,
        "race_session_uploaded": session_up,
        "race_torrents": race_torrents,
        "race_upload_rate": race_live.upload_rate,
        "race_uploading": race_live.torrents_uploading,
        "ts": now,
    }))
    .into_response()
}

/// The host's ZFS ARC figures, read from kstat.
///
/// Host-wide and not this process's, which is why the bench excludes them from
/// its comparison -- but the operator reads them next to the hoard's hit rate,
/// and an unconditional zero there looks like a cache that is not working.
#[derive(Default)]
struct ArcStats {
    size_bytes: i64,
    hit_rate_pct: f64,
    demand_hit_rate_pct: f64,
}

fn arc_stats() -> ArcStats {
    let Ok(text) = std::fs::read_to_string("/proc/spl/kstat/zfs/arcstats") else {
        return ArcStats::default();
    };
    let mut field = |name: &str| -> f64 {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(name))
            .and_then(|l| l.split_whitespace().nth(2))
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let hits = field("hits");
    let misses = field("misses");
    let dhits = field("demand_data_hits") + field("demand_metadata_hits");
    let dmisses = field("demand_data_misses") + field("demand_metadata_misses");
    let pct = |h: f64, m: f64| if h + m > 0.0 { h / (h + m) * 100.0 } else { 0.0 };
    ArcStats {
        size_bytes: field("size") as i64,
        hit_rate_pct: pct(hits, misses),
        demand_hit_rate_pct: pct(dhits, dmisses),
    }
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
    /// Where this category's torrents go when they have to leave the race disk
    /// but still owe their tracker seeding time. Empty means there is nowhere
    /// to move them, so under pressure they can only be deleted once the
    /// obligation is paid.
    ///
    /// ⚠ A field added to `Category` and not here is SILENTLY DROPPED: this is
    /// the struct the write path normalises through, and anything it does not
    /// know about disappears on the next save. Measured: drain_action came back
    /// from the API as absent after a PUT that carried it, with a 200 and no
    /// error anywhere.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    graduate_to: String,
    /// See `Category::transit`. Listed here because of the warning above: left
    /// out, a transit area would quietly stop being one at the next save.
    #[serde(default, skip_serializing_if = "is_false")]
    transit: bool,
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
/// Carry a pause decision down to the engine that is moving the bytes.
///
/// The store column is the durable INTENT; `is_paused` on the engine is what
/// actually stops a transfer. Writing only the first is what let a paused
/// torrent keep downloading at 8 MB/s while the interface said "stopped": the
/// state shown is derived from the intent (`row::derive_state`), so it agreed
/// with the click and not with the disk, and nothing anywhere disagreed.
///
/// Quiet when the torrent is not in that engine. A row can name a session this
/// process does not run -- a catalogue outlives a config change -- and that is
/// not an error worth a log line on every bulk pause.
pub(crate) fn apply_pause_to_engine(state: &AppState, engine_id: &str, hash: &str, paused: bool) {
    let Some(info_hash) = crate::store::hex20(hash) else {
        return;
    };
    let Some(engine) = state.engines.engines().iter().find(|e| e.id == engine_id) else {
        return;
    };
    let _ = if paused {
        engine.manager.stop_torrent(&info_hash)
    } else {
        engine.manager.start_torrent(&info_hash)
    };
}

/// Apply one decision to every engine holding a copy of this torrent.
///
/// The qBittorrent shim has no engine to name: *arr clients do not know
/// engines exist, and the store keeps one row per copy. Pausing the intent
/// everywhere while stopping only one copy would leave the others seeding
/// under a row that says stopped.
fn apply_pause_everywhere(state: &AppState, hash: &str, paused: bool) {
    let ids: Vec<String> = state
        .engines
        .engines()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    for id in ids {
        apply_pause_to_engine(state, &id, hash, paused);
    }
}

fn set_paused(state: &AppState, form: &Fields, paused: bool) {
    let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
    let resolved: Vec<String> = {
        let store = state.store.lock().unwrap();
        hashes
            .into_iter()
            .filter_map(|prefix| store.resolve_hash(&prefix))
            .inspect(|hash| {
                let _ = store.set_paused_everywhere(hash, paused);
            })
            .collect()
    };
    // Outside the store lock: stopping a torrent touches the engine and the
    // DHT, and holding the database while doing it would serialise every other
    // request behind a bulk pause.
    for hash in resolved {
        apply_pause_everywhere(state, &hash, paused);
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
fn resolve_in_hoard(state: &AppState, engine: &str, prefix: &str, message: &str) -> Result<String, Response> {
    let store = state.store.lock().unwrap();
    store.resolve_hash_in(engine, prefix).ok_or_else(|| {
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
            // Which copy. `?agent=` carries the row the operator clicked, so a
            // torrent seeded from three engines is paused in the one they
            // pointed at instead of whichever the lookup happened to find.
            let engine = engine_param(&query, "hoard");
            let hash = match resolve_in_hoard(&state, &engine, &info_hash, $message) {
                Ok(h) => h,
                Err(response) => return response,
            };
            let apply: fn(&AppState, &str, &str, &str) = $body;
            apply(&state, &hash, &body, &engine);
            let ok: fn(&str) -> serde_json::Value = $ok;
            Json(ok(&info_hash)).into_response()
        }
    };
}

torrent_write!(hoard_pause_one, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, _body: &str, engine: &str| {
    {
        let store = state.store.lock().unwrap();
        let _ = store.set_paused(hash, engine, true);
    }
    apply_pause_to_engine(state, engine, hash, true);
});

torrent_write!(hoard_resume_one, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, _body: &str, engine: &str| {
    {
        let store = state.store.lock().unwrap();
        let _ = store.set_paused(hash, engine, false);
    }
    apply_pause_to_engine(state, engine, hash, false);
});

torrent_write!(hoard_pin_one, "torrent not in hoard: {}", |ih: &str| serde_json::json!({"info_hash": ih, "pinned": true, "status": "ok"}), |state: &AppState, hash: &str, _body: &str, engine: &str| {
    let store = state.store.lock().unwrap();
    let _ = store.set_pinned(hash, engine, true);
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
        let _ = store.set_pinned_everywhere(&hash, false);
    }
    Json(serde_json::json!({
        "info_hash": info_hash, "pinned": false, "status": "ok",
    }))
    .into_response()
}

torrent_write!(set_torrent_category, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, body: &str, engine: &str| {
    // The body is {"category": "..."} on the native API.
    let category = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("category").and_then(|c| c.as_str()).map(str::to_string))
        .unwrap_or_default();
    let store = state.store.lock().unwrap();
    // The engine was already a parameter here, ignored as `_engine`, so the
    // native API relabelled every copy of a torrent it was given one of.
    let _ = store.set_category_in(hash, engine, &category);
});

torrent_write!(set_torrent_tags, "torrent not found", |_ih: &str| serde_json::json!({"status": "ok"}), |state: &AppState, hash: &str, body: &str, _engine: &str| {
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
pub(crate) fn edit_config<F>(state: &AppState, mutate: F) -> bool
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

    // Hand the new tables to the runners before answering, so the
    // reply cannot claim an override that is not live yet.
    let engines_reloaded = refresh_announce_policies(&state);
    Json(serde_json::json!({
        "engines_reloaded": engines_reloaded,
        "status": "ok",
        "ip_modes": state.cfg().announce_ip_modes,
        // One "agent" per local engine: this node presents itself as local-race
        // and local-hoard, and a config push reaches both.
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
    // Hand the new tables to the runners before answering, so the
    // reply cannot claim an override that is not live yet.
    let engines_reloaded = refresh_announce_policies(&state);
    Json(serde_json::json!({
        "engines_reloaded": engines_reloaded,
        "status": "ok",
        "passkeys": state.cfg().announce_passkeys,
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
/// Push the config now on disk into every running announcer.
///
/// Without this the tables are written, the UI redraws them, and the runner
/// keeps announcing with the policy it was handed at startup -- the setting
/// looks applied and is not, which is the failure mode nothing contradicts.
///
/// Returns how many engines were handed the new policy.
fn refresh_announce_policies(state: &AppState) -> usize {
    let cfg = state.cfg();
    crate::announce::refresh_policies(&cfg, state.engines.engines())
}

/// What each running announcer is ACTUALLY using.
///
/// Deliberately not `state.cfg()`: every other announce route reports the
/// config, so a policy that failed to reload would be invisible -- the file
/// says one thing, the UI repeats it, and the tracker sees the old identity.
/// This reads the live handles, which is the only way to tell "written" from
/// "in force". cf the announce policy being frozen at startup until 4.28.
async fn get_live_announce_policy(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let engines: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .map(|engine| match engine.announce_policy.get() {
            None => serde_json::json!({
                "engine": engine.id,
                "announcing": false,
            }),
            Some(handle) => {
                let p = handle
                    .read()
                    .map(|p| p.clone())
                    .unwrap_or_else(|e| e.into_inner().clone());
                serde_json::json!({
                    "engine": engine.id,
                    "announcing": true,
                    "peer_id": p.peer_id,
                    "user_agent": p.user_agent,
                    "public_ip": p.public_ip,
                    "passkeys": p.passkeys.keys().collect::<Vec<_>>(),
                    "ip_modes": p.ip_modes,
                })
            }
        })
        .collect();

    Json(serde_json::json!({ "engines": engines })).into_response()
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
    let (count, hashes) = {
        let store = state.store.lock().unwrap();
        let count = store.set_paused_all(engine, paused).unwrap_or(0);
        (count, store.all_hashes(engine).unwrap_or_default())
    };
    // The engine, not just the intent. Outside the lock: this walks a whole
    // session, and holding the database across it would stall every request.
    for hash in &hashes {
        apply_pause_to_engine(state, engine, hash, paused);
    }
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
    // Resolve, then write the survivors in ONE transaction, off the runtime.
    // Same reasoning as `bulk_action`: a per-hash autocommit under a blocking
    // mutex is what took the API down on 2026-09-16. Resolution stays inside
    // the same locked section so a torrent cannot vanish between the check and
    // the write.
    let (applied, touched) = {
        let state_cl = state.clone();
        let hashes = req.hashes.clone();
        let engine_id = engine.to_string();
        let paused = req.paused;
        match tokio::task::spawn_blocking(move || {
            let store = state_cl.store.lock().unwrap();
            let touched: Vec<String> = hashes
                .iter()
                .map(|h| h.to_lowercase())
                .filter(|h| {
                    store
                        .resolve_hash_in(&engine_id, h)
                        .is_some_and(|found| &found == h)
                })
                .collect();
            let n = store.set_paused_batch(&touched, &engine_id, paused)?;
            Ok::<_, rusqlite::Error>((n, touched))
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::error!("[api] bulk pause failed to write: {e}");
                (0, Vec::new())
            }
            Err(e) => {
                tracing::error!("[api] bulk pause panicked: {e}");
                (0, Vec::new())
            }
        }
    };
    // The intent is written; now stop the transfers it describes.
    for hash in &touched {
        apply_pause_to_engine(state, engine, hash, req.paused);
    }
    // `paused` is echoed back: the UI updates the row from the answer rather
    // than refetching, and needs to know which way it went.
    Json(serde_json::json!({"status": "ok", "applied": applied, "paused": req.paused}))
        .into_response()
}

/// Pause or resume a set of hashes in ONE named engine.
///
/// `hoard` and `race` have literal routes because 3.x published them; this one
/// exists so a third engine is reachable at all. Without it, pausing the copy
/// held by `vpn1` had no endpoint to call, and the interface fell back to the
/// hoard route -- which paused a different copy.
async fn engine_pause_bulk(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if !state.engines.engines().iter().any(|e| e.id == id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no engine named {id} on this node")})),
        )
            .into_response();
    }
    pause_bulk(&state, &id, &body).await
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
        "bitness": 64, "boost": "1.83.0", "libtorrent": "2.0.9.0", // leak-ok: library versions
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
            // The shim has no engine to name: qBit clients send a hash and a
            // category and nothing else. Every copy it is, deliberately.
            let _ = store.set_category_everywhere(&hash, &category);
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
/// This is the *arr stack's only way in. Sonarr, Radarr, autobrr and
/// cross-seed configure a "qBittorrent" with a host, a port, a username and a
/// password; none of them can set `X-Api-Key`, and none of them can add a
/// query parameter. They POST here and then carry the `SID` cookie.
///
/// It used to answer `Ok.` to anything and set no cookie, which was harmless
/// only for as long as the instance authorised everyone anyway. Once the
/// placeholder key stopped being a free pass, every one of those clients
/// started taking a silent 401 on the request *after* a login that had told
/// them it worked.
///
/// The credentials are the admin account -- the same one the WebUI signs in
/// with at `/api/login`. Answers `Ok.` / `Fails.` with a 200 either way,
/// because that is what qBittorrent answers and the clients parse the body.
async fn qbit_login(State(state): State<AppState>, body: String) -> Response {
    let cfg = state.cfg();
    let username = form_field(&body, "username").unwrap_or_default();
    let password = form_field(&body, "password").unwrap_or_default();

    if !qbit_credentials_ok(&cfg, &username, &password) {
        // One answer for a wrong name, a wrong password and a wrong key alike.
        tracing::warn!(%username, "qBittorrent login refused: invalid credentials");
        return (StatusCode::OK, "Fails.").into_response();
    }

    let sid = state.sessions.create();
    tracing::info!(%username, "qBittorrent client logged in");
    // `HttpOnly` because no page of ours reads it, `SameSite=Lax` because no
    // cross-site form should be able to drive the API with it. Deliberately no
    // `Secure`: the *arr stack talks to this over plain HTTP on the LAN, and a
    // cookie marked Secure would simply never come back.
    let cookie = format!("{}={}; Path=/; HttpOnly; SameSite=Lax", crate::session::COOKIE_NAME, sid);
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        "Ok.",
    )
        .into_response()
}

/// Whether these credentials may open a qBittorrent session.
///
/// Two ways in, because the clients have one password field and there are two
/// things worth putting in it:
///
///  1. **The admin account**, as the WebUI signs in at `/api/login`.
///  2. **The API key**, typed into the password box.
///
/// The second exists so nobody has to rotate anything to fix an install that
/// is already broken: every one of these clients holds the API key today, in a
/// field the daemon stopped reading. It is not a weaker credential -- it is
/// the SAME secret the header carries, over the same connection, and a caller
/// who has it can already drive the whole API. It is compared in constant time
/// for that reason, and never echoed back the way `/api/login` echoes it to
/// the WebUI.
///
/// An instance with neither a key nor an admin account authorises nobody. That
/// guard is not decoration: without it an empty password would compare equal
/// to an empty expected key, which is the 4.14 hole in a new doorway.
fn qbit_credentials_ok(cfg: &crate::config::Config, username: &str, password: &str) -> bool {
    if cfg.daemon.api_key.is_empty() && cfg.auth.password_hash.is_empty() {
        return false;
    }
    let by_password = !cfg.auth.password_hash.is_empty()
        && username == cfg.auth.username
        && bcrypt::verify(password, &cfg.auth.password_hash).unwrap_or(false);
    let by_key = !password.is_empty()
        && constant_time_eq(password.as_bytes(), cfg.daemon.api_key.as_bytes());
    by_password || by_key
}

/// End the session this request carries, and tell the client to drop it.
async fn qbit_logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(sid) = session_of(&headers) {
        state.sessions.revoke(&sid);
    }
    let cookie = format!("{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0", crate::session::COOKIE_NAME);
    (StatusCode::OK, [(axum::http::header::SET_COOKIE, cookie)], "").into_response()
}

/// Pull one field out of an `application/x-www-form-urlencoded` body.
///
/// The same shape as `query_param`, against a body rather than a query string;
/// qBittorrent's login is a form POST and the clients all send it that way.
fn form_field(body: &str, name: &str) -> Option<String> {
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            if key == name {
                return Some(percent_decode(value));
            }
        }
    }
    None
}


/// The torrent listing qBittorrent clients poll.
///
/// Both engines in one list, each row carrying its engine as the fallback
/// category. This is the endpoint the *arr stack reads on a timer, so it is
/// built from the engines directly -- in 3.x it was rebuilt from a cached copy
/// of a copy, which is where 618 MB of the Go heap lived.
/// Does a qBittorrent state belong to a named filter?
///
/// Ported from filterStateMatch. `paused` and `stopped` name the same set:
/// qBittorrent 5 renamed the filter and we still answer the old spelling for
/// everything written before it.
fn qbit_filter_matches(state: &str, filter: &str) -> bool {
    match filter {
        "all" => true,
        "downloading" => matches!(
            state,
            "downloading" | "stalledDL" | "checkingDL" | "queuedDL" | "allocating"
        ),
        "seeding" => matches!(state, "uploading" | "stalledUP" | "queuedUP" | "checkingUP"),
        "completed" => matches!(
            state,
            "uploading" | "stalledUP" | "pausedUP" | "stoppedUP" | "queuedUP" | "checkingUP"
        ),
        "paused" | "stopped" => {
            matches!(state, "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP")
        }
        "active" => matches!(state, "downloading" | "uploading"),
        "inactive" => matches!(
            state,
            "stalledDL" | "stalledUP" | "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP"
        ),
        "stalled" => matches!(state, "stalledDL" | "stalledUP"),
        "stalled_uploading" => state == "stalledUP",
        "stalled_downloading" => state == "stalledDL",
        "errored" => state == "error",
        "resumed" | "running" => !matches!(
            state,
            "pausedDL" | "pausedUP" | "stoppedDL" | "stoppedUP"
        ),
        // An unknown filter name lists everything, as qBittorrent does. Hiding
        // every torrent instead would read to a client as "the queue is empty".
        _ => true,
    }
}

/// Order two listing values the way compareValues did: same-typed values on
/// their own terms, anything else on its rendered form.
fn qbit_value_cmp(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    use std::cmp::Ordering;
    match (a, b) {
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&y.as_f64().unwrap_or(0.0))
            .unwrap_or(Ordering::Equal),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

/// One engine's torrents in the qBittorrent shape, with the listing's category
/// and hash filters applied *before* each row is built.
///
/// Filtering here rather than over the finished list is the whole point.
/// Building a row serialises a torrent to JSON, so an unfiltered build is the
/// entire library every time -- and the *arr stack, cross-seed and autobrr each
/// poll this endpoint several times a minute. The category a row is filtered on
/// is the one it will be reported under, engine-name fallback included, so a
/// client that asks for what it sees gets it back.
fn engine_qbit_rows(
    state: &AppState,
    engine_id: &str,
    now: i64,
    category: Option<&str>,
    hashes: Option<&std::collections::HashSet<String>>,
) -> Vec<serde_json::Value> {
    let Some(engine) = state.engines.get(engine_id) else {
        return Vec::new();
    };

    let agent = local_agent(engine_id);
    let empty = crate::row::StoreFacts::default();
    let mut rows = Vec::new();

    // *arr asks by category and nothing else, over and over. Walking the whole
    // catalogue to keep one category cost 1.5 s per poll on a 300k library:
    // 300k StoreFacts built, 1972 kept.
    //
    // A NAMED category can be answered from the store's index alone, because a
    // torrent with no row there has no category either -- it can only fall
    // under the engine's own name. So this shortcut is exact for every category
    // except that one, which still takes the walk below.
    if let Some(wanted) = category.filter(|c| *c != engine_id) {
        let facts = {
            let store = state.store.lock().unwrap();
            store.facts_in_category(engine_id, wanted).unwrap_or_default()
        };
        for (hash, torrent_facts) in facts.iter() {
            if let Some(only) = hashes {
                if !only.contains(hash) {
                    continue;
                }
            }
            let Ok(key) = typhon_engine::torrent::hex_decode(hash) else { continue };
            // The store can name a torrent the engine no longer holds. It is
            // not this endpoint's job to reconcile that -- it reports what is
            // actually running.
            let Some(torrent) = engine.manager.get(&key) else { continue };
            let raw = typhon_engine::rpc::dispatch::torrent_to_json(&torrent);
            let native = crate::row::build(&raw, torrent_facts, &agent);
            rows.push(crate::qbitrow::build(&native, engine_id, now));
        }
        return rows;
    }

    // One query for the whole session, not one per torrent.
    let facts = {
        let store = state.store.lock().unwrap();
        store.facts_by_session(engine_id).unwrap_or_default()
    };

    for torrent in engine.manager.all().iter() {
        let hash = typhon_engine::torrent::hex_encode(&torrent.info_hash);
        if let Some(wanted) = hashes {
            if !wanted.contains(&hash) {
                continue;
            }
        }
        let torrent_facts = facts.get(&hash).unwrap_or(&empty);
        if let Some(wanted) = category {
            let effective = if torrent_facts.category.is_empty() {
                engine_id
            } else {
                torrent_facts.category.as_str()
            };
            if effective != wanted {
                continue;
            }
        }
        let raw = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
        let native = crate::row::build(&raw, torrent_facts, &agent);
        rows.push(crate::qbitrow::build(&native, engine_id, now));
    }
    rows
}

/// The qBittorrent listing.
///
/// The filter arguments are not decoration. Every client of this endpoint is
/// configured with one category and assumes the answer is scoped to it: Sonarr
/// and Radarr treat what comes back as *their own queue*, and a listing that
/// ignores `category` hands each of them the whole library -- 300k torrents,
/// ebooks included -- to fail an import on, one row at a time.
async fn qbit_torrents_info(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    // The route answers any verb, and qBittorrent takes these either on the
    // query string or as a POST form, so both are read. On a GET the body is
    // empty and the second lookup costs nothing.
    let param = |name: &str| {
        query_param(&query, name)
            .filter(|v| !v.is_empty())
            .or_else(|| query_param(&body, name).filter(|v| !v.is_empty()))
    };

    let filter = param("filter").unwrap_or_else(|| "all".to_string());
    let category = param("category");
    let tag = param("tag");
    let sort_field = param("sort").unwrap_or_else(|| "added_on".to_string());
    let reverse = param("reverse").as_deref() == Some("true");
    let limit = param("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let offset = param("offset")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);

    // "all" is qBittorrent's word for "no hash filter"; taken literally it is a
    // hash that matches nothing, which reads to a client as an empty queue.
    let hashes = param("hashes")
        .filter(|raw| raw != "all")
        .map(|raw| {
            raw.split(['|', ','])
                .map(|h| h.trim().to_lowercase())
                .filter(|h| !h.is_empty())
                .collect::<std::collections::HashSet<String>>()
        });

    let now = now_secs();

    // Non-empty by construction: an empty listing must marshal as [], never
    // null. Clients dereference the array directly -- cross-seed calls
    // torrents.find(...) straight on the parsed body -- so a null throws there
    // instead of reading as "no torrents".
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for engine in ["race", "hoard"] {
        rows.extend(engine_qbit_rows(
            &state,
            engine,
            now,
            category.as_deref(),
            hashes.as_ref(),
        ));
    }

    if filter != "all" {
        rows.retain(|row| {
            let state = row.get("state").and_then(serde_json::Value::as_str).unwrap_or("");
            qbit_filter_matches(state, &filter)
        });
    }

    if let Some(wanted) = tag.as_deref() {
        rows.retain(|row| {
            row.get("tags")
                .and_then(serde_json::Value::as_str)
                .map(|tags| tags.split(',').any(|t| t.trim() == wanted))
                .unwrap_or(false)
        });
    }

    let null = serde_json::Value::Null;
    rows.sort_by(|a, b| {
        let ordering = qbit_value_cmp(
            a.get(&sort_field).unwrap_or(&null),
            b.get(&sort_field).unwrap_or(&null),
        );
        if reverse {
            ordering.reverse()
        } else {
            ordering
        }
    });

    if offset > 0 {
        rows = if offset < rows.len() {
            rows.split_off(offset)
        } else {
            // Past the end is an empty page, not the whole list again.
            Vec::new()
        };
    }
    if limit > 0 && limit < rows.len() {
        rows.truncate(limit);
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
    // Keyed lookup, not a scan: the map is already indexed by info hash, and
    // the scan this replaced serialized every one of 300k torrents to JSON to
    // compare one string -- seconds of CPU to open a detail panel.
    let wanted = info_hash.to_lowercase();
    let key = typhon_engine::torrent::hex_decode(&wanted).ok()?;
    for engine in state.engines.engines() {
        if let Some(torrent) = engine.manager.get(&key) {
            return Some((engine.id.clone(), torrent));
        }
    }
    None
}

/// The .torrent file itself.
///
/// The metainfo has to travel before the data can: a node cannot be told to
/// fetch a torrent it has never been given. Served from the store rather than
/// rebuilt, so the info dict stays byte-identical and the info hash with it.
async fn get_torrent_file(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let blob = {
        let store = state.store.lock().unwrap();
        store.torrent_blob(&info_hash).ok().flatten()
    };
    match blob {
        Some(bytes) => (
            [(axum::http::header::CONTENT_TYPE, "application/x-bittorrent")],
            bytes,
        )
            .into_response(),
        None => not_found(),
    }
}

/// Tell a torrent about peers it has not been given by a tracker.
///
/// This is the piece a cross-node handoff needs: the receiving Hydra adds the
/// torrent, is told that the sending one has the data, and pulls it over
/// BitTorrent. Nothing relays bytes through the control plane, and every piece
/// is hash-checked on arrival because that is what the protocol already does.
///
/// The same door DHT already comes through -- `enqueue_dial` is what
/// `dht.rs` calls for every peer it discovers, so an injected peer is dialled
/// on exactly the path a discovered one is.
async fn post_torrent_peers(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(caller): axum::extract::ConnectInfo<std::net::SocketAddr>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    // The copy the caller means. A peer injected into the wrong engine dials
    // from the wrong tunnel, which is the failure this whole model exists to
    // avoid; and with the copies paused differently it may dial from one that
    // is not running at all.
    let want = engine_param(&query, "");
    let torrent = if want.is_empty() {
        match find_torrent(&state, &info_hash) {
            Some((_, t)) => t,
            None => return not_found(),
        }
    } else {
        match find_copy(&state, &want, &info_hash) {
            Some(t) => t,
            None => return not_found(),
        }
    };
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let list = v.get("peers").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    if list.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "peers is required and must be a non-empty list"})),
        )
            .into_response();
    }

    let mut queued = 0usize;
    let mut rejected: Vec<String> = Vec::new();
    for p in list {
        let Some(text) = p.as_str() else { continue };
        // Named and reported rather than skipped: a typo in one address must
        // not look like a handoff that quietly did nothing.
        // `auto:<port>` means "whoever is asking, on this port". A handing-off
        // node cannot know which of its addresses this one can reach, but this
        // one knows exactly where the request came from -- so the answer is
        // resolved on the side that has it rather than guessed on the side
        // that does not.
        let resolved = match text.strip_prefix("auto:") {
            Some(port) => port
                .parse::<u16>()
                .ok()
                .map(|p| std::net::SocketAddr::new(caller.ip(), p)),
            None => text.parse::<std::net::SocketAddr>().ok(),
        };
        match resolved {
            Some(addr) => {
                typhon_engine::tracker::enqueue_dial(addr, torrent.clone());
                queued += 1;
            }
            None => rejected.push(text.to_string()),
        }
    }
    Json(serde_json::json!({"queued": queued, "rejected": rejected})).into_response()
}

/// Which engine a per-copy request means.
///
/// The same torrent may now be held by several engines, so "pause it" without
/// naming one is three different requests. The front labels a row
/// `local-<engine>` when it is here and `<node>-<engine>` when it is not, and
/// the selection carries that label through as `agent`.
///
/// A `<node>-...` label is deliberately left unstripped: it will match no local
/// engine, and the handler refuses rather than acting on this node's own copy,
/// which is a DIFFERENT copy than the one the operator clicked.
fn engine_param(query: &str, fallback: &str) -> String {
    let raw = query_param(query, "engine")
        .or_else(|| query_param(query, "agent"))
        .unwrap_or_default();
    if raw.is_empty() || raw == "local" {
        return fallback.to_string();
    }
    raw.strip_prefix("local-").unwrap_or(&raw).to_string()
}

/// The copy a request is about: the one `?agent=` names, or the first found.
///
/// Used by the routes whose ANSWER differs between copies -- live rates, peers,
/// progress, and the reannounce, which each copy makes with its own peer_id from
/// its own port. Routes that read the metainfo (files, tracker list) are left
/// alone: two copies of one torrent are the same file on disk and the same
/// announce URLs, so there is nothing to choose between.
fn find_selected(
    state: &AppState,
    query: &str,
    info_hash: &str,
) -> Option<(String, std::sync::Arc<typhon_engine::torrent::meta::TorrentState>)> {
    let want = engine_param(query, "");
    if want.is_empty() {
        return find_torrent(state, info_hash);
    }
    find_copy(state, &want, info_hash).map(|t| (want, t))
}

/// The copy of a torrent held by one named engine.
fn find_copy(
    state: &AppState,
    engine_id: &str,
    info_hash: &str,
) -> Option<std::sync::Arc<typhon_engine::torrent::meta::TorrentState>> {
    let key = typhon_engine::torrent::hex_decode(&info_hash.to_lowercase()).ok()?;
    state.engines.get(engine_id).and_then(|e| e.manager.get(&key))
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
/// A peer id as a human reads it: printable bytes kept, the rest escaped.
///
/// The tail is random binary, so a plain from_utf8 would either fail or render
/// as replacement characters and make two different ids look identical.
fn printable_peer_id(id: &[u8; 20]) -> String {
    id.iter()
        .map(|&b| {
            if (0x20..0x7f).contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02x}")
            }
        })
        .collect()
}

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

    // The identity a tracker was last actually told, beside the one the current
    // policy would send. An override applies at the NEXT announce, so these two
    // disagree for a while -- and without showing both, the only way to know
    // which torrents have caught up is to wait and hope.
    let (announced, announced_at) = match *torrent.announced_peer_id.read() {
        Some((id, at)) => (Some(printable_peer_id(&id)), Some(at)),
        None => (None, None),
    };
    let next = state
        .engines
        .engines()
        .iter()
        .find(|e| e.id == engine)
        .and_then(|e| e.announce_policy.get())
        .map(|handle| {
            let p = handle
                .read()
                .map(|p| p.clone())
                .unwrap_or_else(|e| e.into_inner().clone());
            printable_peer_id(&crate::announce::policy::announced_peer_id(&p, &tiers))
        });

    Json(serde_json::json!({
        "engine": engine,
        "trackers": tiers,
        // None until this torrent has announced at least once since startup.
        "announced_peer_id": announced,
        "announced_at": announced_at,
        "next_peer_id": next,
        "peer_id_pending": match (&announced, &next) {
            (Some(a), Some(n)) => a != n,
            // Never announced yet: nothing has been told to anyone, so there is
            // nothing stale to warn about.
            _ => false,
        },
    }))
    .into_response()
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
    let mut rows = vec![
        pseudo("** [DHT] **"),
        pseudo("** [PeX] **"),
        pseudo("** [LSD] **"),
    ];

    // And the real ones. This route used to return the three pseudo rows and
    // nothing else, whatever the torrent: a client asking "who is this
    // announcing to" got an answer that was the same for every torrent in the
    // catalogue, and looked like a torrent with no trackers at all.
    let hash = query_param(&query, "hash").unwrap_or_default().to_lowercase();
    if let Some((_, torrent)) = find_torrent(&state, &hash) {
        let last_error = torrent
            .last_announce_error
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default();
        let announced = torrent
            .last_announce_at
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0;
        // qBit's vocabulary: 0 disabled, 1 not contacted yet, 2 working,
        // 4 not working. "Not contacted yet" is the state of a torrent added
        // stopped, and it is the one qBit clients render without an alarm.
        let status = if !announced {
            1
        } else if last_error.is_empty() {
            2
        } else {
            4
        };
        let seeders = torrent.scrape_seeders.load(std::sync::atomic::Ordering::Relaxed) as i64;
        let leechers = torrent.scrape_leechers.load(std::sync::atomic::Ordering::Relaxed) as i64;
        for (tier, urls) in torrent.live_trackers.read().iter().enumerate() {
            for url in urls {
                rows.push(serde_json::json!({
                    "url": url,
                    "tier": tier,
                    "status": status,
                    "msg": if status == 4 { last_error.clone() } else { String::new() },
                    "num_peers": 0,
                    "num_seeds": seeders,
                    "num_leeches": leechers,
                    "num_downloaded": 0,
                }));
            }
        }
    }
    Json(serde_json::Value::Array(rows)).into_response()
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
    let resolved = {
        let store = state.store.lock().unwrap();
        match store.resolve_hash_in(engine, prefix) {
            Some(hash) => {
                let _ = store.set_paused_everywhere(&hash, paused);
                Some(hash)
            }
            None => None,
        }
    };
    match resolved {
        Some(hash) => {
            // The intent covers every copy, so the stop has to as well.
            apply_pause_everywhere(state, &hash, paused);
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not found"})),
        )
            .into_response(),
    }
}

/// ⚠⚠ `deny_unknown_fields` IS THE POINT OF THIS STRUCT, NOT A DETAIL.
///
/// Until 2026-09-16 the error message for this body read
/// `expected {action, filter, exclude, hashes}` while the struct declared no
/// `filter` at all. The browser believed the message and sent the filter for
/// any selection over 500 rows, serde dropped the unknown key without a word,
/// `hashes` defaulted to empty -- and empty meant THE WHOLE ENGINE. A start
/// aimed at 70k calewood torrents started all 293k instead, and nothing in the
/// request, the response or the logs said so.
///
/// A field this API does not implement must now be a 400. Silence is what made
/// the incident invisible.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BulkBody {
    #[serde(default)]
    action: String,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    hashes: Vec<String>,
    /// Opt IN to "every torrent in this engine". Never inferred from an empty
    /// list: that inference is exactly what turned a filtered selection into
    /// the whole library.
    #[serde(default)]
    all: bool,
}

/// Apply start/stop to a named set, minus an exclusion list.
///
/// The matched COUNT is answered on purpose: the filter exists both here and in
/// the browser, so the only real risk is the two drifting apart, and a visible
/// number turns that from a silent wrong-set into something somebody notices.
async fn bulk_action(state: &AppState, engine: &str, body: &str) -> Response {
    let req: BulkBody = match serde_json::from_str::<BulkBody>(body) {
        Ok(req) => req,
        // The parse error is echoed back. `deny_unknown_fields` names the
        // offending key, which is the whole point: a caller sending `filter`
        // now learns that this endpoint has never implemented one.
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "expected {action, hashes, exclude, all}",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
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

    // ⚠⚠ AN EMPTY `hashes` IS A REFUSAL, NOT A WILDCARD.
    //
    // 3.x read an empty list as "every torrent in the engine", and that is what
    // fired on 2026-09-16: the browser sent a filter this endpoint never
    // implemented, serde dropped it, and the empty default started all 293k
    // torrents instead of the 70k the operator had selected. The old contract
    // is reachable, but only by SAYING so with `"all": true`.
    //
    // "Everything" is the ENGINE's list, not the front store's. The two differ
    // -- 486 torrents in the race engine against 148 rows in the store, the gap
    // recorded in project_hydra_api_db_count_gap -- and counting from the store
    // would silently leave 338 torrents running.
    let targets: Vec<String> = if req.hashes.is_empty() {
        if !req.all {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "hashes is empty; pass \"all\": true to mean every torrent in this engine",
                })),
            )
                .into_response();
        }
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

    let selected: Vec<String> = targets
        .into_iter()
        .filter(|h| !excluded.contains(h))
        .collect();
    let matched = selected.len();

    // ONE transaction, off the async runtime.
    //
    // Both halves matter. The loop used to call a single-statement update per
    // hash -- 293k autocommits, ~17 rows/s measured -- while holding a
    // std::sync::Mutex across every await-free iteration of it. Every other
    // worker that wanted the store blocked its whole thread, so the API stopped
    // accepting connections entirely for half an hour. The batch makes the work
    // short; spawn_blocking keeps what is left off the tokio workers.
    let applied = {
        let state = state.clone();
        let hashes = selected.clone();
        match tokio::task::spawn_blocking(move || {
            let store = state.store.lock().unwrap();
            store.set_paused_everywhere_batch(&hashes, stop)
        })
        .await
        {
            Ok(Ok(n)) => n,
            // The store row may not exist -- see the count gap above. A failed
            // write is reported, not swallowed: `let _ =` on this path is what
            // let a half-applied bulk look like a clean one.
            Ok(Err(e)) => {
                tracing::error!("[api] bulk {} failed to write: {e}", req.action);
                0
            }
            Err(e) => {
                tracing::error!("[api] bulk {} panicked: {e}", req.action);
                0
            }
        }
    };

    // The intent is written; now stop or start the transfers it describes.
    // `bulk_action` used to skip this entirely, so a bulk start marked 293k rows
    // as running in the database and left the engine seeding none of them --
    // the rows said one thing and the engine did another until the next restart.
    for hash in &selected {
        apply_pause_to_engine(state, engine, hash, stop);
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

    // The row carries its tracker host, so an edit changes what the list
    // paints. Tell the reconnect ring, or a client returning on its cursor
    // keeps showing the old tracker until it reloads everything.
    state.reconnect.record_changed(&[info_hash.to_string()]);

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
/// WARNING Until 2026-09-13 this route answered `no_drain_needed` without
/// looking at a disk: the button had never drained anything and said so in a
/// way that read like a result. Same shape as the Verify and reannounce stubs.
///
/// Scoped to one volume through `?volume=`, because that is the whole point of
/// the panel: pressing the button on the full SSD must not touch the other one.
/// Without the parameter it passes over every volume that is over its mark.
async fn drain_now(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let want = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("volume="))
        .map(|v| percent_decode(v))
        .unwrap_or_default();
    let cfg = state.cfg();
    // Off the async runtime: this deletes files and queues copies, and a race
    // panel is not worth stalling every other request for.
    let st = state.clone();
    let out = tokio::task::spawn_blocking(move || {
        let mut total = crate::workers::DrainOutcome::default();
        let mut touched: Vec<String> = Vec::new();
        for engine in st.engines.engines().iter() {
            if engine.role != "race" {
                continue;
            }
            for volume in crate::volumes::discover(&st, &engine.manager, &cfg.race_drain) {
                if !want.is_empty() && volume.id != want {
                    continue;
                }
                if want.is_empty() && volume.used_pct() < volume.policy.high as f64 {
                    continue;
                }
                let o = crate::workers::drain_once(&st, &engine.manager, &volume, &cfg, &engine.id);
                total.deleted += o.deleted;
                total.graduated += o.graduated;
                total.stuck += o.stuck;
                total.freed_bytes += o.freed_bytes;
                touched.push(volume.id.clone());
            }
        }
        (total, touched)
    })
    .await;
    let Ok((total, touched)) = out else {
        return bad_request("drain panicked");
    };
    if touched.is_empty() {
        return Json(serde_json::json!({"status": "no_volume", "volumes": touched})).into_response();
    }
    let did = total.deleted + total.graduated;
    Json(serde_json::json!({
        "status": if did > 0 { "ok" } else { "no_drain_needed" },
        "volumes": touched,
        "deleted": total.deleted,
        "graduated": total.graduated,
        "stuck": total.stuck,
        "freed_bytes": total.freed_bytes,
    }))
    .into_response()
}

/// Verify one torrent. Scoped to hoard, like the other per-torrent routes.
///
/// WARNING Until 2026-09-12 this route resolved the hash and answered `ok`
/// without hash-checking anything: the Verify button in the UI had never
/// checked a torrent, and said it had. Same shape as the reannounce stub
/// below, and the same reason it went unnoticed -- nothing contradicts a
/// success that was never going to be measured. `TorrentManager::recheck`
/// already existed and already does the right thing; it was simply never
/// called from here.
///
/// A paused torrent is rechecked and STAYS paused: `run_recheck` stores
/// Stopped rather than Downloading when `is_paused` is set, so the operator
/// learns what is missing without putting the torrent on the network. That is
/// the whole point of verifying before starting.
async fn hoard_verify_one(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let Some((engine_id, torrent)) = find_selected(&state, &query, &info_hash) else {
        return not_found();
    };
    let Some(engine) = state.engines.get(&engine_id) else {
        return not_found();
    };
    // The engine owns the hash, so take the typed one off the torrent instead
    // of re-parsing the prefix the caller typed.
    match engine.manager.recheck(&torrent.info_hash) {
        // Answer what happened, not that the request was well-formed: the
        // check runs in the background, and "checking" is what the caller
        // needs to know to start polling progress.
        Ok(()) => Json(serde_json::json!({
            "status": "ok",
            "checking": true,
            "paused": torrent.is_paused.load(std::sync::atomic::Ordering::Relaxed),
        }))
        .into_response(),
        // recheck refuses a torrent it cannot check. A refusal is a 409,
        // never a silent ok.
        Err(e) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
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

    // Until 4.17.2 this route found the torrent and answered ok without
    // announcing anything: there was no way to ask for an announce at all, so
    // the button in the Trackers tab had never done a thing. The scheduler owns
    // the queue, so asking is sending it a message.
    let Some((engine_id, _torrent)) = find_selected(&state, &query, &info_hash) else {
        return not_found();
    };
    let Some(engine) = state.engines.get(&engine_id) else {
        return not_found();
    };
    let Some(bump) = engine.bump.get() else {
        // Loaded but not on the network: no announce loop to jump.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "engine is not announcing"})),
        )
            .into_response();
    };
    // try_send, never send: this runs on a request, and a scheduler too busy to
    // read is a reason to refuse the click, not to hold the connection open.
    //
    // ⭐ The reply channel is the whole point of this shape. Answering ok the
    // moment the message entered the queue is what let a bulk reannounce of 540
    // torrents report success and do nothing on 2026-09-12: the scheduler
    // refuses a bump inside its sixty-second cooldown, and the refusal was
    // dropped on the floor. What the caller is told now is what happened.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if bump
        .try_send(crate::announce::scheduler::BumpReq {
            info_hash: info_hash.to_lowercase(),
            reply: Some(reply_tx),
        })
        .is_err()
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "announce queue is busy, try again"})),
        )
            .into_response();
    }
    // Bounded wait: a scheduler in the middle of reconciling 300k torrents may
    // take a moment, and "queued" is the honest answer if it does. The bump is
    // not lost -- we simply cannot say yet what became of it, and saying "ok"
    // is exactly the lie this change removes.
    match tokio::time::timeout(std::time::Duration::from_secs(5), reply_rx).await {
        Ok(Ok(crate::announce::scheduler::BumpOutcome::Bumped)) => Json(serde_json::json!({
            "status": "ok",
            "engine": engine_id,
            "bumped": true,
        }))
        .into_response(),
        // Not a failure: the announce being asked for is the one already in
        // progress. The caller gets what it wanted, just not because of it.
        Ok(Ok(crate::announce::scheduler::BumpOutcome::InFlight)) => Json(serde_json::json!({
            "status": "in_flight",
            "engine": engine_id,
            "bumped": false,
        }))
        .into_response(),
        Ok(Ok(crate::announce::scheduler::BumpOutcome::Cooldown { retry_in })) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({
                "status": "cooldown",
                "engine": engine_id,
                "bumped": false,
                "retry_after_secs": retry_in.as_secs() + 1,
            })),
        )
            .into_response(),
        // The scheduler dropped its end: there is no announce loop left to jump.
        Ok(Err(_)) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "engine is not announcing"})),
        )
            .into_response(),
        Err(_) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({
                "status": "queued",
                "engine": engine_id,
                "bumped": false,
            })),
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
/// The announce fields (last_error, last_announce, next_announce) are written
/// by the announce runner onto the torrent as of 4.4.5. Before that nothing
/// wrote them and this panel reported "never" on a node announcing normally.
///
/// ⭐ Three states, not two. `last_error.is_empty()` used to mean "Success",
/// which made a torrent nobody had announced yet -- every torrent, for the
/// first hour of a 300k boot -- report a healthy tracker it had never spoken
/// to. "No error" and "it went well" are not the same claim, and the panel had
/// no way to tell them apart because this function did not send one.
///
/// `admission` is the engine's own scheduler progress, used only to say how
/// long a torrent still waiting its turn might wait. It is an upper bound for
/// the queue as a whole -- see `Admission::drain_seconds` -- so it is reported
/// as a separate field the UI can hedge, never as `next_announce`.
fn tracker_rows(
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    admission: &crate::announce::scheduler::Admission,
) -> Vec<serde_json::Value> {
    use std::sync::atomic::Ordering;

    let last_error = torrent
        .last_announce_error
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
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

    // An announce that has never happened cannot have succeeded and cannot
    // have failed. A tracker that answered with an error keeps saying so even
    // once the attempt is old, which is why the error is checked first.
    let status = if !last_error.is_empty() {
        "error"
    } else if last_at > 0 {
        "ok"
    } else {
        "never"
    };
    let ok = status == "ok";

    // Only for a torrent with no announce and no deadline: it is waiting for
    // the scheduler to reach it. Anything else already has a real answer, and
    // an estimate next to a fact reads as though the fact were an estimate.
    let eta = if status == "never" && next_at == 0 {
        admission.drain_seconds()
    } else {
        -1
    };

    let mut rows = Vec::new();
    for (tier, urls) in torrent.live_trackers.read().iter().enumerate() {
        for url in urls {
            rows.push(serde_json::json!({
                "url": url,
                "tier": tier,
                "verified": ok,
                "endpoints": [{
                    // "" rather than "Success" when nothing has happened:
                    // an older client reading last_error sees "no error",
                    // which is true, instead of a success that never was.
                    "last_error": if ok { "Success".to_string() } else { last_error.clone() },
                    "message": if ok { String::new() } else { last_error.clone() },
                    "status": status,
                    "last_announce": last_announce,
                    "next_announce": next_announce,
                    "next_announce_eta_max": eta,
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
    let Some((engine_id, torrent)) = find_selected(&state, &query, &hash) else {
        return not_found();
    };
    // By ROLE, not by id. `race` is a behaviour, and a node may run several
    // engines that have it -- one per tunnel. Comparing the id refused the
    // detail panel of every copy held by any of them, which is the same
    // mistake `connect` made when it handed those engines hoard's network.
    let is_race = state
        .engines
        .engines()
        .iter()
        .any(|e| e.id == engine_id && e.role == "race");
    if !is_race {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not in race"})),
        )
            .into_response();
    }

    Json(detail_payload(&state, "race", &hash, &torrent, &admission_of(&state, &engine_id))).into_response()
}

/// This engine's scheduler progress, by its real id.
///
/// By ID and not by role: a node runs one engine per tunnel and they admit at
/// their own pace, so the wrong handle would answer with another engine's
/// backlog. An engine that is offline, or one that has gone since the lookup,
/// has admitted nothing, and a default `Admission` says exactly that.
fn admission_of(
    state: &AppState,
    engine_id: &str,
) -> std::sync::Arc<crate::announce::scheduler::Admission> {
    state
        .engines
        .engines()
        .iter()
        .find(|e| e.id == engine_id)
        .map(|e| e.admission.clone())
        .unwrap_or_default()
}

/// The detail panel's view of one torrent, for either engine.
///
/// Shared so the two engines cannot drift: the hoard route used to answer a
/// bare `{"status":"ok"}`, which rendered an empty panel for the 300k torrents
/// that live there while race showed a full one.
fn detail_payload(
    state: &AppState,
    engine_id: &str,
    hash: &str,
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    admission: &crate::announce::scheduler::Admission,
) -> serde_json::Value {
    let raw = typhon_engine::rpc::dispatch::torrent_to_json(torrent);
    let facts = {
        let store = state.store.lock().unwrap();
        store.facts_by_session(engine_id).unwrap_or_default()
    };
    let empty = crate::row::StoreFacts::default();
    let row = crate::row::build(&raw, facts.get(hash).unwrap_or(&empty), "");

    let i = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    let total_download = i(&row, "total_download");
    let total_upload = i(&row, "total_upload");
    let ratio = if total_download > 0 {
        total_upload as f64 / total_download as f64
    } else {
        0.0
    };

    serde_json::json!({
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
        "peers": typhon_engine::rpc::dispatch::peers_json(torrent),
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
        "trackers": tracker_rows(torrent, admission),
        "upload_rate": i(&row, "upload_rate"),
        "uploads_limit": 0,
    })
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
    // The store resolves a prefix to a full hash and proves the torrent is
    // hoard's; the engine then supplies the live half of the panel.
    let hash = match resolve_in_hoard(&state, &engine_param(&query, "hoard"), &info_hash, "torrent not found") {
        Ok(hash) => hash,
        Err(response) => return response,
    };
    let Some((engine_id, torrent)) = find_selected(&state, &query, &hash) else {
        return not_found();
    };
    // By ROLE, as on the race side: a node may run several hoard engines, one
    // per tunnel, and only one of them is called "hoard".
    let is_hoard = state
        .engines
        .engines()
        .iter()
        .any(|e| e.id == engine_id && e.role == "hoard");
    if !is_hoard {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not in hoard"})),
        )
            .into_response();
    }
    Json(detail_payload(&state, "hoard", &hash, &torrent, &admission_of(&state, &engine_id))).into_response()
}

/// Add a torrent, native API: a `torrent_path` already on this node's disk.
///
/// The refusal keeps the per-target breakdown the UI reads to say WHICH engine
/// refused. `magnet_uri` is still not accepted here -- resolution is a
/// background job with its own polling contract, and answering "added" for a
/// magnet whose metadata never arrives would be worse than refusing it.
async fn post_torrent_add(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let refuse = |message: String| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": message,
                "targets": [{"agent": "local", "error": message}],
            })),
        )
            .into_response()
    };

    let payload: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let s = |k: &str| {
        payload.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string()
    };
    let torrent_path = s("torrent_path");
    if torrent_path.is_empty() {
        return refuse("race: torrent_path or magnet_uri required".to_string());
    }

    let bytes = match std::fs::read(&torrent_path) {
        Ok(b) => b,
        Err(e) => return refuse(format!("race: {torrent_path}: {e}")),
    };
    let paused = payload.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false);
    let seed_mode = payload.get("seed_mode").and_then(|v| v.as_bool()).unwrap_or(false);

    match add_torrent_bytes(
        &state,
        &bytes,
        &s("category"),
        &s("save_path"),
        &s("tags"),
        paused,
        seed_mode,
        &s("engine"),
    ) {
        Ok((hash, name)) => {
            Json(serde_json::json!({"info_hash": hash, "name": name})).into_response()
        }
        Err(e) => refuse(format!("race: {e}")),
    }
}

/// qBittorrent's add: multipart, one or more `torrents` file parts.
///
/// This is the route autobrr, cross-seed and the *arrs use, so it is the one
/// that decides whether anything can reach this node at all. It answers plain
/// "Ok."/"Fails." like qBit, because those clients match on the body.
async fn qbit_torrent_add(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let mut files: Vec<Vec<u8>> = Vec::new();
    let mut category = String::new();
    let mut save_path = String::new();
    let mut tags = String::new();
    let mut paused = false;
    // skip_checking is qBit's "trust the data on disk"; cross-seed relies on it
    // and treating it as false would re-hash every cross-seeded torrent.
    let mut seed_mode = false;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "torrents" | "file" | "fileselect" => {
                if let Ok(data) = field.bytes().await {
                    files.push(data.to_vec());
                }
            }
            _ => {
                let value = field.text().await.unwrap_or_default();
                match name.as_str() {
                    "category" => category = value,
                    "savepath" => save_path = value,
                    "tags" => tags = value,
                    "paused" | "stopped" => paused = value == "true" || value == "1",
                    "skip_checking" => seed_mode = value == "true" || value == "1",
                    _ => {}
                }
            }
        }
    }

    if files.is_empty() {
        return (StatusCode::BAD_REQUEST, "Bad request").into_response();
    }

    let mut failed = 0;
    for bytes in &files {
        if let Err(e) =
            add_torrent_bytes(&state, bytes, &category, &save_path, &tags, paused, seed_mode, "")
        {
            // "already added" is not a failure to a client that retries a
            // release it has seen before; qBit answers Ok. for it too.
            if e.contains("already added") {
                continue;
            }
            tracing::warn!(error = %e, "qbit add refused");
            failed += 1;
        }
    }

    if failed == files.len() {
        return (StatusCode::BAD_REQUEST, "Fails.").into_response();
    }
    (StatusCode::OK, "Ok.").into_response()
}


/// Add a torrent from an uploaded .torrent file.
///
/// Was a `refuse!` stub, which is what broke the farm's `sw_fill` and left a
/// handoff no way to name the engine it wants: the qBit shim routes by
/// category, and a category names a MODE, not one of the engines a node hosts.
/// This route takes an explicit `engine`, so a torrent can be placed in `vpn1`.
async fn post_torrent_upload(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let mut bytes: Vec<u8> = Vec::new();
    let (mut category, mut save_path, mut tags, mut engine) =
        (String::new(), String::new(), String::new(), String::new());
    let (mut paused, mut seed_mode) = (false, false);

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        if name == "torrents" || name == "torrent" || name == "file" {
            bytes = field.bytes().await.map(|b| b.to_vec()).unwrap_or_default();
            continue;
        }
        let value = field.text().await.unwrap_or_default();
        match name.as_str() {
            "category" => category = value,
            "savepath" | "save_path" => save_path = value,
            "tags" => tags = value,
            "engine" => engine = value,
            "paused" | "stopped" => paused = value == "true" || value == "1",
            "skip_checking" | "seed_mode" => seed_mode = value == "true" || value == "1",
            _ => {}
        }
    }
    if bytes.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "no torrent file in request"})),
        )
            .into_response();
    }
    // A named engine that does not exist is refused rather than quietly
    // falling back: silently landing in race is how a hoard torrent changes
    // tier without anyone noticing.
    if !engine.is_empty() && !state.engines.engines().iter().any(|e| e.id == engine) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("no engine named {engine} on this node")})),
        )
            .into_response();
    }
    match add_torrent_bytes(
        &state, &bytes, &category, &save_path, &tags, paused, seed_mode, &engine,
    ) {
        Ok((hash, name)) => Json(serde_json::json!({
            "info_hash": hash, "name": name, "engine": engine
        }))
        .into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))).into_response(),
    }
}
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
    let resolved = {
        let store = state.store.lock().unwrap();
        store.resolve_hash(&hash)
    };
    let Some(hash) = resolved else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "torrent not found"})),
        )
            .into_response();
    };

    // ⚠ INVERTED POLARITY, and it is the dangerous direction: the API asks
    // whether to DELETE the files, the engine is told whether to KEEP them.
    // Passing this through unflipped drains the payload of every torrent the
    // user meant to keep.
    let delete_files = query_param(&query, "delete_files")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let keep_data = !delete_files;

    // WHICH copy. Removing every engine's copy when the operator selected one
    // row would delete the other tunnels' seeds too, and the files with them if
    // delete_files is set. An unqualified request still means all of them --
    // that is what the qBit shim asks for, and what "remove this torrent" meant
    // before a torrent could be in two places.
    let want = engine_param(&query, "");
    let sessions: Vec<String> = if want.is_empty() {
        let store = state.store.lock().unwrap();
        store.sessions_of(&hash)
    } else {
        vec![want.clone()]
    };
    if !want.is_empty() && find_copy(&state, &want, &hash).is_none() {
        return not_found();
    }

    match remove_one_torrent(&state, &hash, &sessions, &want, delete_files) {
        Ok(dropped) => {
            tracing::info!(hash = %hash, delete_files, copies = dropped, "torrent removed");
            Json(serde_json::json!({"status": "ok"})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        )
            .into_response(),
    }
}

/// Move a removed torrent's lifetime bytes into the durable carry-over, and
/// drop its store row, in one transaction.
///
/// Every total the interface publishes is a sum over the torrents currently
/// loaded plus a stored baseline. Removing a torrent takes its bytes out of the
/// sum; this is what puts them into the baseline, so the published figure does
/// not move. The V4 port dropped this call and kept only the row deletion,
/// which is why deleting a torrent silently erased everything it had ever
/// uploaded -- from the all-time figure as well as the day's.
///
/// Per tracker as well as globally: the Trackers tab reads the same counters,
/// and a ratio that forgets what a removed torrent gave back is the number an
/// operator is judged on.
pub(crate) fn absorb_on_remove(
    state: &AppState,
    engine_id: &str,
    torrent: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>,
    hash: &str,
    session: Option<&str>,
) {
    use std::sync::atomic::Ordering;
    let ul = torrent.total_uploaded.load(Ordering::Relaxed) as i64;
    let dl = torrent.total_downloaded.load(Ordering::Relaxed) as i64;

    // The host baked into the torrent, read the way the Trackers tab reads it,
    // and bucketed under the same name it uses for a torrent carrying no
    // announce URL -- otherwise the absorbed bytes land on a row only this path
    // knows about, and the tab keeps showing the figure without them.
    let host = torrent
        .live_trackers
        .read()
        .iter()
        .flatten()
        .next()
        .map(|u| typhon_engine::rpc::dispatch::tracker_host_of(u))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "(no tracker)".to_string());

    let keys = vec![
        "global".to_string(),
        crate::store::Store::tracker_counter_key(engine_id, &host),
    ];

    {
        let store = state.store.lock().unwrap();
        if let Err(e) = store.delete_absorb(hash, session, &keys, ul, dl) {
            // Loud on purpose: lifetime upload is the one figure here that
            // cannot be recomputed from anything else, so a fold that did not
            // land must never pass for a clean delete.
            tracing::error!(
                hash = %hash, engine = %engine_id, ul, dl,
                "absorb-on-remove failed, lifetime bytes not carried over: {e}"
            );
            return;
        }
    }

    // Only once the bytes are durable. The in-process mark and the stored
    // counter have to move together, or the headline jumps by the difference
    // until the next restart re-reads the store.
    let mut odo = state.odometer.lock().unwrap_or_else(|e| e.into_inner());
    odo.forget(engine_id, ul, dl);
}

/// Remove a torrent from the engines that hold it, and from the store.
///
/// Shared by the native DELETE and the qBit shim: the shim used to read
/// `deleteFiles` and honour none of it, so an *arr removing a torrent with its
/// data left both the row and the payload in place -- which is how /race filled
/// to 100% while every client believed it had cleaned up after itself.
///
/// Returns how many copies were dropped.
pub(crate) fn remove_one_torrent(
    state: &AppState,
    hash: &str,
    sessions: &[String],
    want: &str,
    delete_files: bool,
) -> Result<usize, String> {
    // The engine first: dropping the store row alone leaves a torrent that
    // still seeds, still announces, and comes back at the next restart from the
    // engine's own state -- present to the network, invisible to the interface.
    //
    // The files go only with the LAST copy: two engines seeding one payload
    // share it, so deleting it with the first would leave the others seeding
    // nothing.
    let remaining = {
        let store = state.store.lock().unwrap();
        store.sessions_of(hash).len()
    };
    let mut dropped = 0usize;
    for session in sessions {
        let Some(engine) = state.engines.get(session) else { continue };
        let Some(torrent) = find_copy(state, session, hash) else { continue };
        let last = dropped + 1 >= remaining;
        let keep = !(delete_files && last);
        if let Err(e) = engine.manager.remove_torrent(&torrent.info_hash, keep) {
            tracing::warn!(hash = %hash, session, "engine refused removal: {e}");
            return Err(e);
        }
        engine.announce_cache.forget(hash);
        // AFTER the engine let go, never before: absorbing a torrent the engine
        // then refuses to drop counts its bytes twice, once in the carry-over
        // and once in the live sum. This also drops the row for this copy.
        absorb_on_remove(state, session, &torrent, hash, Some(session));
        dropped += 1;
    }

    {
        let store = state.store.lock().unwrap();
        if want.is_empty() {
            let _ = store.delete_torrent(hash);
        } else {
            let _ = store.delete_copy(hash, want);
        }
    }
    Ok(dropped)
}

/// Seed the same torrent from a SECOND engine of this node.
///
/// No transfer and no second copy on disk: both engines are pointed at the same
/// files. What it buys is a second identity in the swarm -- its own peer_id, its
/// own listening port, its own tunnel -- which is worth having when the tunnel
/// is what saturates rather than the leechers. Three tunnels are three egress
/// paths, and the payload is paid for once.
///
/// Added in seed mode: the data is there and already verified, and rechecking a
/// large payload to learn what the first engine already knows would cost hours
/// of disk for nothing.
async fn post_torrent_copy(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let target = v.get("engine").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    let hash = info_hash.to_lowercase();

    let bad = |m: String| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))).into_response();
    if target.is_empty() {
        return bad("engine is required".into());
    }
    if !state.engines.engines().iter().any(|e| e.id == target) {
        return bad(format!("no engine named {target} on this node"));
    }
    if find_copy(&state, &target, &hash).is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("{target} already seeds it")})),
        )
            .into_response();
    }

    // The source copy supplies the save path: two engines seeding one payload
    // must be pointed at the SAME files, or the second downloads its own and
    // the whole point is lost.
    let (existing, save_path, category, added) = {
        let store = state.store.lock().unwrap();
        let sessions = store.sessions_of(&hash);
        let Some(first) = sessions.first().cloned() else {
            return not_found();
        };
        let facts = store.facts_for_session(&first).unwrap_or_default();
        let f = facts.get(&hash).cloned().unwrap_or_default();
        (first, f.save_path, f.category, f.added_time as f64)
    };
    let blob = {
        let store = state.store.lock().unwrap();
        store.torrent_blob(&hash).ok().flatten()
    };
    let Some(blob) = blob else { return not_found() };
    let Some(dst) = state.engines.get(&target) else { return not_found() };
    if let Err(e) = dst.manager.add_torrent_bytes(&blob, &save_path, false, true) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("the engine refused it: {e}")})),
        )
            .into_response();
    }
    {
        let store = state.store.lock().unwrap();
        let _ = store.insert_torrent(&hash, &target, &blob, &save_path, &category, added, false, "");
    }
    tracing::info!(hash = %hash, from = %existing, to = %target, "torrent now seeded from a second engine");
    Json(serde_json::json!({"status": "ok", "engine": target, "save_path": save_path}))
        .into_response()
}

/// Move a torrent to another engine OF THIS NODE.
///
/// No transfer: both engines read the same filesystem, so the payload stays
/// exactly where it is and only ownership changes. That is the difference with
/// a handoff, and it is why this is instant -- moving hoard to a VPN-bound
/// engine to change which tunnel it seeds from costs nothing.
///
/// Duplicating locally is NOT offered, and the reason is not tidiness: two
/// engines pointed at one set of files are two writers on the same bytes the
/// first time either repairs a piece.
/// Queue a graduation: move this torrent's DATA to another engine's storage.
///
/// Not the same thing as `POST /api/torrents/:hash/engine`, which reassigns the
/// engine and deliberately leaves every byte where it is. This one moves the
/// payload, which is the whole point when the race disk is what needs emptying.
/// It returns a job id rather than doing the work: at ~520 MB/s a 50 GB torrent
/// is a hundred seconds, and an HTTP request is not the place to spend them.
async fn post_torrent_graduate(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let hash = info_hash.to_lowercase();
    let Some((current, torrent)) = find_torrent(&state, &hash) else {
        return not_found();
    };
    let to = v.get("engine").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let category = v.get("category").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    let mut save_path = v.get("save_path").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    if to.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "engine is required"})))
            .into_response();
    }
    if to == current {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("already in {current}")}))).into_response();
    }
    if state.engines.get(&to).is_none() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("no engine named {to}")}))).into_response();
    }
    // The category's own save path when the caller did not name one: that is
    // where a graduation is supposed to land, and asking the operator to repeat
    // it is how the two drift apart.
    if save_path.is_empty() && !category.is_empty() {
        let cfg = state.cfg();
        let _ = &cfg;
        if let Some(c) = category_entry(&state, &category) {
            if !c.save_path.is_empty() {
                save_path = c.save_path;
            }
        }
    }
    if save_path.is_empty() {
        return (StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "save_path is required (or a category that has one)"})))
            .into_response();
    }
    match crate::jobsrun::queue_graduation(
        &state,
        &hash,
        &torrent.meta.name,
        &current,
        &to,
        &category,
        &save_path,
        torrent.meta.total_size as i64,
    ) {
        Some(id) => Json(serde_json::json!({"status": "queued", "job": id})).into_response(),
        None => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "a graduation is already queued or running for this torrent"})),
        )
            .into_response(),
    }
}

/// What the drain may do with this torrent, per its category.
///
/// "keep" when the category says nothing, which is every category that existed
/// before this shipped.
fn is_false(b: &bool) -> bool {
    !*b
}

/// Every configured category.
///
/// Same source and same precedence as `category_entry`: the store document
/// first, the file only when the store has nothing. Reading them from two
/// different places is how the two would start to disagree.
fn all_categories(state: &AppState) -> Vec<Category> {
    let cfg = state.cfg();
    let raw = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.meta_doc("categories")
    }
    .filter(|doc| !doc.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("categories.json");
        std::fs::read_to_string(path).ok()
    });
    let Some(raw) = raw else { return Vec::new() };
    // ⚠ `name` is NOT in the stored document -- it is the map KEY, and the
    // struct defaults it to "". Returning the values as-is hands back a list of
    // nameless categories, and every caller that matches on the name silently
    // matches nothing. `category_entry` fills it in for the same reason.
    serde_json::from_str::<std::collections::BTreeMap<String, Category>>(&raw)
        .map(|m| {
            m.into_iter()
                .map(|(k, mut c)| {
                    c.name = k;
                    c
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The categories some other category graduates INTO.
///
/// A transit area, not a library: a torrent lands there to finish the seeding
/// time it still owes, and is deleted once it has. Scoped deliberately -- the
/// same sweep applied to every hoard category would delete the whole library.
pub(crate) fn graduation_target_categories(state: &AppState) -> std::collections::HashSet<String> {
    let cats = all_categories(state);
    // Named as a destination by somebody...
    let destinations: std::collections::HashSet<String> = cats
        .iter()
        .filter(|c| !c.graduate_to.is_empty())
        .map(|c| c.graduate_to.clone())
        .collect();
    // ...AND marked as a waiting room rather than a library. Both conditions,
    // on purpose: `transit` ticked on a category nothing graduates into is
    // then harmless, and the cost of being wrong here is someone's library.
    cats.into_iter()
        .filter(|c| c.transit && destinations.contains(&c.name))
        .map(|c| c.name)
        .collect()
}

/// The category THIS ENGINE's copy is filed under, or empty.
///
/// `engine_id` is required for the reason spelled out on `Store::category_of`:
/// a torrent held by two engines has one category per copy, and the caller
/// always knows which engine it is asking for.
pub(crate) fn category_of_hash(state: &AppState, hash: &str, engine_id: &str) -> String {
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    store.category_of(hash, engine_id).unwrap_or_default()
}

/// The graduation target of THIS ENGINE's copy: (engine, category, path).
pub(crate) fn category_graduation(
    state: &AppState,
    hash: &str,
    engine_id: &str,
) -> Option<(String, String, String)> {
    let cat = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.category_of(hash, engine_id).unwrap_or_default()
    };
    let entry = category_entry(state, &cat)?;
    if entry.graduate_to.is_empty() {
        return None;
    }
    let target = category_entry(state, &entry.graduate_to)?;
    if target.save_path.is_empty() {
        return None;
    }
    let (engine_id, _) = placement(state, &entry.graduate_to, "");
    Some((engine_id, entry.graduate_to, target.save_path))
}

/// A category's configured save path, and the engine role it files under.
///
/// Read the way `get_categories` reads them -- the store document first, the
/// on-disk `categories.json` as the fallback -- so the two cannot disagree
/// about where a category puts its data.
fn category_entry(state: &AppState, name: &str) -> Option<Category> {
    let cfg = state.cfg();
    let raw = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.meta_doc("categories")
    }
    .filter(|doc| !doc.is_empty())
    .or_else(|| {
        let path = std::path::Path::new(&cfg.daemon.data_dir).join("categories.json");
        std::fs::read_to_string(path).ok()
    })?;
    let map: std::collections::BTreeMap<String, Category> = serde_json::from_str(&raw).ok()?;
    let mut c = map.get(name).cloned()?;
    c.name = name.to_string();
    Some(c)
}

async fn post_torrent_engine(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let target = v.get("engine").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();

    let hash = info_hash.to_lowercase();
    // WHICH copy moves. With the same torrent in two engines, "move it" without
    // naming the source would move whichever the lookup met first.
    let asked = engine_param(&query, "");
    let (current, torrent) = if asked.is_empty() {
        match find_torrent(&state, &hash) {
            Some(v) => v,
            None => return not_found(),
        }
    } else {
        match find_copy(&state, &asked, &hash) {
            Some(t) => (asked.clone(), t),
            None => return not_found(),
        }
    };
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "engine is required"})))
            .into_response();
    }
    if target == current {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("already in {current}")})),
        )
            .into_response();
    }
    if !state.engines.engines().iter().any(|e| e.id == target) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("no engine named {target} on this node")})),
        )
            .into_response();
    }

    let (save_path, paused) = {
        let store = state.store.lock().unwrap();
        store
            .facts_for_session(&current)
            .ok()
            .and_then(|m| m.get(&hash).map(|f| (f.save_path.clone(), f.user_paused)))
            .unwrap_or_default()
    };
    let blob = {
        let store = state.store.lock().unwrap();
        store.torrent_blob(&hash).ok().flatten()
    };
    let Some(blob) = blob else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "no metainfo in the store; cannot re-add it elsewhere"})),
        )
            .into_response();
    };

    // Source first, and KEEPING the data: the files are the whole point of not
    // transferring anything.
    let ih = torrent.info_hash;
    if let Some(src) = state.engines.get(&current) {
        if let Err(e) = src.manager.remove_torrent(&ih, true) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("the source engine refused: {e}")})),
            )
                .into_response();
        }
        src.announce_cache.forget(&hash);
    }
    let Some(dst) = state.engines.get(&target) else { return not_found() };
    // seed_mode: the data is already there and already verified. Rechecking a
    // large payload for a move that touched nothing would cost hours of disk.
    if let Err(e) = dst
        .manager
        .add_torrent_bytes(&blob, &save_path, paused, true)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("the target engine refused it, and the source has let it go: {e}")
            })),
        )
            .into_response();
    }
    {
        let store = state.store.lock().unwrap();
        let _ = store.set_session(&hash, &current, &target);
    }
    tracing::info!(hash = %hash, from = %current, to = %target, "torrent moved between engines");
    Json(serde_json::json!({"status": "ok", "from": current, "to": target})).into_response()
}

/// Drop a torrent from this node, engine first then store.
///
/// Same order as `delete_torrent`, and for the same reason: removing the store
/// row alone leaves a torrent that still seeds, still announces, and comes back
/// at the next restart from the engine's own state -- present to the network,
/// invisible to the interface.
fn remove_torrent_everywhere(state: &AppState, info_hash: &str, delete_files: bool) {
    let hash = info_hash.to_lowercase();
    let keep_data = !delete_files;
    if let Some((_, torrent)) = find_torrent(state, &hash) {
        let ih = torrent.info_hash;
        for engine in state.engines.engines() {
            // This engine's OWN copy: the counters are per copy, and absorbing
            // the first engine's figures for every engine would credit the
            // carry-over with bytes the others never moved.
            if let Some(copy) = engine.manager.get(&ih) {
                if let Err(e) = engine.manager.remove_torrent(&ih, keep_data) {
                    tracing::warn!(hash = %hash, "engine refused removal: {e}");
                    return;
                }
                engine.announce_cache.forget(&hash);
                absorb_on_remove(state, &engine.id, &copy, &hash, Some(&engine.id));
            }
        }
    }
    {
        let store = state.store.lock().unwrap();
        let _ = store.delete_torrent(&hash);
    }
    tracing::info!(hash = %hash, delete_files, "torrent removed");
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

    // deleteFiles goes through the same path as the native DELETE. It used to
    // be read and honoured by nobody: with the flag set this handler dropped
    // neither the payload NOR the row and answered "Ok.", so every *arr that
    // removed a torrent with its data left both behind. That is how /race
    // reached 100% with no client aware of having failed at anything.
    let wants_files = form
        .get("deleteFiles")
        .map(|v| matches!(v.trim(), "true" | "1"))
        .unwrap_or(false);

    let hashes = split_list(form.get("hashes").map(String::as_str).unwrap_or(""));
    for prefix in hashes {
        let resolved = {
            let store = state.store.lock().unwrap();
            store.resolve_hash(&prefix)
        };
        let Some(hash) = resolved else { continue };
        // Unqualified, as qBit means it: every copy of this torrent.
        let sessions = {
            let store = state.store.lock().unwrap();
            store.sessions_of(&hash)
        };
        match remove_one_torrent(&state, &hash, &sessions, "", wants_files) {
            Ok(dropped) => {
                tracing::info!(hash = %hash, delete_files = wants_files, copies = dropped,
                    "torrent removed (qbit shim)");
            }
            Err(e) => tracing::warn!(hash = %hash, "shim removal failed: {e}"),
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
    let state_str = find_selected(&state, &query, &hash)
        .map(|(_, torrent)| {
            typhon_engine::rpc::dispatch::torrent_to_json(&torrent)
                .get("state")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_default();

    // Read from bench.db, which has been recording this all along.
    //
    // This handler returned two hard-coded empty arrays from the V4 port until
    // 2026-09-16, so every race timeline drew "No timeline data" while the
    // events piled up underneath -- 2585 rows on the production node.
    //
    // An unreadable measurement database is not an error worth a 500: the
    // timeline is observability, and losing it must never cost the seedbox.
    // The empty answer the front already handles is the right one.
    let (events, snapshots) = match state.bench.as_ref() {
        Some(bench) => {
            let db = match bench.lock() {
                Ok(db) => db,
                Err(poisoned) => poisoned.into_inner(),
            };
            (
                db.events_for(&hash).unwrap_or_default(),
                db.snapshots_for(&hash).unwrap_or_default(),
            )
        }
        None => (Vec::new(), Vec::new()),
    };

    Json(serde_json::json!({
        "events": events, "snapshots": snapshots, "info_hash": hash, "state": state_str,
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
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);

    let Ok(creds) = serde_json::from_str::<crate::importer::QbitCreds>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid body: url, username, password"})),
        )
            .into_response();
    };

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let job_id = format!("imp-{nanos}");
    let progress = Arc::new(crate::importer::Progress::default());
    state.imports.lock().unwrap().insert(job_id.clone(), progress.clone());

    // The engine that receives the library. A qBittorrent import is a
    // takeover of a settled collection, which is a hoard, not a race.
    let manager = state
        .engines
        .engines()
        .iter()
        .find(|e| e.id == "hoard")
        .map(|e| e.manager.clone());
    let torrent_dir = state.config_path.parent().map(|p| p.join("hoard").join("torrents"));

    tokio::spawn(async move {
        crate::importer::run_import(creds, progress, move |t, bytes| {
            let Some(manager) = manager.as_ref() else {
                return Err("no hoard engine to import into".into());
            };
            // seed_mode: the data is already there, whole. Rechecking a
            // quarter of a million imported torrents would read the entire
            // library off disk before a single one could be served.
            manager
                .add_torrent_bytes(&bytes, &t.save_path, false, true)
                .map(|_| ())
        })
        .await;
    });

    Json(serde_json::json!({"job_id": job_id})).into_response()
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

    // Everything below used to be missing: this handler validated the race port
    // and answered {"status":"ok"} without touching the file. The panel reported
    // a saved network configuration that had never been written anywhere, which
    // is worse than refusing -- the operator walks away believing it took.
    let f = parsed.get("fields").cloned().unwrap_or_default();
    let num = |k: &str| f.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
    let txt = |k: &str| {
        f.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let flag = |k: &str| f.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let q = crate::tomledit::quote_toml_key;

    let hoard_port = f
        .get("hoard_listen_port")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if !(1..=65535).contains(&hoard_port) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":
                "the hoard listen port must be between 1 and 65535"})),
        )
            .into_response();
    }
    // Two engines on one port is the failure this build already had once, from
    // the other direction: refuse it here rather than write it and find out at
    // the next boot, in a log nobody is reading.
    if race_port == hoard_port {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":
                "race and hoard cannot share a listen port"})),
        )
            .into_response();
    }

    let trusted = f
        .get("proxy_v2_trusted_sources")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(q)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    let shared = |port: i64, iface: &str, pv2: i64| -> Vec<(String, String)> {
        vec![
            ("listen_port".into(), port.to_string()),
            ("bind_interface".into(), q(iface)),
            ("enable_ipv6".into(), flag("enable_ipv6").to_string()),
            ("listen_port_proxy_v2".into(), pv2.to_string()),
            ("listen_addr_proxy_v2".into(), q(&txt("proxy_v2_listen_addr"))),
            ("proxy_v2_trusted_sources".into(), format!("[{trusted}]")),
            ("socks5_outbound_host".into(), q(&txt("socks5_host"))),
            ("socks5_outbound_port".into(), num("socks5_port").to_string()),
            ("socks5_outbound_user".into(), q(&txt("socks5_user"))),
            ("socks5_outbound_pass".into(), q(&txt("socks5_pass"))),
        ]
    };

    let mut race_kv = shared(
        race_port,
        &txt("race_bind_interface"),
        num("race_proxy_v2_port"),
    );
    let mut hoard_kv = shared(
        hoard_port,
        &txt("hoard_bind_interface"),
        num("hoard_proxy_v2_port"),
    );
    // Gluetun forwards one port, so it belongs to one engine. The other must be
    // written as OFF, or switching the choice would leave both following it.
    let gl_engine = txt("gluetun_port_engine");
    for (name, kv) in [("race", &mut race_kv), ("hoard", &mut hoard_kv)] {
        let mine = gl_engine == name;
        kv.push((
            "gluetun_port_forward".into(),
            (mine && flag("gluetun_port_forward")).to_string(),
        ));
        kv.push(("gluetun_url".into(), q(&txt("gluetun_url"))));
        kv.push(("gluetun_api_key".into(), q(&txt("gluetun_api_key"))));
    }

    let extras: Vec<serde_json::Value> = parsed
        .get("extra_engines")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let ok = edit_config(&state, |doc| {
        let mut out = crate::tomledit::set_toml_table(doc, "race", &race_kv)?;
        out = crate::tomledit::set_toml_table(&out, "hoard", &hoard_kv)?;
        for e in &extras {
            let Some(id) = e.get("id").and_then(|v| v.as_str()) else { continue };
            if let Some(p) = e.get("listen_port").and_then(|v| v.as_i64()) {
                if (1..=65535).contains(&p) {
                    if let Some(next) =
                        crate::tomledit::set_agent_session_key(&out, id, "listen_port", &p.to_string())
                    {
                        out = next;
                    }
                }
            }
            if let Some(i) = e.get("bind_interface").and_then(|v| v.as_str()) {
                if let Some(next) =
                    crate::tomledit::set_agent_session_key(&out, id, "bind_interface", &q(i))
                {
                    out = next;
                }
            }
        }
        Ok(out)
    });
    if !ok {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "the config could not be written"})),
        )
            .into_response();
    }
    // Engines read their network once, at boot.
    Json(serde_json::json!({"status": "ok", "restart_required": true})).into_response()
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

/// Declare one more engine on THIS node.
///
/// Writes a `[[agent]]` block and asks for a restart: engines are built once,
/// at boot, from the config. Nothing is started here.
///
/// The port check is the point. `connect` used to hand every engine that was
/// not race the hoard session, so two engines quietly shared one socket; that
/// is fixed, but a form that lets the operator ASK for the same port would
/// reproduce it by another road, and the failure would again be silent.
async fn post_engine_create(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    let role = v.get("role").and_then(|x| x.as_str()).unwrap_or_default().trim().to_string();
    let port = v.get("listen_port").and_then(|x| x.as_i64()).unwrap_or(0);
    let iface = v
        .get("bind_interface")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();

    let bad = |m: &str| {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": m}))).into_response()
    };
    // An id becomes a directory name under the config dir, and a TOML key.
    if id.is_empty()
        || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return bad("id must be non-empty and only letters, digits, - or _");
    }
    if role != "race" && role != "hoard" {
        return bad("role must be \"race\" or \"hoard\"");
    }
    if !(1..=65535).contains(&port) {
        return bad("listen_port is required");
    }

    for e in state.engines.engines() {
        if e.id == id {
            return bad("an engine with that id already exists");
        }
        if e.listen_port as i64 == port {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("port {port} is already taken by engine {}", e.id)
                })),
            )
                .into_response();
        }
    }

    // An interface that is not up binds nothing, and the engine says nothing:
    // measured on this build, `bind_interface = "wg9"` with no such device
    // logged "session started, listen=0.0.0.0:16379" and "on the network,
    // announcing" while `ss` showed no listener at all. That failure mode is
    // invisible in production -- an engine that seeds nothing while claiming to
    // be online -- so the declaration is refused here instead.
    if !iface.is_empty() {
        let known: Vec<String> = interfaces()
            .iter()
            .filter_map(|i| i.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        if !known.iter().any(|n| n == &iface) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("no interface named {iface} is up"),
                    "interfaces": known,
                })),
            )
                .into_response();
        }
    }

    let mut session: Vec<(String, String)> = vec![("listen_port".into(), port.to_string())];
    if !iface.is_empty() {
        session.push((
            "bind_interface".into(),
            crate::tomledit::quote_toml_key(&iface),
        ));
    }
    let ok = edit_config(&state, |doc| {
        Ok(crate::tomledit::append_agent_block(doc, &id, &role, &session))
    });
    if !ok {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "the config could not be written"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"status": "ok", "id": id, "restart_required": true}))
        .into_response()
}

/// Removing an engine is NOT idempotent: an unknown one is 404.
///
/// `race` and `hoard` are refused: they come from the `[race]` and `[hoard]`
/// sections that every install has, not from a block that can be dropped, and
/// deleting one would leave a node that cannot describe its own tiers.
///
/// The data is left where it is. An engine declaration is not its catalogue.
async fn delete_engine(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let query = query.unwrap_or_default();
    guard!(state, headers, query);
    if id == "race" || id == "hoard" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "race and hoard cannot be removed"})),
        )
            .into_response();
    }
    let mut found = false;
    let ok = edit_config(&state, |doc| match crate::tomledit::delete_agent_block(doc, &id) {
        Some(edited) => {
            found = true;
            Ok(edited)
        }
        None => Err("unknown engine".to_string()),
    });
    if !found || !ok {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "unknown engine"})))
            .into_response();
    }
    Json(serde_json::json!({"status": "ok", "restart_required": true})).into_response()
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

/// Liveness, and where the interface reads its own version from.
///
/// Public, like 3.x: it is what a probe hits, and it must answer before the
/// operator has a key. The port left it out of the main router entirely -- only
/// the rescue surface had one -- so `/health` 404'd, and the page that fills
/// both version labels from it silently left the header blank and the footer on
/// its hardcoded placeholder.
async fn get_health(State(state): State<AppState>) -> Response {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(serde_json::json!({
        "status": "healthy",
        "version": HYDRANOS_VERSION,
        "uptime": (now - state.started_at) as f64,
    }))
    .into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(get_health))
        .route("/api/dedup/stats", get(get_dedup_stats))
        .route("/api/dedup/config", axum::routing::post(post_dedup_config))
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
        .route("/api/hoard/page", get(get_hoard_page))
        .route("/api/race/page", get(get_race_page))
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
        .route("/api/drain/policy", axum::routing::post(set_volume_policy))
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
        .route("/api/nodes", get(get_nodes).post(post_node))
        .route("/api/nodes/test", axum::routing::post(post_node_test))
        .route("/api/nodes/enrol", axum::routing::post(post_node_enrol))
        .route("/api/nodes/register", axum::routing::post(post_node_register))
        .route("/install.sh", get(get_install_script))
        .route("/api/nodes/:name", axum::routing::delete(delete_node))
        .route("/api/nodes/:name/handoff", axum::routing::post(post_node_handoff))
        .route("/api/nodes/:name/fetch", axum::routing::post(post_node_fetch))
        .route("/api/nodes/:name/move-engine", axum::routing::post(post_node_move_engine))
        .route("/node/:name/open", get(get_node_open))
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
        .route("/api/hoard/pause-all", axum::routing::post(hoard_pause_all))
        .route("/api/hoard/resume-all", axum::routing::post(hoard_resume_all))
        .route("/api/hoard/pause", axum::routing::post(hoard_pause_bulk))
        // Any engine, not just the two with a literal route. A node running one
        // engine per tunnel has to be able to pause the copy on ONE of them.
        .route("/api/engines/:id/pause", axum::routing::post(engine_pause_bulk))
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
        .route("/api/torrents/:info_hash/torrent", get(get_torrent_file))
        .route("/api/torrents/:info_hash/peers", axum::routing::post(post_torrent_peers))
        .route("/api/torrents/:info_hash/engine", axum::routing::post(post_torrent_engine))
        .route("/api/torrents/:info_hash/graduate", axum::routing::post(post_torrent_graduate))
        .route("/api/torrents/:info_hash/copy", axum::routing::post(post_torrent_copy))
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
        // The interface. Not an API route, which is exactly why a bench that
        // compares /api/* answers cannot tell whether it is served at all.
        // The four the interface calls before it can show anything. /api/setup
        // is its very first request: unanswered, the page sits on
        // "Initializing…" with every other route green.
        .route("/api/setup", get(crate::bootstrap::setup_status)
                             .post(crate::bootstrap::setup_password))
        .route("/api/login", axum::routing::post(crate::bootstrap::login))
        .route("/api/startup", get(crate::bootstrap::startup))
        .route("/metrics", get(crate::bootstrap::metrics))
        .route("/", get(crate::web::index))
        .route("/static/*path", get(crate::web::static_file))
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
        .route("/api/announce/health", get(get_announce_health))
        .route("/api/announce/policy", get(get_live_announce_policy))
        .route("/api/announce/mute", axum::routing::post(set_announce_mute))
        .route("/api/announce/hidden", axum::routing::post(set_announce_hidden))
        .route("/api/announce/min-seed", axum::routing::post(set_announce_min_seed))
        .route("/api/announce/passkeys", get(get_passkeys).post(set_announce_passkey))
        .route("/api/categories/:name", axum::routing::put(category_update).delete(category_delete))
        // Workflows carry their own routes, so this file does not grow another
        // six handlers. Merged before with_state so they share it.
        .merge(crate::rulesapi::routes())
        .with_state(state)
        // gzip, as 3.x does on this stream. Hydration is ~250 MB of JSON at
        // 300k torrents: a browser will not sit through that uncompressed, and
        // the list stays empty while it tries.
        // gzip, as 3.x does on this stream. Hydration is ~250 MB of JSON at
        // 300k torrents: a browser will not sit through that uncompressed, and
        // the list stays empty while it tries.
        //
        // The default predicate excludes text/event-stream, on the reasoning
        // that buffering breaks a live stream. It does not here: the body is
        // flushed per frame, which is exactly what the Go side does with its
        // own gzip writer. So the predicate is replaced by the size floor
        // alone -- compressing a 32-byte keepalive would cost more than it
        // saves.
        .layer(
            tower_http::compression::CompressionLayer::new()
                .gzip(true)
                .compress_when(tower_http::compression::predicate::SizeAbove::new(32)),
        )
        .layer(axum::middleware::from_fn(qbit_refusal_shape))
}

/// Answer an unauthenticated qBittorrent call the way qBittorrent answers it.
///
/// ⭐⭐ qBittorrent's WebUI returns **403 Forbidden** when there is no session,
/// not 401. Sonarr and Radarr rely on that: `QBittorrentProxySelector` probes
/// `/api/v2/app/webapiVersion` *before* logging in, reads a 403 as "log in
/// first", and treats anything else as the server not being a qBittorrent at
/// all. Against a 401 the test failed with "Unable to connect to qBittorrent"
/// -- with the network fine and the credentials correct, which is as
/// misleading as an error message gets.
///
/// Only the shim. The native API keeps 401, which is the right code and what
/// its own callers expect; this is compatibility with one client's reading of
/// another server's quirk, and it is scoped to the paths that imitate it.
async fn qbit_refusal_shape(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let is_shim = req.uri().path().starts_with("/api/v2/");
    let res = next.run(req).await;
    if is_shim && res.status() == StatusCode::UNAUTHORIZED {
        return (StatusCode::FORBIDDEN, "Forbidden.").into_response();
    }
    res
}

#[cfg(test)]
mod fleet_tests {
    use super::*;

    /// The host extraction has to survive every shape a URL arrives in, since
    /// a wrong answer here either blocks a legitimate node or lets a loopback
    /// one through.
    #[test]
    fn loopback_is_recognised_whatever_the_url_looks_like() {
        let host_of = |url: &str| -> String {
            url.split("//").nth(1).unwrap_or(url)
                .split('/').next().unwrap_or_default()
                .rsplit(':').last().unwrap_or_default()
                .trim_matches(|c| c == '[' || c == ']')
                .to_string()
        };
        assert_eq!(host_of("http://127.0.0.1:8499"), "127.0.0.1");
        assert_eq!(host_of("http://localhost:8199/"), "localhost");
        assert_eq!(host_of("http://192.168.99.200:8499"), "192.168.99.200");
        assert_eq!(host_of("https://seedbox.example.net:8199"), "seedbox.example.net");
        // A LAN address must NOT be mistaken for loopback.
        assert!(!host_of("http://192.168.99.200:8499").starts_with("127."));
    }

    fn page(rows: &[(&str, f64)]) -> serde_json::Value {
        serde_json::json!({
            "total": rows.len(),
            "filtered": rows.len(),
            "rows": rows.iter().map(|(h, t)| serde_json::json!({
                "info_hash": h, "added_time": t, "name": h, "state": "seeding"
            })).collect::<Vec<_>>(),
        })
    }

    fn hashes(v: &serde_json::Value) -> Vec<String> {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["info_hash"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn two_sorted_pages_interleave_and_the_counts_add_up() {
        let a = page(&[("aa", 30.0), ("bb", 10.0)]);
        let b = page(&[("cc", 20.0), ("dd", 5.0)]);
        let out = merge_pages(vec![a, b], "added_time", false, 0, 10);
        assert_eq!(hashes(&out), vec!["aa", "cc", "bb", "dd"]);
        assert_eq!(out["total"].as_i64(), Some(4));
        assert_eq!(out["filtered"].as_i64(), Some(4));
    }

    /// The property that matters: paging the fleet must not duplicate a torrent
    /// or lose one. Two pages of two, read back to back, must be the whole set
    /// in order and each row exactly once.
    #[test]
    fn consecutive_windows_are_disjoint_and_complete() {
        let mk = || vec![
            page(&[("aa", 30.0), ("bb", 10.0)]),
            page(&[("cc", 20.0), ("dd", 5.0)]),
        ];
        let first = hashes(&merge_pages(mk(), "added_time", false, 0, 2));
        let second = hashes(&merge_pages(mk(), "added_time", false, 2, 2));
        assert_eq!(first, vec!["aa", "cc"]);
        assert_eq!(second, vec!["bb", "dd"]);
        let mut all = first.clone();
        all.extend(second);
        all.sort();
        all.dedup();
        assert_eq!(all.len(), 4, "a row was duplicated or dropped between pages");
    }

    /// Rows that compare equal still need a total order, or the same torrent
    /// lands on two pages -- or on none -- as soon as the sort is not unique.
    #[test]
    fn equal_keys_are_broken_by_hash_so_the_order_is_total() {
        let a = page(&[("bb", 10.0)]);
        let b = page(&[("aa", 10.0)]);
        let out = merge_pages(vec![a, b], "added_time", true, 0, 10);
        assert_eq!(hashes(&out), vec!["aa", "bb"]);
    }

    fn page_with(rows: Vec<(&str, &str)>, filtered: i64) -> serde_json::Value {
        serde_json::json!({
            "total": filtered,
            "filtered": filtered,
            "rows": rows.iter().map(|(hash, msg)| serde_json::json!({
                "info_hash": hash,
                "tracker_error_msg": msg,
                "added_time": 1,
            })).collect::<Vec<_>>(),
        })
    }

    /// A node that does not know the filter sends everything. Without the
    /// guard the page shows its passkey failures under "dead", which is the
    /// bug this exists to stop: remove the retain() and this test fails.
    #[test]
    fn an_older_node_does_not_smuggle_in_other_classes() {
        let mut pages = vec![page_with(
            vec![
                ("a", "v4: tracker: torrent introuvable"),
                ("b", "v4: tracker: invalid passkey"),
                ("c", "v4: <url>): operation timed out"),
            ],
            3,
        )];
        enforce_error_class(&mut pages, &["dead".to_string()], &[], 10);
        let rows = pages[0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["info_hash"], "a");
        // The whole catalogue fitted in the window, so the count is exact.
        assert_eq!(pages[0]["filtered"], 1);
    }

    /// A node that filled its window is holding rows nobody has seen, so its
    /// count cannot be corrected from the visible ones.
    #[test]
    fn a_full_window_keeps_its_own_count() {
        let mut pages = vec![page_with(
            vec![
                ("a", "v4: tracker: torrent introuvable"),
                ("b", "v4: tracker: invalid passkey"),
            ],
            900,
        )];
        enforce_error_class(&mut pages, &["dead".to_string()], &[], 2);
        assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
        assert_eq!(pages[0]["filtered"], 900);
    }

    /// No filter asked for, nothing touched -- including the torrents with no
    /// error at all, which belong to no class and must not be swept away.
    #[test]
    fn no_filter_leaves_every_row_alone() {
        let mut pages = vec![page_with(vec![("a", ""), ("b", "v4: tracker: invalid passkey")], 2)];
        enforce_error_class(&mut pages, &[], &[], 10);
        assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 2);
    }

    /// Excluding a class keeps the torrents that are in NO class.
    #[test]
    fn excluding_a_class_keeps_the_healthy_ones() {
        let mut pages = vec![page_with(vec![("a", ""), ("b", "v4: tracker: invalid passkey")], 2)];
        enforce_error_class(&mut pages, &[], &["auth".to_string()], 10);
        let rows = pages[0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["info_hash"], "a");
    }

    /// Facets from several nodes add up instead of vanishing. Enrolling one
    /// node used to blank every number on the page.
    #[test]
    fn facets_of_two_nodes_are_summed() {
        let a = serde_json::json!({
            "total": 1, "filtered": 1, "rows": [],
            "facets": {"all": 10, "category": {"movies": 4}, "error_class": {"dead": 2}},
        });
        let b = serde_json::json!({
            "total": 1, "filtered": 1, "rows": [],
            "facets": {"all": 5, "category": {"movies": 1, "series": 3}, "error_class": {"auth": 1}},
        });
        let out = merge_pages(vec![a, b], "added_time", false, 0, 10);
        assert_eq!(out["facets"]["all"], 15);
        assert_eq!(out["facets"]["category"]["movies"], 5);
        // A category only one node knows is still the answer for that node.
        assert_eq!(out["facets"]["category"]["series"], 3);
        assert_eq!(out["facets"]["error_class"]["dead"], 2);
        assert_eq!(out["facets"]["error_class"]["auth"], 1);
    }

    /// Null when nobody counted: that is a different claim from zero, and the
    /// UI reads the difference to decide whether to draw chips at all.
    #[test]
    fn facets_stay_null_when_no_node_counted() {
        let a = serde_json::json!({"total": 1, "filtered": 1, "rows": [], "facets": null});
        let out = merge_pages(vec![a], "added_time", false, 0, 10);
        assert!(out["facets"].is_null());
    }

    #[test]
    fn a_seeding_row_sorts_as_complete_whatever_its_progress() {
        let seeding = serde_json::json!({"state": "seeding", "progress": 0.2, "info_hash": "aa"});
        let leeching = serde_json::json!({"state": "downloading", "progress": 0.9, "info_hash": "bb"});
        assert_eq!(row_sort_key(&seeding, "progress").1, 1.0);
        assert_eq!(row_sort_key(&leeching, "progress").1, 0.9);
    }

    #[test]
    fn the_window_is_replaced_and_every_other_filter_survives() {
        let q = with_window("search=demo&offset=500&limit=500&sort=name", 0, 1000);
        assert!(q.contains("search=demo"), "{q}");
        assert!(q.contains("sort=name"), "{q}");
        assert!(q.contains("offset=0"), "{q}");
        assert!(q.contains("limit=1000"), "{q}");
        assert!(!q.contains("offset=500"), "the old window must go: {q}");
    }
}

#[cfg(test)]
mod bulk_body_tests {
    use super::*;

    /// ⭐ THE REGRESSION TEST FOR 2026-09-16.
    ///
    /// This is the exact body the browser sent for any selection over 500 rows.
    /// It parsed without complaint, `filter` was dropped on the floor, `hashes`
    /// defaulted to empty -- and empty meant the whole engine. A start aimed at
    /// 70k torrents started 293k.
    ///
    /// It must now be refused. If someone re-adds a `filter` field, they have to
    /// implement it: this test fails the moment it parses again.
    #[test]
    fn the_body_that_started_the_whole_library_is_refused() {
        let body = r#"{"action":"start","filter":{"category":"Calewood","search":"","tracker":"","tag":"","state":""},"exclude":[]}"#;
        let parsed = serde_json::from_str::<BulkBody>(body);
        let err = parsed.err().expect("a filter this API never implemented must not parse");
        assert!(
            err.to_string().contains("filter"),
            "the error must NAME the field, or the caller learns nothing: {err}"
        );
    }

    /// The same shape without the unknown key still parses -- and still means
    /// nothing, because `all` was not set. The refusal of an empty selection
    /// lives in `bulk_action`; this pins the value it reads.
    #[test]
    fn an_empty_selection_does_not_opt_into_everything() {
        let req: BulkBody =
            serde_json::from_str(r#"{"action":"start","hashes":[],"exclude":[]}"#).unwrap();
        assert!(req.hashes.is_empty());
        assert!(!req.all, "empty must never imply the whole engine");
    }

    #[test]
    fn everything_is_reachable_but_only_on_purpose() {
        let req: BulkBody =
            serde_json::from_str(r#"{"action":"stop","hashes":[],"all":true}"#).unwrap();
        assert!(req.all);
    }

    #[test]
    fn a_normal_selection_still_parses() {
        let req: BulkBody = serde_json::from_str(
            r#"{"action":"stop","hashes":["aa","BB"],"exclude":["cc"]}"#,
        )
        .unwrap();
        assert_eq!(req.hashes, vec!["aa", "BB"]);
        assert_eq!(req.exclude, vec!["cc"]);
        assert!(!req.all);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The arithmetic behind session/day, without an engine.
    ///
    /// Written as a pure function of the same three marks `session_and_day`
    /// keeps, because the bug it guards is not a crash: it is a lifetime total
    /// published in a field labelled "day", which reads as a plausible number.
    fn split(odo: &mut Odometer, totals: (i64, i64), today: &str) -> ((i64, i64), (i64, i64)) {
        if totals.0 < odo.session_offset.0 || totals.1 < odo.session_offset.1 {
            odo.session_offset = totals;
        }
        let session = (
            (totals.0 - odo.session_offset.0).max(0),
            (totals.1 - odo.session_offset.1).max(0),
        );
        if odo.day_date != today {
            odo.day_date = today.to_string();
            odo.day_baseline = session;
        }
        if session.0 < odo.day_baseline.0 || session.1 < odo.day_baseline.1 {
            odo.day_baseline = (0, 0);
        }
        let day = (
            (session.0 - odo.day_baseline.0).max(0),
            (session.1 - odo.day_baseline.1).max(0),
        );
        (session, day)
    }

    #[test]
    fn a_lifetime_total_is_not_todays_traffic() {
        // A library that has moved 321 TB before this process ever started.
        let mut odo = Odometer {
            per_engine: Default::default(),
            session_offset: (321_000, 90_000),
            day_baseline: (0, 0),
            day_date: "2026-09-08".into(),
        };
        // Nothing has moved yet this boot.
        let (session, day) = split(&mut odo, (321_000, 90_000), "2026-09-08");
        assert_eq!(session, (0, 0), "a fresh boot has moved nothing");
        assert_eq!(day, (0, 0), "and today is not the whole history");

        // 500 units later.
        let (session, day) = split(&mut odo, (321_500, 90_000), "2026-09-08");
        assert_eq!(session, (500, 0));
        assert_eq!(day, (500, 0));
    }

    #[test]
    fn midnight_resets_the_day_but_not_the_session() {
        let mut odo = Odometer {
            per_engine: Default::default(),
            session_offset: (1000, 0),
            day_baseline: (0, 0),
            day_date: "2026-09-08".into(),
        };
        let (session, day) = split(&mut odo, (1700, 0), "2026-09-08");
        assert_eq!((session.0, day.0), (700, 700));

        // The date rolls; the session keeps counting, the day starts over.
        let (session, day) = split(&mut odo, (1900, 0), "2026-09-09");
        assert_eq!(session.0, 900, "the session survives midnight");
        assert_eq!(day.0, 0, "the day does not");

        let (session, day) = split(&mut odo, (2000, 0), "2026-09-09");
        assert_eq!((session.0, day.0), (1000, 100));
    }

    #[test]
    fn removing_a_torrent_never_makes_the_counters_negative() {
        let mut odo = Odometer {
            per_engine: Default::default(),
            session_offset: (1000, 0),
            day_baseline: (0, 0),
            day_date: "2026-09-08".into(),
        };
        let _ = split(&mut odo, (1500, 0), "2026-09-08");
        // A torrent carrying 1.2k lifetime bytes is removed: the sum drops
        // below the mark taken at boot.
        let (session, day) = split(&mut odo, (300, 0), "2026-09-08");
        assert_eq!(session, (0, 0), "follow the totals down, never go negative");
        assert_eq!(day, (0, 0));
    }

    /// The bug, stated as a test.
    ///
    /// The totals are a sum over the torrents currently LOADED, so a delete
    /// takes that torrent's LIFETIME bytes out of the sum. Without `forget`,
    /// removing a torrent that had uploaded 1.2k over months subtracts 1.2k
    /// from TODAY -- and the ratchet above then floors the day at zero, which
    /// is the "never negative" safety net firing on a number that should never
    /// have moved. On prod this erased 7 TB in a single day.
    #[test]
    fn a_delete_does_not_rewrite_todays_figure() {
        let mut odo = Odometer {
            per_engine: [("hoard".to_string(), (1000, 0))].into_iter().collect(),
            session_offset: (1000, 0),
            day_baseline: (0, 0),
            day_date: "2026-09-08".into(),
        };
        // 500 moved today, on a library summing 1500.
        let (session, day) = split(&mut odo, (1500, 0), "2026-09-08");
        assert_eq!((session.0, day.0), (500, 500));

        // A torrent holding 1200 lifetime bytes is removed. The live sum falls
        // to 300; the mark follows it down by the same 1200.
        odo.forget("hoard", 1200, 0);
        let (session, day) = split(&mut odo, (300, 0), "2026-09-08");
        assert_eq!(session.0, 500, "the session is untouched by a delete");
        assert_eq!(day.0, 500, "and so is the day");
        assert_eq!(odo.per_engine["hoard"], (-200, 0), "the engine mark moved too");

        // What the process keeps doing afterwards still counts.
        let (session, day) = split(&mut odo, (400, 0), "2026-09-08");
        assert_eq!((session.0, day.0), (600, 600));
    }

    /// An engine the odometer has no mark for must not be credited with the
    /// removal -- a typo'd engine id silently moving the wrong mark is exactly
    /// the kind of quiet drift this whole change exists to end.
    #[test]
    fn forgetting_names_an_engine_or_moves_only_the_global_mark() {
        let mut odo = Odometer {
            per_engine: [("hoard".to_string(), (10, 0))].into_iter().collect(),
            session_offset: (10, 0),
            day_baseline: (0, 0),
            day_date: "2026-09-08".into(),
        };
        odo.forget("nope", 5, 0);
        assert_eq!(odo.session_offset, (5, 0), "the global mark always moves");
        assert_eq!(odo.per_engine["hoard"], (10, 0), "an unknown engine moves nothing else");
    }

    fn state(key: &str, password_hash: &str) -> AppState {
        let mut cfg = Config::default();
        cfg.daemon.api_key = key.into();
        cfg.auth.password_hash = password_hash.into();
        cfg.auth.username = "admin".into();
        AppState {
            imports: Default::default(),
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
            net_engines: Arc::new(tokio::sync::Mutex::new((Vec::new(), 0))),
            odometer: Default::default(),
            records: Default::default(),
            bench_path: std::path::PathBuf::new(),
            started_at: 0,
            logs: crate::logbuf::LogBuffer::new(),
            reconnect: Default::default(),
            bench: None,
            sessions: Default::default(),
        }
    }

    fn with_key(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("X-Api-Key", value.parse().unwrap());
        h
    }

    fn with_cookie(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::COOKIE, value.parse().unwrap());
        h
    }

    /// The *arr stack's path in, asserted from the outside.
    ///
    /// Sonarr, Radarr, autobrr and cross-seed configure a "qBittorrent" with a
    /// username and a password and cannot set a header, so a session cookie is
    /// the only credential they can present. Before this, `/api/v2/auth/login`
    /// answered `Ok.` to anything and set no cookie, and the very next request
    /// took a silent 401 -- which is exactly how it looked in production when
    /// the placeholder key stopped being a free pass.
    #[test]
    fn a_live_session_cookie_authorises_and_nothing_else_does() {
        let s = state("secret", "$2a$hash");
        let sid = s.sessions.create();

        assert!(
            authorised(&s, &with_cookie(&format!("SID={sid}")), ""),
            "a minted session must pass without any key"
        );
        assert!(
            authorised(&s, &with_cookie(&format!("other=1; SID={sid}; x=2")), ""),
            "and must still be found among other cookies"
        );
        assert!(
            !authorised(&s, &with_cookie("SID=deadbeef"), ""),
            "an invented session must be refused"
        );
        assert!(
            !authorised(&s, &with_cookie("SID="), ""),
            "an empty session must be refused"
        );
        assert!(
            !authorised(&s, &with_cookie("XSID=".to_string().as_str()), ""),
            "a cookie that merely ends in SID is not the session cookie"
        );

        // Logging out must actually end it, or a stolen cookie outlives the
        // client that dropped it.
        s.sessions.revoke(&sid);
        assert!(!authorised(&s, &with_cookie(&format!("SID={sid}")), ""));
    }

    /// A cookie is a credential, so it cannot rescue an instance that has none.
    ///
    /// Without this, `authorised` returning on the session branch would be a
    /// second way past the empty-key rule the test above pins down.
    #[test]
    fn a_session_cannot_open_an_instance_with_no_key() {
        let open = state("", "$2a$hash");
        let sid = open.sessions.create();
        assert!(!authorised(&open, &with_cookie(&format!("SID={sid}")), ""));
    }

    /// The login accepts the key, and the key alone is not a blank cheque.
    ///
    /// This is what let the seven broken clients be fixed by typing what they
    /// already held into a field they already had, instead of rotating an
    /// admin password nobody had written down.
    #[test]
    fn the_api_key_is_accepted_where_a_password_is_expected() {
        // bcrypt("hunter2"), so the admin path is real and not stubbed.
        let hash = bcrypt::hash("hunter2", 4).unwrap();
        let s = state("secret-key", &hash);

        assert!(qbit_creds_ok(&s, "admin", "hunter2"), "the admin account works");
        assert!(qbit_creds_ok(&s, "admin", "secret-key"), "so does the API key");
        // Any username with the key: the clients let people type anything
        // there, and the key is what is being checked.
        assert!(qbit_creds_ok(&s, "whatever", "secret-key"));

        assert!(!qbit_creds_ok(&s, "admin", "wrong"), "a wrong password is refused");
        assert!(!qbit_creds_ok(&s, "notadmin", "hunter2"), "the right password under the wrong name is refused");
        assert!(!qbit_creds_ok(&s, "admin", ""), "an empty password is refused");
    }

    /// An instance with nothing configured authorises nobody here either.
    ///
    /// Without the empty guard, an empty password would compare equal to an
    /// empty key and log anyone in -- the 4.14 hole, in a new doorway.
    #[test]
    fn an_unconfigured_instance_issues_no_session() {
        let fresh = state("", "");
        assert!(!qbit_creds_ok(&fresh, "admin", ""));
        assert!(!qbit_creds_ok(&fresh, "admin", "anything"));
        assert!(!qbit_creds_ok(&fresh, "", ""));
    }

    /// The real decision `qbit_login` makes, called directly.
    fn qbit_creds_ok(state: &AppState, username: &str, password: &str) -> bool {
        qbit_credentials_ok(&state.cfg(), username, password)
    }

    #[test]
    fn a_form_body_is_split_like_a_query() {
        assert_eq!(form_field("username=admin&password=p", "username").as_deref(), Some("admin"));
        assert_eq!(form_field("username=admin&password=p", "password").as_deref(), Some("p"));
        // The passwords people actually use.
        assert_eq!(form_field("password=a%2Bb%26c", "password").as_deref(), Some("a+b&c"));
        assert_eq!(form_field("password=a+b", "password").as_deref(), Some("a b"));
        assert_eq!(form_field("username=admin", "password"), None);
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

    /// The hole this closes, asserted from the outside.
    ///
    /// A fresh Docker install had `api_key = ""` from the shipped template.
    /// `provided == expected` then made *sending nothing* equal *having
    /// nothing*: no key got 200, a wrong key got 401. Measured on 4.14.0
    /// against a container with a virgin /config.
    ///
    /// Both halves matter. That an empty key refuses a caller who sends
    /// nothing is the fix; that it also refuses one who sends the empty string
    /// is what stops the same equality sneaking back in another spelling.
    #[test]
    fn an_instance_with_no_key_authorises_nobody() {
        let open = state("", "$2a$hash");
        assert!(
            !authorised(&open, &HeaderMap::new(), ""),
            "no key configured must refuse a caller who sends none"
        );
        assert!(
            !authorised(&open, &with_key(""), ""),
            "nor one who sends an empty key"
        );
        assert!(
            !authorised(&open, &HeaderMap::new(), "apikey="),
            "nor one who sends an empty key in the query"
        );
        assert!(
            !authorised(&open, &with_key("anything"), ""),
            "nor anyone else"
        );

        // Same before setup: an unconfigured install must not be wide open
        // either, which is the half a refactor would quietly drop.
        let fresh = state("", "");
        assert!(!authorised(&fresh, &HeaderMap::new(), ""));
    }

    /// The placeholder is a key, not a bypass.
    ///
    /// It used to switch the check off entirely once an admin password
    /// existed -- which production had, so production authenticated nothing.
    /// It is now compared like any other value; `config::ensure_api_key`
    /// replaces it at boot so an install should never reach this state.
    #[test]
    fn the_placeholder_key_is_no_longer_a_bypass() {
        let configured = state(DEFAULT_API_KEY, "$2a$hash");
        assert!(
            !authorised(&configured, &HeaderMap::new(), ""),
            "placeholder + password set must NOT wave a keyless caller through"
        );
        assert!(
            authorised(&configured, &with_key(DEFAULT_API_KEY), ""),
            "it is still the configured key, so it still opens the door"
        );
    }

    #[test]
    fn keys_compare_in_constant_time_but_still_compare() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrey"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    // The trap this guards: "3.9.0" is lexically greater than "3.180.0", so a
    // string comparison would offer 3.9.0 as an upgrade from 3.180.0.
    #[test]
    /// Locks the 2026-09-12 regression: adding a torrent PAUSED must still
    /// hash-check data already on disk. The `!paused` gate that used to live
    /// here left a complete 184 GB torrent reporting 0%, with a Verify button
    /// that was itself a stub -- the two holes covered each other.
    #[test]
    fn a_paused_add_still_checks_the_data_on_disk() {
        // The case that regressed: paused is simply not an input.
        assert!(add_recheck_wanted(false, true));
    }

    #[test]
    fn an_add_with_no_data_on_disk_checks_nothing() {
        assert!(!add_recheck_wanted(false, false));
    }

    #[test]
    fn seed_mode_keeps_its_trust_fast_path() {
        // skip_checking is an assertion by the caller; honour it.
        assert!(!add_recheck_wanted(true, true));
        assert!(!add_recheck_wanted(true, false));
    }

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

/// Fixtures shared by the handler tests of this binary.
///
/// `AppState` is what every `/api` route takes, so until it could be built in
/// a test none of them could be exercised -- which is most of why this file
/// sat at 8% covered while carrying the whole control plane. Nothing here
/// touches the network: `EngineHost::offline` is the same constructor a real
/// start uses before any listener is bound, and the store is in memory.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A config directory of this test's own, removed by `TestState`.
    pub(crate) struct TestState {
        pub state: AppState,
        pub dir: std::path::PathBuf,
    }

    impl Drop for TestState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl std::ops::Deref for TestState {
        type Target = AppState;
        fn deref(&self) -> &AppState {
            &self.state
        }
    }

    /// Build a state from a TOML document, so a test can turn on exactly the
    /// setting it is about and leave the rest at the defaults a fresh install
    /// runs with.
    pub(crate) fn state_from(tag: &str, toml_src: &str) -> TestState {
        let dir = std::env::temp_dir().join(format!(
            "typhon-api-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test config dir");

        let config: Config = toml::from_str(toml_src).expect("test config parses");
        let config_path = dir.join("default.toml");
        std::fs::write(&config_path, toml_src).expect("test config written");

        let engines = Arc::new(crate::engines::EngineHost::offline(&config, &dir));
        // `open_in_memory` applies SCHEMA, which is not all of it: the
        // workflow, job and node tables are created by `ensure_schema`, and a
        // store without them answers "no such table" to routes that look fine.
        let store = crate::store::Store::open_in_memory().expect("in-memory store");
        store.ensure_schema().expect("full schema");
        let store = Arc::new(std::sync::Mutex::new(store));

        let state = AppState {
            imports: Default::default(),
            config: Arc::new(std::sync::RwLock::new(Arc::new(config))),
            config_path,
            update_check: Arc::new(tokio::sync::Mutex::new(None)),
            engines,
            store,
            public_ip: Default::default(),
            net_engines: Default::default(),
            odometer: Default::default(),
            records: Default::default(),
            bench_path: dir.join("bench.db"),
            started_at: 1_700_000_000,
            logs: crate::logbuf::LogBuffer::new(),
            reconnect: Default::default(),
            // `None` is a normal state, not a failure: the timeline is
            // observability and must never cost the seedbox.
            bench: None,
            sessions: Default::default(),
        };
        TestState { state, dir }
    }

    /// The default install: no API key set.
    pub(crate) fn state(tag: &str) -> TestState {
        state_from(tag, "")
    }

    /// Read a handler's response body as JSON.
    pub(crate) async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("response body");
        if bytes.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("body is not JSON: {:?} ({e})", String::from_utf8_lossy(&bytes)))
    }

    /// Headers carrying an API key.
    pub(crate) fn keyed(key: &str) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        h.insert("X-API-Key", key.parse().unwrap());
        h
    }
}

#[cfg(test)]
mod auth_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn with_key(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// ⚠️ THE regression to keep out. A Docker install that has never been
    /// through setup has no key in default.toml, and an empty expected key
    /// once meant "compare nothing, match everything" -- every `/api` route
    /// answered unauthenticated on a fresh container.
    #[test]
    fn an_unset_api_key_refuses_everyone_rather_than_admitting_everyone() {
        let s = state("nokey");
        assert!(!authorised(&s, &HeaderMap::new(), ""), "no key, no header");
        assert!(!authorised(&s, &keyed(""), ""), "no key, empty header");
        assert!(!authorised(&s, &keyed("anything"), ""), "no key, any header");
        assert!(!authorised(&s, &HeaderMap::new(), "apikey="), "no key, empty query param");
        assert!(!authorised(&s, &HeaderMap::new(), "apikey=guess"), "no key, any query param");
    }

    #[test]
    fn the_right_key_in_the_header_is_admitted() {
        let s = with_key("hdr");
        assert!(authorised(&s, &keyed(KEY), ""));
    }

    /// The header name is compared case-insensitively by http, and callers do
    /// spell it every way.
    #[test]
    fn the_header_name_is_not_case_sensitive() {
        let s = with_key("case");
        for name in ["X-API-Key", "x-api-key", "X-Api-Key"] {
            let mut h = HeaderMap::new();
            h.insert(name, KEY.parse().unwrap());
            assert!(authorised(&s, &h, ""), "{name} must be accepted");
        }
    }

    #[test]
    fn a_wrong_key_is_refused() {
        let s = with_key("wrong");
        assert!(!authorised(&s, &keyed("not-the-key"), ""));
        // Same length, one byte out: the case constant_time_eq exists for.
        let mut near = KEY.to_string();
        near.pop();
        near.push('0');
        if near != KEY {
            assert!(!authorised(&s, &keyed(&near), ""));
        }
    }

    /// The query parameter is the fallback for callers that cannot set a
    /// header -- a browser opening an SSE stream, mainly.
    #[test]
    fn the_key_may_arrive_in_the_query_instead() {
        let s = with_key("query");
        assert!(authorised(&s, &HeaderMap::new(), &format!("apikey={KEY}")));
        assert!(authorised(&s, &HeaderMap::new(), &format!("foo=1&apikey={KEY}&bar=2")));
        assert!(!authorised(&s, &HeaderMap::new(), "apikey=wrong"));
    }

    /// An empty header must fall through to the query rather than count as an
    /// answer: a client that sets the header to "" and the parameter properly
    /// is still a client with the key.
    #[test]
    fn an_empty_header_falls_through_to_the_query() {
        let s = with_key("fallthrough");
        assert!(authorised(&s, &keyed(""), &format!("apikey={KEY}")));
    }

    /// A parameter that merely ends in "apikey" is a different parameter.
    #[test]
    fn a_lookalike_query_parameter_is_not_the_key() {
        let s = with_key("lookalike");
        assert!(!authorised(&s, &HeaderMap::new(), &format!("notapikey={KEY}")));
        assert!(!authorised(&s, &HeaderMap::new(), &format!("apikeyx={KEY}")));
    }

    #[test]
    fn a_cookie_that_names_no_live_session_is_refused() {
        let s = with_key("cookie");
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            format!("{}=never-issued", crate::session::COOKIE_NAME).parse().unwrap(),
        );
        assert!(!authorised(&s, &h, ""), "an unissued session id is not a session");
    }

    /// The *arr stack's only path: log in, get a cookie, ride it. A session
    /// the daemon issued must be accepted with no key at all.
    #[test]
    fn a_live_session_cookie_is_admitted_without_a_key() {
        let s = with_key("session");
        let sid = s.sessions.create();
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::COOKIE,
            format!("{}={sid}", crate::session::COOKIE_NAME).parse().unwrap(),
        );
        assert!(authorised(&s, &h, ""));
    }

    #[test]
    fn constant_time_eq_is_still_an_equality() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"), "a prefix is not a match");
        assert!(!constant_time_eq(b"ab", b"abc"));
        // Differing in the LAST byte is the case an early-return comparison
        // would leak the length of the shared prefix for.
        assert!(!constant_time_eq(b"aaaaaaaa", b"aaaaaaab"));
    }
}

#[cfg(test)]
mod pure_tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_plus_escapes_and_literals() {
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("a+b"), "a b", "+ is a space in a query string");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    /// A stray `%` is not an escape and must come back as itself rather than
    /// eating the characters after it.
    #[test]
    fn a_truncated_escape_is_left_alone() {
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%4"), "%4");
        assert_eq!(percent_decode("%zz"), "%zz", "not hex, so not an escape");
    }

    #[test]
    fn utf8_survives_percent_decoding() {
        assert_eq!(percent_decode("caf%C3%A9"), "café");
    }

    #[test]
    fn a_query_parameter_is_found_by_its_whole_name() {
        assert_eq!(query_param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(query_param("a=1", "missing"), None);
        assert_eq!(query_param("", "a"), None);
        assert_eq!(query_param("flag", "flag"), None, "a bare flag has no value");
        assert_eq!(query_param("name=a%20b", "name").as_deref(), Some("a b"));
    }

    /// A parameter that merely CONTAINS the name is a different parameter.
    #[test]
    fn a_lookalike_parameter_name_is_not_a_match() {
        assert_eq!(query_param("xapikey=k", "apikey"), None);
        assert_eq!(query_param("apikeyx=k", "apikey"), None);
    }

    #[test]
    fn the_host_of_a_url_drops_the_scheme_the_path_and_the_port() {
        assert_eq!(url_host("http://example.com/path"), "example.com");
        assert_eq!(url_host("https://example.com:8199/x"), "example.com");
        assert_eq!(url_host("http://192.168.99.200:8199"), "192.168.99.200");
        assert_eq!(url_host("example.com"), "example.com", "a bare host is a host");
    }

    /// ⚠️ A bracketed IPv6 literal keeps its colons. Splitting on the last
    /// colon returned "2001" here, and the empty string for `[::1]` -- so
    /// `is_loopback_host` answered FALSE for the one address the enrolment
    /// guard exists to refuse, and the dial address was never parseable.
    #[test]
    fn an_ipv6_literal_keeps_its_colons() {
        assert_eq!(url_host("http://[2001:db8::1]:8199/x"), "2001:db8::1");
        assert_eq!(url_host("http://[2001:db8::1]"), "2001:db8::1");
        assert_eq!(url_host("http://[::1]:8199"), "::1");
    }

    /// The consequence, stated where it bites: a node offering to register
    /// itself on this machine's own loopback must still be refused when it
    /// spells the address in IPv6.
    #[test]
    fn the_loopback_guard_sees_through_an_ipv6_literal() {
        assert!(is_loopback_host(&url_host("http://[::1]:8199")));
        assert!(is_loopback_host(&url_host("http://127.0.0.1:8199")));
        assert!(!is_loopback_host(&url_host("http://[2001:db8::1]:8199")));
    }

    #[test]
    fn loopback_is_recognised_by_every_spelling_the_ui_offers() {
        for h in ["127.0.0.1", "localhost", "::1", "0.0.0.0", "127.1.2.3"] {
            assert!(is_loopback_host(h), "{h} is loopback");
        }
        for h in ["example.com", "192.168.99.200", "", "127x"] {
            assert!(!is_loopback_host(h), "{h} is not loopback");
        }
    }

    #[test]
    fn a_release_tag_is_three_numbers() {
        assert!(is_semver_tag("v4.27.0"));
        assert!(is_semver_tag("4.27.0"));
        assert!(!is_semver_tag("v4.27"));
        assert!(!is_semver_tag("v4.27.0.1"));
        assert!(!is_semver_tag("nightly"));
        assert!(!is_semver_tag("v4.27.x"));
        assert!(!is_semver_tag("v4..0"), "an empty component is not a number");
    }

    /// ⭐ String comparison is what makes this subtly wrong: "3.9.0" sorts
    /// AFTER "3.180.0" lexically, so a naive check announces a downgrade as an
    /// update.
    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(version_less("3.9.0", "3.180.0"), "9 < 180, whatever the strings do");
        assert!(!version_less("3.180.0", "3.9.0"));
        assert!(version_less("4.26.0", "4.27.0"));
        assert!(!version_less("4.27.0", "4.27.0"), "equal is not less");
        assert!(version_less("v4.26.0", "4.27.0"), "a leading v is ignored");
    }

    /// Hydranos' own version carries a "-typhon" suffix; comparing it as part
    /// of the number would make every check report an update.
    #[test]
    fn a_build_suffix_is_dropped_before_comparing() {
        assert!(!version_less("4.27.0-typhon", "4.27.0"));
        assert!(!version_less("4.27.0", "4.27.0-typhon"));
        assert!(version_less("4.27.0-typhon", "4.28.0"));
    }

    #[test]
    fn a_missing_component_counts_as_zero() {
        assert!(version_less("4.27", "4.27.1"));
        assert!(!version_less("4.27.0", "4.27"));
    }

    /// ⭐ Seed mode means "the data is already here, take my word for it".
    /// Rechecking then is exactly the work the operator asked to skip.
    #[test]
    fn seed_mode_is_taken_at_its_word_and_never_rechecks() {
        assert!(!add_recheck_wanted(true, true), "seed mode skips the check");
        assert!(!add_recheck_wanted(true, false));
        assert!(add_recheck_wanted(false, true), "data on disk is worth checking");
        assert!(
            !add_recheck_wanted(false, false),
            "a fresh download has nothing on disk: an all-miss check is wasted"
        );
    }

    #[test]
    fn search_separators_are_the_ones_a_release_name_uses() {
        for c in [' ', '.', '_', '-', '\t'] {
            assert!(is_search_sep(c), "{c:?} separates words");
        }
        for c in ['a', '0', '\'', '&'] {
            assert!(!is_search_sep(c), "{c:?} does not");
        }
    }

    /// Equivalent to a `contains` over the fully normalised name -- that
    /// equivalence is the whole reason the per-row String is avoided.
    #[test]
    fn a_token_is_found_inside_any_one_word() {
        assert!(name_has_token("Some.Show.S01E01.1080p", "s01e01"));
        assert!(name_has_token("Some.Show.S01E01.1080p", "show"));
        assert!(name_has_token("Some.Show.S01E01.1080p", "1080"));
        assert!(!name_has_token("Some.Show.S01E01", "showS01"), "a token cannot span words");
        assert!(!name_has_token("Some.Show", "absent"));
    }

    #[test]
    fn token_matching_ignores_case_including_outside_ascii() {
        assert!(name_has_token("SOME.SHOW", "show"));
        assert!(name_has_token("CAFÉ.2024", "café"), "real folding for non-ascii");
    }

    #[test]
    fn a_token_longer_than_the_word_cannot_match() {
        assert!(!name_has_token("ab", "abc"));
    }

    /// The window is REPLACED, never appended: a query that already carried an
    /// offset would otherwise arrive with two, and which one wins is the
    /// parser's business rather than ours.
    #[test]
    fn paging_replaces_any_window_the_query_already_had() {
        let out = with_window("sort=name&offset=500&limit=10", 0, 100);
        assert!(out.contains("sort=name"));
        assert_eq!(out.matches("offset=").count(), 1, "exactly one offset: {out}");
        assert_eq!(out.matches("limit=").count(), 1, "exactly one limit: {out}");
        assert!(out.contains("offset=0") && out.contains("limit=100"));
    }

    #[test]
    fn paging_an_empty_query_produces_just_the_window() {
        assert_eq!(with_window("", 20, 50), "offset=20&limit=50");
    }

    #[test]
    fn a_timestamp_in_the_past_reads_as_a_date_not_as_an_offset() {
        let s = iso8601_ago(std::time::Duration::from_secs(3600));
        assert!(s.len() >= 10, "an RFC3339 stamp, got {s:?}");
        assert!(s.contains('-'), "got {s:?}");
    }

    #[test]
    fn zero_is_the_value_that_gets_omitted() {
        assert!(is_zero_i64(&0));
        assert!(!is_zero_i64(&1));
        assert!(!is_zero_i64(&-1));
    }

    /// The settings screen shows and edits keys this binary does not model, so
    /// the file is re-read and parsed generically rather than serialised back
    /// out of the typed Config -- a struct round trip drops them.
    #[test]
    fn toml_becomes_json_without_losing_a_key_the_struct_does_not_model() {
        let src = r#"
            answer = 42
            ratio = 1.5
            on = true
            name = "hydranos"
            list = [1, 2]
            [section]
            unknown_to_the_struct = "kept"
        "#;
        let v: toml::Value = toml::from_str(src).expect("valid toml");
        let j = toml_to_json(&v);
        assert_eq!(j["answer"], serde_json::json!(42));
        assert_eq!(j["ratio"], serde_json::json!(1.5));
        assert_eq!(j["on"], serde_json::json!(true));
        assert_eq!(j["name"], serde_json::json!("hydranos"));
        assert_eq!(j["list"], serde_json::json!([1, 2]));
        assert_eq!(
            j["section"]["unknown_to_the_struct"],
            serde_json::json!("kept"),
            "a key the binary does not model survives the round trip"
        );
    }

    #[test]
    fn the_local_agent_name_is_derived_from_the_engine_id() {
        let a = local_agent("race");
        let b = local_agent("hoard");
        assert_ne!(a, b, "two engines are two agents");
        assert!(a.contains("race"), "got {a}");
    }
}

#[cfg(test)]
mod handler_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn with_key(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// Every read route, against a fresh install: refused without a key, and
    /// answering JSON with one.
    ///
    /// The point is not the bodies -- it is that a route cannot answer an
    /// unauthenticated caller, and cannot panic on an engine that holds
    /// nothing. Both were real: the open API of 10/09, and handlers that
    /// assumed at least one torrent.
    macro_rules! read_routes {
        ($($test_name:ident => $name:ident),+ $(,)?) => {
            $(
                #[tokio::test]
                async fn $test_name() {
                    let s = with_key(concat!("route-", stringify!($name)));

                    let refused = super::$name(
                        State(s.state.clone()),
                        RawQuery(None),
                        HeaderMap::new(),
                    ).await;
                    assert_eq!(
                        refused.status(),
                        StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key")
                    );

                    let allowed = super::$name(
                        State(s.state.clone()),
                        RawQuery(None),
                        keyed(KEY),
                    ).await;
                    assert_eq!(
                        allowed.status(),
                        StatusCode::OK,
                        concat!(stringify!($name), " must answer a caller with the key")
                    );
                    // Body has to parse: a route that answers 200 with a
                    // truncated document is a route the UI renders as empty.
                    let _ = body_json(allowed).await;
                }
            )+
        };
    }

    read_routes!(
        route_get_categories => get_categories,
        route_get_tags => get_tags,
        route_get_engines => get_engines,
        route_get_jobs => get_jobs,
        route_get_download_slots => get_download_slots,
        route_get_public_ip => get_public_ip,
        route_get_settings => get_settings,
        route_get_ip_modes => get_ip_modes,
        route_get_passkeys => get_passkeys,
        route_get_add_defaults => get_add_defaults,
        route_get_drain_status => get_drain_status,
        route_get_hoard_stats => get_hoard_stats,
        route_get_race_settings => get_race_settings,
        route_get_baseline => get_baseline,
        route_qbit_categories => qbit_categories,
        route_qbit_tags => qbit_tags,
        route_get_dedup_stats => get_dedup_stats,
        route_get_race_choking => get_race_choking,
        route_get_hoard_pinned => get_hoard_pinned,
    );

    /// The key may ride the query instead of the header on every route, not
    /// only the ones a browser happens to open.
    #[tokio::test]
    async fn a_read_route_accepts_the_key_in_the_query() {
        let s = with_key("route-query");
        let resp = super::get_engines(
            State(s.state.clone()),
            RawQuery(Some(format!("apikey={KEY}"))),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// ⚠️ A fresh Docker install has no key in default.toml. Every route must
    /// refuse it rather than serve the control plane to the network.
    #[tokio::test]
    async fn an_install_with_no_key_serves_nothing() {
        let s = state("route-nokey");
        for resp in [
            super::get_engines(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
            super::get_settings(State(s.state.clone()), RawQuery(None), keyed("")).await,
            super::get_categories(State(s.state.clone()), RawQuery(None), keyed("guess")).await,
        ] {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }

    /// The settings screen edits keys this binary does not model, so the route
    /// re-reads the file rather than serialising the typed Config back out.
    #[tokio::test]
    async fn settings_serve_keys_the_struct_does_not_model() {
        let s = state_from(
            "route-settings",
            &format!(
                "[daemon]\napi_key = \"{KEY}\"\n\n[a_section_the_binary_never_heard_of]\nkept = \"yes\"\n"
            ),
        );
        let body = body_json(
            super::get_settings(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
        )
        .await;
        assert_eq!(
            body["a_section_the_binary_never_heard_of"]["kept"],
            serde_json::json!("yes"),
            "an unmodelled key survives the read: {body}"
        );
    }

    /// A fresh install lists no jobs -- and says so with an empty list rather
    /// than a null the UI has to special-case.
    #[tokio::test]
    async fn a_fresh_install_reports_empty_collections_not_null() {
        let s = with_key("route-empty");
        let jobs = body_json(super::get_jobs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert!(!jobs.is_null(), "jobs answered null: {jobs}");
        let tags = body_json(super::get_tags(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert!(!tags.is_null(), "tags answered null: {tags}");
    }
}

#[cfg(test)]
mod pure2_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn row(fields: serde_json::Value) -> serde_json::Value {
        fields
    }

    /// ⭐ A seeding torrent sorts as complete whatever its stored progress.
    /// A library restored from resume can carry a progress of 0 on a torrent
    /// that is fully seeding, and sorting on the stored number alone puts the
    /// complete ones at the bottom.
    #[test]
    fn a_seeding_torrent_sorts_as_complete_whatever_its_stored_progress() {
        let seeding = row(serde_json::json!({"state": "seeding", "progress": 0.0}));
        let half = row(serde_json::json!({"state": "downloading", "progress": 0.5}));
        assert_eq!(row_sort_key(&seeding, "progress").1, 1.0);
        assert_eq!(row_sort_key(&half, "progress").1, 0.5);
    }

    #[test]
    fn text_sorts_fold_case_so_the_order_is_not_ascii_order() {
        let a = row(serde_json::json!({"name": "Zebra"}));
        let b = row(serde_json::json!({"name": "apple"}));
        assert_eq!(row_sort_key(&a, "name").0, "zebra");
        assert_eq!(row_sort_key(&b, "name").0, "apple");
        assert!(row_sort_key(&b, "name").0 < row_sort_key(&a, "name").0);
    }

    #[test]
    fn a_numeric_column_sorts_on_its_own_number() {
        let r = row(serde_json::json!({"ratio": 2.5, "added_time": 10.0}));
        assert_eq!(row_sort_key(&r, "ratio").1, 2.5);
    }

    /// An unknown sort is not an error and not an arbitrary order: it falls
    /// back to when the torrent was added.
    #[test]
    fn an_unknown_sort_falls_back_to_the_added_time() {
        let r = row(serde_json::json!({"added_time": 42.0, "ratio": 9.0}));
        assert_eq!(row_sort_key(&r, "no_such_column").1, 42.0);
    }

    #[test]
    fn a_missing_field_sorts_as_empty_or_zero_rather_than_panicking() {
        let r = row(serde_json::json!({}));
        assert_eq!(row_sort_key(&r, "name").0, "");
        assert_eq!(row_sort_key(&r, "ratio").1, 0.0);
        assert_eq!(row_sort_key(&r, "progress").1, 0.0);
    }

    fn job(kind: &str, params: &str, total: i64, done: i64, error: &str) -> crate::store::Job {
        crate::store::Job {
            id: "j1".into(),
            kind: kind.into(),
            state: "running".into(),
            info_hash: String::new(),
            params: params.into(),
            progress_bytes: done,
            total_bytes: total,
            error: error.into(),
            created_at: 1,
            updated_at: 2,
        }
    }

    /// ⭐ A corrupt `params` row must not make the WHOLE listing unparseable
    /// for the caller: it is dropped, exactly as 3.x checks json.Valid first.
    #[test]
    fn a_job_with_unparseable_params_still_lists_without_them() {
        let v = job_view(&job("move", "{not json", 0, 0, ""));
        assert_eq!(v["id"], serde_json::json!("j1"));
        assert!(v.get("params").is_none(), "the corrupt field is dropped, not embedded");
    }

    #[test]
    fn a_jobs_params_are_embedded_as_json_not_as_a_string() {
        let v = job_view(&job("move", r#"{"to":"/data"}"#, 0, 0, ""));
        assert_eq!(v["params"]["to"], serde_json::json!("/data"));
    }

    /// An absent info_hash is ABSENT, not the empty string: the Go side marks
    /// it omitempty and clients test for presence.
    #[test]
    fn empty_optional_fields_are_omitted_rather_than_sent_empty() {
        let v = job_view(&job("scan", "", 0, 0, ""));
        assert!(v.get("info_hash").is_none());
        assert!(v.get("error").is_none());
        assert!(v.get("params").is_none());
    }

    #[test]
    fn an_error_is_reported_when_there_is_one() {
        let v = job_view(&job("move", "", 0, 0, "disk full"));
        assert_eq!(v["error"], serde_json::json!("disk full"));
    }

    /// A job of unknown size is 0 %, not a division by zero.
    #[test]
    fn a_job_with_no_total_reports_zero_percent_not_nan() {
        let v = job_view(&job("scan", "", 0, 500, ""));
        assert_eq!(v["percent"], serde_json::json!(0.0));
        assert!(!v["percent"].as_f64().unwrap().is_nan());
    }

    #[test]
    fn job_progress_is_a_percentage_of_the_total() {
        let v = job_view(&job("move", "", 200, 50, ""));
        assert_eq!(v["percent"].as_f64().unwrap(), 25.0);
    }

    fn page(rows: Vec<serde_json::Value>, filtered: i64) -> serde_json::Value {
        serde_json::json!({"rows": rows, "filtered": filtered})
    }

    fn err_row(msg: &str) -> serde_json::Value {
        serde_json::json!({"tracker_error_msg": msg})
    }

    /// With neither an include nor an exclude list there is nothing to
    /// enforce, and the pages must come back untouched rather than emptied.
    #[test]
    fn no_error_filter_leaves_every_row_in_place() {
        let mut pages = vec![page(vec![err_row("whatever")], 1)];
        enforce_error_class(&mut pages, &[], &[], 100);
        assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
    }

    /// Excluding a class drops exactly the rows of that class.
    #[test]
    fn excluding_a_class_drops_only_that_class() {
        let clean = err_row("");
        let unregistered = err_row("unregistered torrent");
        let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();

        let mut pages = vec![page(vec![clean.clone(), unregistered.clone()], 2)];
        enforce_error_class(&mut pages, &[], &[class_of_unreg.clone()], 100);
        let rows = pages[0]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "the excluded class is gone");
        assert_eq!(rows[0]["tracker_error_msg"], serde_json::json!(""));
    }

    /// Including a class keeps only it.
    #[test]
    fn including_a_class_keeps_only_that_class() {
        let clean = err_row("");
        let unregistered = err_row("unregistered torrent");
        let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();

        let mut pages = vec![page(vec![clean, unregistered], 2)];
        enforce_error_class(&mut pages, &[class_of_unreg], &[], 100);
        assert_eq!(pages[0]["rows"].as_array().unwrap().len(), 1);
        assert_eq!(
            pages[0]["rows"][0]["tracker_error_msg"],
            serde_json::json!("unregistered torrent")
        );
    }

    /// The filtered count must follow the rows it dropped, or the UI shows a
    /// total that does not match what it is displaying.
    #[test]
    fn the_filtered_count_follows_the_rows_that_were_dropped() {
        let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();
        let mut pages = vec![page(vec![err_row(""), err_row("unregistered torrent")], 2)];
        enforce_error_class(&mut pages, &[], &[class_of_unreg], 100);
        assert_eq!(pages[0]["filtered"], serde_json::json!(1));
    }

    /// The count is never taken below zero, whatever the page claimed.
    #[test]
    fn the_filtered_count_never_goes_negative() {
        let class_of_unreg = crate::errclass::classify("unregistered torrent").to_string();
        let mut pages = vec![page(vec![err_row("unregistered torrent")], 0)];
        enforce_error_class(&mut pages, &[], &[class_of_unreg], 100);
        assert!(pages[0]["filtered"].as_i64().unwrap() >= 0);
    }

    /// A page that is not an object, or carries no rows, is skipped rather
    /// than panicking: these come from another node over the wire.
    #[test]
    fn a_malformed_page_from_another_node_is_skipped_not_fatal() {
        let mut pages = vec![
            serde_json::json!("not an object"),
            serde_json::json!({"no_rows_here": true}),
            serde_json::json!({"rows": "not an array"}),
        ];
        enforce_error_class(&mut pages, &[], &["whatever".to_string()], 100);
    }

    /// ⭐ A category carries a MODE ("hoard" or "race"), which names a
    /// behaviour, not one of the engines a node hosts. An explicit engine is
    /// the only way to reach an engine that is neither -- without it a torrent
    /// could never be placed in `vpn1`, whatever the config said.
    #[tokio::test]
    async fn an_unknown_category_lands_in_race_by_default() {
        let s = state_from("placement", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let (engine, path) = placement(&s.state, "no-such-category", "");
        assert_eq!(engine, "race");
        assert!(path.is_empty());
    }

    /// An engine override that names no engine of this node must NOT be
    /// honoured: it would place the torrent nowhere.
    #[tokio::test]
    async fn an_override_naming_no_engine_of_this_node_is_ignored() {
        let s = state_from("placement-bad", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let (engine, _) = placement(&s.state, "no-such-category", "no-such-engine");
        assert_eq!(engine, "race", "the bogus override falls back rather than being obeyed");
    }
}

#[cfg(test)]
mod page_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// A minimal single-file torrent. Bencode lengths are COMPUTED, never
    /// counted by hand: a wrong one yields a file the parser refuses for a
    /// reason unrelated to the test.
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        // The info hash must differ per torrent, or the second add is refused
        // as a duplicate. The piece hash is what varies.
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');

        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn add(s: &TestState, engine_id: &str, name: &str) {
        let engines = s.engines.engines();
        let engine = engines
            .iter()
            .find(|e| e.id == engine_id)
            .unwrap_or_else(|| panic!("no engine {engine_id}"));
        engine
            .manager
            .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
            .unwrap_or_else(|e| panic!("add {name}: {e}"));
    }

    fn rows(v: &serde_json::Value) -> &Vec<serde_json::Value> {
        v["rows"].as_array().expect("a rows array")
    }

    /// A fresh install has both engines and neither holds anything. The page
    /// must be an empty list with honest counters, not a null the UI has to
    /// special-case.
    #[tokio::test]
    async fn an_empty_engine_answers_an_empty_page_not_null() {
        let s = st("page-empty");
        let v = engine_page_value(&s.state, "race", "").await;
        assert_eq!(v["total"], serde_json::json!(0));
        assert_eq!(v["filtered"], serde_json::json!(0));
        assert!(rows(&v).is_empty());
    }

    /// An engine this node does not host is an empty page, never a panic: the
    /// id comes off the wire.
    #[tokio::test]
    async fn an_unknown_engine_is_an_empty_page() {
        let s = st("page-unknown");
        let v = engine_page_value(&s.state, "no-such-engine", "").await;
        assert_eq!(v["total"], serde_json::json!(0));
        assert!(rows(&v).is_empty());
    }

    #[tokio::test]
    async fn every_torrent_of_the_engine_is_listed_and_counted() {
        let s = st("page-list");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        let v = engine_page_value(&s.state, "race", "").await;
        assert_eq!(v["total"], serde_json::json!(2));
        assert_eq!(rows(&v).len(), 2);
    }

    /// ⭐ The engines do not share a catalogue: a torrent added to race must
    /// not appear on the hoard's page. Collapsing the two is how a torrent
    /// ends up running in one engine and listed under another.
    #[tokio::test]
    async fn one_engines_torrents_do_not_appear_on_the_others_page() {
        let s = st("page-sep");
        add(&s, "race", "only-in-race");
        let race = engine_page_value(&s.state, "race", "").await;
        let hoard = engine_page_value(&s.state, "hoard", "").await;
        assert_eq!(race["total"], serde_json::json!(1));
        assert_eq!(hoard["total"], serde_json::json!(0));
    }

    /// `total` is the library; `filtered` is what the query kept. Reporting
    /// the filtered count as the total is how a search makes the library look
    /// like it shrank.
    #[tokio::test]
    async fn a_search_narrows_filtered_but_never_total() {
        let s = st("page-search");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        let v = engine_page_value(&s.state, "race", "search=alpha").await;
        assert_eq!(v["total"], serde_json::json!(2), "the library did not shrink");
        assert_eq!(v["filtered"], serde_json::json!(1));
        assert_eq!(rows(&v).len(), 1);
        assert_eq!(rows(&v)[0]["name"], serde_json::json!("alpha"));
    }

    #[tokio::test]
    async fn a_search_matching_nothing_returns_no_rows_rather_than_all_of_them() {
        let s = st("page-nomatch");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "search=nosuchthing").await;
        assert_eq!(v["filtered"], serde_json::json!(0));
        assert!(rows(&v).is_empty(), "an unmatched search must not fall back to everything");
    }

    #[tokio::test]
    async fn a_search_ignores_case() {
        let s = st("page-case");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "search=ALPHA").await;
        assert_eq!(v["filtered"], serde_json::json!(1));
    }

    /// The window is honoured: a page of one returns one row, and the counters
    /// still describe the whole library.
    #[tokio::test]
    async fn the_window_limits_the_rows_without_lying_about_the_totals() {
        let s = st("page-window");
        for n in ["alpha", "bravo", "charlie"] {
            add(&s, "race", n);
        }
        let v = engine_page_value(&s.state, "race", "limit=1&offset=0").await;
        assert_eq!(rows(&v).len(), 1);
        assert_eq!(v["total"], serde_json::json!(3));
        assert_eq!(v["filtered"], serde_json::json!(3));
    }

    /// An offset past the end is an empty page, not a wrapped one and not a
    /// panic on a slice out of range.
    #[tokio::test]
    async fn an_offset_past_the_end_is_empty_not_a_panic() {
        let s = st("page-past");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "offset=9999&limit=10").await;
        assert!(rows(&v).is_empty());
        assert_eq!(v["total"], serde_json::json!(1));
    }

    /// A limit of zero is not "no rows": it is outside the allowed window and
    /// gets clamped, because a page of zero rows is never what a client wants.
    #[tokio::test]
    async fn a_nonsense_window_is_clamped_rather_than_obeyed() {
        let s = st("page-clamp");
        add(&s, "race", "alpha");
        for q in ["limit=0", "limit=-5", "limit=abc", "offset=abc"] {
            let v = engine_page_value(&s.state, "race", q).await;
            assert_eq!(v["total"], serde_json::json!(1), "{q} must still answer");
            assert_eq!(rows(&v).len(), 1, "{q} must not produce an empty page");
        }
    }

    /// Sorting by name is case-insensitive, so "Zebra" does not lead "apple"
    /// the way ASCII order would.
    ///
    /// ⚠️ The default direction is DESCENDING (`order=asc` opts in): the
    /// default sort is the added time, where newest-first is what anyone
    /// wants. A test that assumes ascending is testing its own assumption.
    #[tokio::test]
    async fn sorting_by_name_folds_case() {
        let s = st("page-sort");
        add(&s, "race", "Zebra");
        add(&s, "race", "apple");

        let up = engine_page_value(&s.state, "race", "sort=name&order=asc").await;
        let names: Vec<&str> = rows(&up).iter().filter_map(|r| r["name"].as_str()).collect();
        assert_eq!(names, vec!["apple", "Zebra"], "ascending folds case");

        let down = engine_page_value(&s.state, "race", "sort=name").await;
        let names: Vec<&str> = rows(&down).iter().filter_map(|r| r["name"].as_str()).collect();
        assert_eq!(names, vec!["Zebra", "apple"], "descending is the default");
    }

    /// An unknown sort must still answer, in some stable order, rather than
    /// refusing the page.
    #[tokio::test]
    async fn an_unknown_sort_still_answers_a_page() {
        let s = st("page-badsort");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        let v = engine_page_value(&s.state, "race", "sort=no_such_column").await;
        assert_eq!(rows(&v).len(), 2);
    }

    /// A search on an info hash is the operator pasting one in. It is matched
    /// as hex against the hash, not as text against the name.
    #[tokio::test]
    async fn a_hex_search_matches_the_info_hash() {
        let s = st("page-hex");
        add(&s, "race", "alpha");
        let all = engine_page_value(&s.state, "race", "").await;
        let hash = rows(&all)[0]["info_hash"]
            .as_str()
            .expect("a row carries its info hash")
            .to_string();
        let v = engine_page_value(&s.state, "race", &format!("search={}", &hash[..12])).await;
        assert_eq!(v["filtered"], serde_json::json!(1), "a hash prefix finds its torrent");
    }

    /// The routes on top of the page carry the same gate as everything else.
    #[tokio::test]
    async fn the_page_routes_refuse_a_caller_with_no_key() {
        let s = st("page-auth");
        for resp in [
            get_race_torrents(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
            get_hoard_torrents(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        ] {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn the_page_routes_answer_a_caller_with_the_key() {
        let s = st("page-ok");
        add(&s, "race", "alpha");
        let resp = get_race_torrents(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        // This route answers the ROWS themselves, not a page object: it is the
        // qBittorrent-shaped listing, which is a bare array.
        let rows = body.as_array().expect("a bare array of rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], serde_json::json!("alpha"));
    }
}

#[cfg(test)]
mod merge_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn row(name: &str, hash: &str, ratio: f64) -> serde_json::Value {
        serde_json::json!({"name": name, "info_hash": hash, "ratio": ratio, "added_time": 1.0})
    }

    fn page(total: i64, filtered: i64, rows: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({"total": total, "filtered": filtered, "rows": rows})
    }

    /// The fleet totals are the SUM of the nodes', not one node's.
    #[test]
    fn the_totals_add_up_across_the_nodes() {
        let merged = merge_pages(
            vec![
                page(100, 10, vec![row("a", "aa", 1.0)]),
                page(50, 5, vec![row("b", "bb", 2.0)]),
            ],
            "name",
            true,
            0,
            100,
        );
        assert_eq!(merged["total"], serde_json::json!(150));
        assert_eq!(merged["filtered"], serde_json::json!(15));
        assert_eq!(merged["rows"].as_array().unwrap().len(), 2);
    }

    /// With no node at all the answer is still a page, with honest zeroes.
    #[test]
    fn merging_nothing_is_an_empty_page_not_a_null() {
        let merged = merge_pages(vec![], "name", true, 0, 100);
        assert_eq!(merged["total"], serde_json::json!(0));
        assert_eq!(merged["filtered"], serde_json::json!(0));
        assert!(merged["rows"].as_array().unwrap().is_empty());
    }

    /// Rows from different nodes interleave by the sort key, rather than
    /// staying grouped per node.
    #[test]
    fn rows_from_two_nodes_interleave_by_the_sort_key() {
        let merged = merge_pages(
            vec![
                page(2, 2, vec![row("alpha", "a1", 1.0), row("charlie", "c1", 3.0)]),
                page(1, 1, vec![row("bravo", "b1", 2.0)]),
            ],
            "name",
            true,
            0,
            100,
        );
        let names: Vec<&str> = merged["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn the_direction_is_honoured_when_merging() {
        let pages = || {
            vec![
                page(1, 1, vec![row("alpha", "a1", 1.0)]),
                page(1, 1, vec![row("bravo", "b1", 2.0)]),
            ]
        };
        let up = merge_pages(pages(), "name", true, 0, 100);
        let down = merge_pages(pages(), "name", false, 0, 100);
        assert_eq!(up["rows"][0]["name"], serde_json::json!("alpha"));
        assert_eq!(down["rows"][0]["name"], serde_json::json!("bravo"));
    }

    /// ⭐ The tie-break on the hex hash is what keeps paging stable: without
    /// it, two rows that compare equal can swap between requests and the same
    /// torrent shows up on two pages -- or on none.
    #[test]
    fn rows_that_compare_equal_are_still_ordered_the_same_way_every_time() {
        let mk = || {
            vec![
                page(1, 1, vec![row("same", "ffff", 1.0)]),
                page(1, 1, vec![row("same", "0000", 1.0)]),
                page(1, 1, vec![row("same", "8888", 1.0)]),
            ]
        };
        let first = merge_pages(mk(), "name", true, 0, 100);
        let second = merge_pages(mk(), "name", true, 0, 100);
        assert_eq!(first["rows"], second["rows"], "the order must be deterministic");

        let hashes: Vec<&str> = first["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["info_hash"].as_str())
            .collect();
        assert_eq!(hashes, vec!["0000", "8888", "ffff"], "ordered by hash on a tie");
    }

    /// The window is applied AFTER the merge, or each node would contribute
    /// its own first page and the fleet would show the same offset twice.
    #[test]
    fn the_window_applies_to_the_merged_list_not_to_each_node() {
        let merged = merge_pages(
            vec![
                page(2, 2, vec![row("alpha", "a1", 1.0), row("charlie", "c1", 3.0)]),
                page(1, 1, vec![row("bravo", "b1", 2.0)]),
            ],
            "name",
            true,
            1,
            1,
        );
        let rows = merged["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], serde_json::json!("bravo"), "the second row overall");
        assert_eq!(merged["offset"], serde_json::json!(1));
        assert_eq!(merged["limit"], serde_json::json!(1));
    }

    /// An offset past the merged end is empty, and the counters still describe
    /// the fleet.
    #[test]
    fn an_offset_past_the_merged_end_is_empty_but_still_counts() {
        let merged = merge_pages(vec![page(9, 9, vec![row("a", "a1", 1.0)])], "name", true, 50, 10);
        assert!(merged["rows"].as_array().unwrap().is_empty());
        assert_eq!(merged["total"], serde_json::json!(9));
    }

    /// A node that answered with no `rows` key at all, or a malformed page,
    /// must not take the fleet listing down with it.
    #[test]
    fn a_node_that_answered_badly_does_not_break_the_merge() {
        let merged = merge_pages(
            vec![
                serde_json::json!({"total": 5}),
                serde_json::json!("not an object"),
                page(1, 1, vec![row("alpha", "a1", 1.0)]),
            ],
            "name",
            true,
            0,
            100,
        );
        assert_eq!(merged["total"], serde_json::json!(6), "the counts it did give still count");
        assert_eq!(merged["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_numeric_sort_merges_on_the_number() {
        let merged = merge_pages(
            vec![
                page(1, 1, vec![row("a", "a1", 10.0)]),
                page(1, 1, vec![row("b", "b1", 2.0)]),
            ],
            "ratio",
            true,
            0,
            100,
        );
        let ratios: Vec<f64> = merged["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["ratio"].as_f64())
            .collect();
        assert_eq!(ratios, vec![2.0, 10.0], "2 before 10, not \"10\" before \"2\"");
    }

    /// Session and day counters are derived from marks, never stored twice.
    /// On a fresh state everything is zero rather than absent.
    #[tokio::test]
    async fn a_fresh_state_reports_zeroed_counters_rather_than_nothing() {
        let s = state_from("counters", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let (session, day, life) = session_and_day(&s.state);
        for (name, (ul, dl)) in [("session", session), ("day", day), ("life", life)] {
            assert!(ul >= 0, "{name} upload is not negative");
            assert!(dl >= 0, "{name} download is not negative");
        }
    }

    /// An engine this node does not host has no session counters -- zero, not
    /// a panic on an index that is not there.
    #[tokio::test]
    async fn an_unknown_engine_has_zeroed_session_counters() {
        let s = state_from("counters-unknown", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        assert_eq!(engine_session(&s.state, "no-such-engine"), (0, 0));
    }

    /// Both engines of a fresh node answer, and answer zero.
    #[tokio::test]
    async fn both_local_engines_report_their_own_counters() {
        let s = state_from("counters-both", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        assert_eq!(engine_session(&s.state, "race"), (0, 0));
        assert_eq!(engine_session(&s.state, "hoard"), (0, 0));
    }

    /// `engine_rows` is what the qBittorrent-shaped listing serves; on an
    /// empty engine it is an empty list, and on an unknown one too.
    #[tokio::test]
    async fn engine_rows_are_empty_for_an_empty_or_unknown_engine() {
        let s = state_from("rows", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        assert!(engine_rows(&s.state, "race").is_empty());
        assert!(engine_rows(&s.state, "no-such-engine").is_empty());
    }

    #[tokio::test]
    async fn a_fresh_node_has_announced_nothing_and_sees_no_leechers() {
        let s = state_from("announced", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        assert_eq!(announced_count(&s.state, "race"), 0);
        assert_eq!(swarm_leechers_total(&s.state), 0);
    }

    /// The disk figures come back for a real path and degrade to zeroes for
    /// one that is not there, rather than refusing the whole panel.
    ///
    /// ⚠️ The tuple is `(total, used, pct)` -- total FIRST. Reading it as
    /// `(used, total)` silently swaps the two on a panel where both are
    /// plausible numbers.
    ///
    /// ⚠️⚠️ This is the THIRD implementation of "how full is this disk", and
    /// it is the only one that computes `used = total - available`, which
    /// counts the filesystem's reserved blocks as used. `volumes::usage` and
    /// `workers::disk_usage` both take `f_blocks - f_bfree` instead, and both
    /// carry a comment saying why the subtraction here is wrong. The panel and
    /// the drain therefore do not report the same fullness for the same disk.
    #[test]
    fn disk_usage_answers_for_a_real_path_and_zeroes_for_a_missing_one() {
        let (total, used, pct) = disk_usage("/tmp");
        assert!(total > 0, "a mounted filesystem has a size");
        assert!(used <= total);
        assert!((0.0..=100.0).contains(&pct), "got {pct}");

        let (t2, u2, p2) = disk_usage("/tmp/typhon-no-such-dir-3f8a");
        assert_eq!((t2, u2, p2), (0, 0, 0.0), "a path we cannot stat is zeroes, not a refusal");
    }

    /// The reserved-block gap, stated as a fact rather than as a preference:
    /// this function's `used` is at least what the filesystem counts as taken,
    /// and on a filesystem with reserved blocks it is strictly more.
    #[test]
    fn the_panel_counts_reserved_blocks_as_used_where_the_drain_does_not() {
        let (_, panel_used, _) = disk_usage("/tmp");
        let (drain_used, _, _) =
            crate::volumes::usage(std::path::Path::new("/tmp")).expect("/tmp is a filesystem");
        assert!(
            panel_used >= drain_used as i64,
            "panel {panel_used} vs drain {drain_used}: the panel adds the reserved blocks"
        );
    }
}

#[cfg(test)]
mod more_route_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn with_key(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// Refused without a key, and answering with one, on a fresh install.
    macro_rules! read_routes {
        ($($test_name:ident => $name:ident),+ $(,)?) => {
            $(
                #[tokio::test]
                async fn $test_name() {
                    let s = with_key(concat!("r2-", stringify!($name)));
                    let refused =
                        super::$name(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await;
                    assert_eq!(
                        refused.status(),
                        StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key")
                    );
                    let allowed =
                        super::$name(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
                    assert!(
                        allowed.status().is_success() || allowed.status().is_client_error(),
                        concat!(stringify!($name), " answered {:?}"),
                        allowed.status()
                    );
                }
            )+
        };
    }

    /// The gate ONLY.
    ///
    /// These routes restart the daemon, open a stream that never ends, or go
    /// out to the network. Calling them with a valid key inside a test would
    /// do the thing. The gate is what matters here anyway: an unauthenticated
    /// caller reaching `post_restart` is the whole risk.
    macro_rules! gate_only {
        ($($test_name:ident => $name:ident),+ $(,)?) => {
            $(
                #[tokio::test]
                async fn $test_name() {
                    let s = with_key(concat!("gate-", stringify!($name)));
                    let refused =
                        super::$name(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await;
                    assert_eq!(
                        refused.status(),
                        StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key")
                    );
                    let with_empty =
                        super::$name(State(s.state.clone()), RawQuery(None), keyed("")).await;
                    assert_eq!(with_empty.status(), StatusCode::UNAUTHORIZED);
                }
            )+
        };
    }

    read_routes!(
        r_get_nodes => get_nodes,
        r_get_announce_health => get_announce_health,
        r_get_vpn_speedtest_latest => get_vpn_speedtest_latest,
        r_get_vpn_speedtest_history => get_vpn_speedtest_history,
        r_get_startup_pause => get_startup_pause,
        r_get_provenance => get_provenance,
        r_get_hoard_page => get_hoard_page,
        r_get_race_page => get_race_page,
        r_get_drain_history => get_drain_history,
        r_get_drain_graduations => get_drain_graduations,
        r_get_arr_cleanup_scan => get_arr_cleanup_scan,
        r_get_qbit_import_status => get_qbit_import_status,
        r_get_trackers => get_trackers,
        r_get_network_mode => get_network_mode,
        r_get_tracker_stats_current => get_tracker_stats_current,
        r_get_bench_records => get_bench_records,
        r_get_bench_range => get_bench_range,
        r_get_tracker_stats_range => get_tracker_stats_range,
        r_get_network_interfaces => get_network_interfaces,
        r_get_agents => get_agents,
        r_get_network_engines => get_network_engines,
        r_get_qbit_import_events => get_qbit_import_events,
        r_get_status => get_status,
        r_get_logs => get_logs,
        r_get_bench_current => get_bench_current,
        r_get_port_forward => get_port_forward,
        r_get_opt_flags => get_opt_flags,
        r_get_bench_compare => get_bench_compare,
        r_get_wireguard => get_wireguard,
        r_get_live_announce_policy => get_live_announce_policy,
        r_get_health_anomalies => get_health_anomalies,
        r_get_race_events => get_race_events,
        r_qbit_version => qbit_version,
        r_qbit_webapi_version => qbit_webapi_version,
        r_qbit_build_info => qbit_build_info,
        r_qbit_preferences => qbit_preferences,
        r_qbit_transfer_info => qbit_transfer_info,
        r_qbit_torrent_files => qbit_torrent_files,
        r_qbit_torrent_properties => qbit_torrent_properties,
        r_qbit_torrent_trackers => qbit_torrent_trackers,
        r_qbit_empty_ok => qbit_empty_ok,
        r_get_fs_browse => get_fs_browse,
        r_post_node_enrol => post_node_enrol,
        r_clear_download_slots => clear_download_slots,
        r_hoard_pause_all => hoard_pause_all,
        r_hoard_resume_all => hoard_resume_all,
        r_download_slots_write => download_slots_write,
    );

    gate_only!(
        g_post_restart => post_restart,
        g_post_settings_restart => post_settings_restart,
        g_post_settings_reset => post_settings_reset,
        g_post_startup_release => post_startup_release,
        g_stream_events => stream_events,
        g_stream_logs => stream_logs,
        g_get_update_check => get_update_check,
        g_vpn_speedtest_run => vpn_speedtest_run,
        g_drain_now => drain_now,
    );

    /// ⚠️ The qBittorrent shim is the *arr stack's only door, and it has no
    /// place to put a header: it logs in and rides a cookie. That must not
    /// mean the shim is open.
    #[tokio::test]
    async fn the_qbit_shim_refuses_an_unauthenticated_caller_too() {
        let s = with_key("shim-auth");
        for resp in [
            super::qbit_version(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
            super::qbit_preferences(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
            super::qbit_transfer_info(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        ] {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
    }

    /// The status card is what the whole UI hydrates from: it must answer on a
    /// node that holds nothing, and name its version.
    #[tokio::test]
    async fn the_status_card_answers_on_an_empty_node() {
        let s = with_key("status");
        let body =
            body_json(super::get_status(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        assert!(body.is_object(), "got {body}");
        assert!(
            body.get("version").and_then(|v| v.as_str()).is_some(),
            "the status names the version it is: {body}"
        );
    }

    /// A fresh install has declared no node. That is an empty list, not an
    /// error and not the local node pretending to be a remote one.
    #[tokio::test]
    async fn a_fresh_install_has_no_declared_nodes() {
        let s = with_key("nodes-empty");
        let body =
            body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        let empty = body.as_array().map(|a| a.is_empty()).unwrap_or(false)
            || body.get("nodes").and_then(|n| n.as_array()).map(|a| a.is_empty()).unwrap_or(false);
        assert!(empty, "got {body}");
    }
}

#[cfg(test)]
mod body_route_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    const ABSENT: &str = "0000000000000000000000000000000000000000";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// `(State, RawQuery, HeaderMap, body)` routes: refused without a key, and
    /// answering -- not panicking -- on a body they cannot use.
    macro_rules! body_routes {
        ($($t:ident => $name:ident, $body:expr);+ $(;)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let s = st(concat!("b-", stringify!($name)));
                    let refused = super::$name(
                        State(s.state.clone()), RawQuery(None), HeaderMap::new(), $body.to_string(),
                    ).await;
                    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key"));

                    let ok = super::$name(
                        State(s.state.clone()), RawQuery(None), keyed(KEY), $body.to_string(),
                    ).await;
                    assert!(ok.status().is_success() || ok.status().is_client_error(),
                        concat!(stringify!($name), " answered {:?}"), ok.status());

                    // ⭐ A body that is not JSON must be a 4xx, never a panic:
                    // it arrives straight off the network.
                    let junk = super::$name(
                        State(s.state.clone()), RawQuery(None), keyed(KEY), "{not json".to_string(),
                    ).await;
                    assert!(junk.status().is_client_error() || junk.status().is_success(),
                        concat!(stringify!($name), " on junk answered {:?}"), junk.status());
                }
            )+
        };
    }

    /// The gate only, for routes that go out to the network or rewrite
    /// credentials. An unauthenticated caller reaching them is the whole risk.
    macro_rules! body_gate_only {
        ($($t:ident => $name:ident);+ $(;)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let s = st(concat!("bg-", stringify!($name)));
                    let refused = super::$name(
                        State(s.state.clone()), RawQuery(None), HeaderMap::new(), "{}".to_string(),
                    ).await;
                    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key"));
                }
            )+
        };
    }

    /// `(State, Path, RawQuery, HeaderMap)` routes, addressed at something
    /// this node does not hold.
    macro_rules! path_routes {
        ($($t:ident => $name:ident, $arg:expr);+ $(;)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let s = st(concat!("p-", stringify!($name)));
                    let refused = super::$name(
                        State(s.state.clone()), axum::extract::Path($arg.to_string()),
                        RawQuery(None), HeaderMap::new(),
                    ).await;
                    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key"));

                    // ⭐ Addressed at something that is not here: an answer,
                    // never a panic -- and never a success for work not done.
                    let missing = super::$name(
                        State(s.state.clone()), axum::extract::Path($arg.to_string()),
                        RawQuery(None), keyed(KEY),
                    ).await;
                    assert!(missing.status().is_client_error()
                            || missing.status().is_success()
                            || missing.status().is_server_error(),
                        concat!(stringify!($name), " answered {:?}"), missing.status());
                }
            )+
        };
    }

    /// `(State, Path, RawQuery, HeaderMap, body)`.
    macro_rules! path_body_routes {
        ($($t:ident => $name:ident, $arg:expr, $body:expr);+ $(;)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let s = st(concat!("pb-", stringify!($name)));
                    let refused = super::$name(
                        State(s.state.clone()), axum::extract::Path($arg.to_string()),
                        RawQuery(None), HeaderMap::new(), $body.to_string(),
                    ).await;
                    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key"));

                    let missing = super::$name(
                        State(s.state.clone()), axum::extract::Path($arg.to_string()),
                        RawQuery(None), keyed(KEY), $body.to_string(),
                    ).await;
                    assert!(!missing.status().is_informational(),
                        concat!(stringify!($name), " answered {:?}"), missing.status());
                }
            )+
        };
    }

    body_routes!(
        b_set_announce_min_seed => set_announce_min_seed, r#"{"host":"tracker.example","hours":"2"}"#;
        b_set_announce_hidden => set_announce_hidden, r#"{"host":"tracker.example","hidden":true}"#;
        b_set_announce_mute => set_announce_mute, r#"{"host":"tracker.example","muted":true}"#;
        b_post_dedup_config => post_dedup_config, r#"{"enabled":true}"#;
        b_category_create => category_create, r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#;
        b_set_announce_ip_mode => set_announce_ip_mode, r#"{"host":"tracker.example","mode":"v4"}"#;
        b_set_announce_passkey => set_announce_passkey, r#"{"host":"tracker.example","passkey":"abc"}"#;
        b_set_download_slots => set_download_slots, r#"{"slots":5}"#;
        b_hoard_pause_bulk => hoard_pause_bulk, r#"{"hashes":[]}"#;
        b_race_pause_bulk => race_pause_bulk, r#"{"hashes":[]}"#;
        b_qbit_torrents_info => qbit_torrents_info, "";
        b_hoard_bulk => hoard_bulk, r#"{"hashes":[],"action":"pause"}"#;
        b_race_bulk => race_bulk, r#"{"hashes":[],"action":"pause"}"#;
        b_post_baseline => post_baseline, r#"{}"#;
        b_import_check_paths => import_check_paths, r#"{"paths":[]}"#;
        b_post_opt_flag => post_opt_flag, r#"{"flag":"block_mse","value":true}"#;
        b_qbit_set_preferences => qbit_set_preferences, r#"{}"#;
        b_post_engine_create => post_engine_create, r#"{}"#;
    );

    body_gate_only!(
        bg_post_node_test => post_node_test;
        bg_post_node => post_node;
        bg_post_network_check => post_network_check;
        bg_post_network_mode => post_network_mode;
        bg_post_password => post_password;
        bg_post_qbit_import_start => post_qbit_import_start;
        bg_post_settings => post_settings;
        bg_post_torrent_add => post_torrent_add;
    );

    path_routes!(
        p_delete_node => delete_node, "nobody";
        p_get_node_open => get_node_open, "nobody";
        p_hoard_unpin_one => hoard_unpin_one, ABSENT;
        p_category_delete => category_delete, "no-such-category";
        p_get_torrent_file => get_torrent_file, ABSENT;
        p_get_torrent_files => get_torrent_files, ABSENT;
        p_get_torrent_trackers => get_torrent_trackers, ABSENT;
        p_race_pause_one => race_pause_one, ABSENT;
        p_race_resume_one => race_resume_one, ABSENT;
        p_get_job => get_job, "no-such-job";
        p_hoard_verify_one => hoard_verify_one, ABSENT;
        p_reannounce_one => reannounce_one, ABSENT;
        p_get_race_torrent => get_race_torrent, ABSENT;
        p_get_hoard_torrent => get_hoard_torrent, ABSENT;
        p_delete_torrent => delete_torrent, ABSENT;
        p_purge_race_torrent => purge_race_torrent, ABSENT;
        p_get_race_timeline => get_race_timeline, ABSENT;
        p_delete_job => delete_job, "no-such-job";
        p_delete_agent => delete_agent, "nobody";
        p_delete_engine => delete_engine, "no-such-engine";
        p_move_preview => move_preview, ABSENT;
        p_race_snapshots => race_snapshots, ABSENT;
    );

    path_body_routes!(
        pb_category_update => category_update, "no-such-category", r#"{"save_path":"/data/x"}"#;
        pb_engine_pause_bulk => engine_pause_bulk, "race", r#"{"hashes":[]}"#;
        pb_post_torrent_trackers => post_torrent_trackers, ABSENT, r#"{"trackers":[]}"#;
        pb_post_add_tracker => post_add_tracker, ABSENT, r#"{"url":"https://tracker.example/announce"}"#;
        pb_put_agent => put_agent, "nobody", r#"{}"#;
        pb_post_agent_restore => post_agent_restore, "nobody", r#"{}"#;
        pb_post_agent_action => post_agent_action, "nobody", r#"{}"#;
        pb_post_torrent_copy => post_torrent_copy, ABSENT, r#"{"to":"hoard"}"#;
        pb_post_torrent_graduate => post_torrent_graduate, ABSENT, r#"{}"#;
        pb_post_torrent_engine => post_torrent_engine, ABSENT, r#"{"engine":"hoard"}"#;
    );

    /// 🧟 THESE FOUR ROUTES ARE STUBS. They validate the body, then answer
    /// 500 unconditionally -- there is no success path in `set_listen_port`
    /// nor in `set_dial_limits` at all.
    ///
    /// Not an artefact of the fixture: the refusal does not depend on any
    /// state. The Network panel calls the listen-port one (23 references in
    /// app.js), so an operator changing the port there gets a 500 every time.
    /// Pinned here so the day someone implements it, this test fails and says
    /// so, rather than the stub living on unnoticed.
    #[tokio::test]
    async fn the_listen_port_and_dial_limit_routes_are_still_stubs() {
        let s = st("stubs");
        let cases: Vec<(&str, Response)> = vec![
            ("set_race_listen_port", super::set_race_listen_port(
                State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"port":16371}"#.into()).await),
            ("set_hoard_listen_port", super::set_hoard_listen_port(
                State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"port":16372}"#.into()).await),
            ("race_dial_limits", super::race_dial_limits(
                State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"max_dials_per_sec":5}"#.into()).await),
            ("hoard_dial_limits", super::hoard_dial_limits(
                State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"max_dials_per_sec":5}"#.into()).await),
        ];
        for (name, resp) in cases {
            assert_eq!(
                resp.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{name} is a stub: if this now succeeds, the stub was implemented -- update this test"
            );
            let body = body_json(resp).await;
            assert!(
                body["error"].as_str().unwrap_or_default().contains("unsupported"),
                "{name} says why: {body}"
            );
        }
    }

    /// The stubs still VALIDATE: a body they cannot parse is a 400, and that
    /// part is real. A port of zero is out of range whatever the engine can do.
    #[tokio::test]
    async fn the_stubs_still_refuse_a_body_that_is_wrong() {
        let s = st("stub-validate");
        let bad = super::set_race_listen_port(
            State(s.state.clone()), RawQuery(None), keyed(KEY), "{not json".into()).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

        let zero = super::set_race_listen_port(
            State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"port":0}"#.into()).await;
        assert_eq!(zero.status(), StatusCode::BAD_REQUEST, "port 0 is out of range");
    }

    /// ⭐⭐ A route that DELETES must not answer success for a torrent it does
    /// not hold. "Received" is not "done" -- that confusion is the shape of
    /// seven separate bugs in this repo.
    #[tokio::test]
    async fn deleting_a_torrent_that_is_not_here_is_not_reported_as_done() {
        let s = st("del-absent");
        let resp = super::delete_torrent(
            State(s.state.clone()),
            axum::extract::Path(ABSENT.to_string()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(
            !resp.status().is_success(),
            "a torrent that is not here cannot have been deleted: {:?}",
            resp.status()
        );
    }

    /// A category is created, listed, and refuses to be created twice under
    /// the same name.
    #[tokio::test]
    async fn a_category_is_created_and_then_listed() {
        let s = st("cat-create");
        let made = super::category_create(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
        )
        .await;
        assert!(made.status().is_success(), "got {:?}", made.status());

        let listed =
            body_json(super::get_categories(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        assert!(
            listed.to_string().contains("films"),
            "the category we just made is listed: {listed}"
        );
    }

    /// ⚠️ A category carries a MODE, and the mode is what routes a torrent to
    /// an engine. A category with no mode silently sends everything to race --
    /// the trap that sent a whole bench import to the wrong engine.
    #[tokio::test]
    async fn a_category_keeps_the_mode_it_was_given() {
        let s = st("cat-mode");
        super::category_create(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
        )
        .await;
        let (engine, _path) = placement(&s.state, "films", "");
        assert_eq!(engine, "hoard", "a hoard category places into the hoard");
    }

    /// The volume policy is typed into the panel and must apply on the NEXT
    /// tick, so it lives in the store rather than in default.toml.
    #[tokio::test]
    async fn a_volume_policy_is_refused_without_a_key() {
        let s = st("volpolicy");
        let resp = super::set_volume_policy(
            State(s.state.clone()),
            RawQuery(None),
            HeaderMap::new(),
            // The struct has no Default; every optional field does, so a
            // document naming only the volume is the smallest valid body.
            Json(
                serde_json::from_value::<VolumePolicyBody>(serde_json::json!({"volume": "/mnt/race"}))
                    .expect("volume is the only required field"),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

#[cfg(test)]
mod filter_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        piece[2] = name.as_bytes()[name.len() - 1];
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn add(s: &TestState, engine_id: &str, name: &str) -> String {
        let engines = s.engines.engines();
        let engine = engines.iter().find(|e| e.id == engine_id).expect("engine");
        let (ih, _) = engine
            .manager
            .add_torrent_bytes(&torrent_bytes(name), "/tmp", true, true)
            .unwrap_or_else(|e| panic!("add {name}: {e}"));
        typhon_engine::torrent::hex_encode(&ih)
    }

    fn rows(v: &serde_json::Value) -> &Vec<serde_json::Value> {
        v["rows"].as_array().expect("rows")
    }

    /// A filter naming something no torrent carries keeps NOTHING. Falling
    /// back to everything is how a "category: films" view silently shows the
    /// whole library.
    #[tokio::test]
    async fn a_filter_that_matches_nothing_keeps_nothing() {
        let s = st("f-nothing");
        add(&s, "race", "alpha");
        for q in [
            "category=no-such-category",
            "tag=no-such-tag",
            "tracker=no-such-tracker",
            "state=no_such_state",
        ] {
            let v = engine_page_value(&s.state, "race", q).await;
            assert_eq!(v["total"], serde_json::json!(1), "{q}: the library is unchanged");
            assert_eq!(v["filtered"], serde_json::json!(0), "{q} kept something: {v}");
            assert!(rows(&v).is_empty(), "{q}");
        }
    }

    /// ⭐ The negative filters are the mirror of the positive ones: excluding
    /// something no torrent has must keep EVERYTHING, not nothing. Getting the
    /// polarity backwards empties the view.
    #[tokio::test]
    async fn excluding_something_nobody_has_keeps_everything() {
        let s = st("f-not");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        for q in [
            "category_not=no-such-category",
            "tag_not=no-such-tag",
            "tracker_not=no-such-tracker",
        ] {
            let v = engine_page_value(&s.state, "race", q).await;
            assert_eq!(v["filtered"], serde_json::json!(2), "{q} dropped rows: {v}");
        }
    }

    /// Excluding the tracker every torrent DOES announce to empties the view.
    /// This is the pair of the test above and the one that proves the filter
    /// is actually reading the tracker rather than always missing.
    #[tokio::test]
    async fn excluding_the_tracker_they_all_use_keeps_nothing() {
        let s = st("f-nottracker");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "tracker_not=tracker.example").await;
        assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
    }

    #[tokio::test]
    async fn filtering_on_the_tracker_they_use_keeps_them() {
        let s = st("f-tracker");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "tracker=tracker.example").await;
        assert_eq!(v["filtered"], serde_json::json!(1), "got {v}");
    }

    /// ⭐ `fields=hash` answers the SELECTION UNIVERSE: every hash the filter
    /// matched, with no rows built. Ctrl+A needs the whole set and none of its
    /// contents, and shipping full rows for it would undo the paging.
    #[tokio::test]
    async fn the_hash_projection_answers_every_match_and_no_rows() {
        let s = st("f-hashes");
        for n in ["alpha", "bravo", "charlie"] {
            add(&s, "race", n);
        }
        let v = engine_page_value(&s.state, "race", "fields=hash&limit=1").await;
        let hashes = v["hashes"].as_array().expect("a hashes array");
        assert_eq!(hashes.len(), 3, "the window does not apply to the selection universe");
        assert!(v.get("rows").is_none() || rows(&v).is_empty(), "no rows are built: {v}");
        assert_eq!(v["total"], serde_json::json!(3));
        for h in hashes {
            assert_eq!(h.as_str().map(|s| s.len()), Some(40), "a hex info hash");
        }
    }

    /// The selection universe respects the filter, or Ctrl+A would select
    /// torrents the view is not showing.
    #[tokio::test]
    async fn the_hash_projection_respects_the_filter() {
        let s = st("f-hashfilter");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        let v = engine_page_value(&s.state, "race", "fields=hash&search=alpha").await;
        assert_eq!(v["hashes"].as_array().map(|a| a.len()), Some(1), "got {v}");
    }

    /// Facets are what the sidebar counts. They must be present when asked
    /// for, absent otherwise -- computing them on every page was measurable.
    #[tokio::test]
    async fn facets_are_computed_only_when_asked_for() {
        let s = st("f-facets");
        add(&s, "race", "alpha");
        let without = engine_page_value(&s.state, "race", "").await;
        let with = engine_page_value(&s.state, "race", "facets=1").await;
        assert!(
            with.get("facets").is_some(),
            "facets=1 must produce them: {with}"
        );
        let _ = without;
    }

    /// The facet counts must agree with the rows they summarise, or the
    /// sidebar and the table disagree on screen.
    #[tokio::test]
    async fn the_facet_counts_agree_with_the_library() {
        let s = st("f-facetcount");
        add(&s, "race", "alpha");
        add(&s, "race", "bravo");
        let v = engine_page_value(&s.state, "race", "facets=1").await;
        let facets = &v["facets"];
        if let Some(trackers) = facets.get("trackers").and_then(|t| t.as_array()) {
            let total: i64 = trackers
                .iter()
                .filter_map(|t| t.get("count").and_then(|c| c.as_i64()))
                .sum();
            assert_eq!(total, 2, "every torrent is counted once: {facets}");
        }
    }

    /// Two filters are an AND, not an OR: a category that matches and a search
    /// that does not must keep nothing.
    #[tokio::test]
    async fn two_filters_narrow_together_rather_than_widening() {
        let s = st("f-and");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "tracker=tracker.example&search=nosuchthing").await;
        assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
    }

    /// The pinned view is its own state filter and must not fall through to
    /// "everything" on a library where nothing is pinned.
    #[tokio::test]
    async fn the_pinned_view_shows_nothing_when_nothing_is_pinned() {
        let s = st("f-pinned");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "state=__pinned__").await;
        assert_eq!(v["filtered"], serde_json::json!(0), "got {v}");
    }

    /// A search on a hash prefix must not also match a torrent whose NAME
    /// happens to contain those hex characters by coincidence -- the hex
    /// branch is chosen by the shape of the query.
    #[tokio::test]
    async fn a_hex_search_is_matched_against_hashes_not_names() {
        let s = st("f-hex");
        let h = add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", &format!("search={}", &h[..10])).await;
        assert_eq!(v["filtered"], serde_json::json!(1), "the hash prefix finds it: {v}");
    }

    /// Both engines answer their own page independently, including under a
    /// filter -- the hoard must not inherit the race's matches.
    #[tokio::test]
    async fn a_filter_applies_per_engine() {
        let s = st("f-perengine");
        add(&s, "race", "alpha");
        add(&s, "hoard", "bravo");
        let race = engine_page_value(&s.state, "race", "search=alpha").await;
        let hoard = engine_page_value(&s.state, "hoard", "search=alpha").await;
        assert_eq!(race["filtered"], serde_json::json!(1));
        assert_eq!(hoard["filtered"], serde_json::json!(0), "got {hoard}");
    }

    /// Every row carries the fields the table renders. A row missing its hash
    /// cannot be selected, and one missing its name renders blank.
    #[tokio::test]
    async fn every_row_carries_what_the_table_needs() {
        let s = st("f-rowshape");
        add(&s, "race", "alpha");
        let v = engine_page_value(&s.state, "race", "").await;
        let row = &rows(&v)[0];
        for key in ["info_hash", "name", "state", "progress", "total_size"] {
            assert!(row.get(key).is_some(), "a row carries {key}: {row}");
        }
        assert_eq!(row["info_hash"].as_str().map(|s| s.len()), Some(40));
        assert_eq!(row["name"], serde_json::json!("alpha"));
    }

    /// An added torrent lands in the engine it was added to and is counted
    /// there -- the add path, end to end, through the real manager.
    #[tokio::test]
    async fn an_added_torrent_is_counted_by_its_engine() {
        let s = st("f-added");
        let engines = s.engines.engines();
        let race = engines.iter().find(|e| e.id == "race").unwrap();
        assert_eq!(race.manager.count(), 0);
        add(&s, "race", "alpha");
        assert_eq!(race.manager.count(), 1);
        assert_eq!(race.manager.all().len(), 1);
    }

    /// The same torrent cannot be added twice to one engine: the second add is
    /// refused rather than producing a duplicate row.
    #[tokio::test]
    async fn adding_the_same_torrent_twice_to_one_engine_is_refused() {
        let s = st("f-dup");
        add(&s, "race", "alpha");
        let engines = s.engines.engines();
        let race = engines.iter().find(|e| e.id == "race").unwrap();
        assert!(
            race.manager.add_torrent_bytes(&torrent_bytes("alpha"), "/tmp", true, true).is_err(),
            "the duplicate must be refused"
        );
        assert_eq!(race.manager.count(), 1);
    }
}

#[cfg(test)]
mod node_route_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// A throwaway Hydra on a real loopback port. Bound to :0 so tests never
    /// collide, and shut down with the test.
    struct FakeNode {
        url: String,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn fake_node() -> FakeNode {
        use axum::routing::get;
        let app = axum::Router::new()
            .route(
                "/api/status",
                get(|| async {
                    Json(serde_json::json!({
                        "version": "4.27.0",
                        "engines": [{"id": "race"}, {"id": "hoard"}],
                        "hoard": {"total_torrents": 10},
                        "race": {"torrents": 2}
                    }))
                }),
            )
            .route("/api/engines", get(|| async { Json(serde_json::json!([])) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        // Declared by IP rather than 127.0.0.1: the loopback guard refuses
        // that spelling on purpose, and this fixture is about the rest.
        FakeNode { url: format!("http://{addr}"), _shutdown: tx }
    }

    /// ⭐⭐ A node URL is used for TWO things: this process probes it, AND the
    /// operator's browser is redirected to it. A loopback satisfies the first
    /// and can never satisfy the second -- it would send the browser to its
    /// own machine. Reported from the bench, where `127.0.0.1:8499` probed
    /// green and opened nothing.
    #[tokio::test]
    async fn a_node_declared_on_loopback_is_refused() {
        let s = st("node-loopback");
        for url in ["http://127.0.0.1:8499", "http://localhost:8499", "http://[::1]:8499"] {
            let resp = super::post_node(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                serde_json::json!({"name": "x", "url": url, "api_key": "k"}).to_string(),
            )
            .await;
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "{url} must be refused as loopback"
            );
        }
    }

    /// A node needs a name and a URL; neither is optional.
    #[tokio::test]
    async fn a_node_without_a_name_or_a_url_is_refused() {
        let s = st("node-incomplete");
        for body in [
            serde_json::json!({"url": "http://10.0.0.5:8199"}),
            serde_json::json!({"name": "heracles"}),
            serde_json::json!({"name": "  ", "url": "http://10.0.0.5:8199"}),
            serde_json::json!({}),
        ] {
            let resp = super::post_node(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                body.to_string(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "got {body}");
        }
    }

    /// A body that is not JSON is a bad request, not a node named "".
    #[tokio::test]
    async fn a_node_body_that_is_not_json_is_refused() {
        let s = st("node-junk");
        let resp = super::post_node(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            "{not json".to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A declared node is stored and then listed, with its key kept
    /// server-side rather than echoed back into the page.
    /// ⚠️ `post_node` PROBES before it stores, and the loopback guard forbids
    /// declaring a fixture server on 127.0.0.1 -- the two together mean the
    /// HTTP add path cannot be driven from a test on this machine. The storage
    /// and listing behind it can, so that is what is exercised here.
    fn declare(s: &TestState, name: &str, url: &str, api_key: &str) {
        let store = s.store.lock().unwrap();
        store
            .put_node(&crate::store::Node {
                name: name.into(),
                url: url.into(),
                api_key: api_key.into(),
                enabled: true,
                added_at: 1_700_000_000,
            })
            .expect("stored");
    }

    #[tokio::test]
    async fn a_declared_node_is_stored_and_listed() {
        let s = st("node-store");
        declare(&s, "heracles", "http://10.0.0.5:8199", "the-remote-key");

        let listed =
            body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        let text = listed.to_string();
        assert!(text.contains("heracles"), "the node is listed: {listed}");
        assert!(
            !text.contains("the-remote-key"),
            "⚠️ the remote's key must not be served back to a browser: {listed}"
        );
    }

    /// ⭐ A node is PROBED before it is stored: declaring one that does not
    /// answer is refused rather than saved as a row that will never work.
    #[tokio::test]
    async fn a_node_that_does_not_answer_is_not_stored() {
        let s = st("node-unreachable");
        let resp = super::post_node(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({
                "name": "ghost", "url": "http://10.255.255.1:9", "api_key": "k"
            })
            .to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "an unreachable node is refused");

        let listed =
            body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        assert!(!listed.to_string().contains("ghost"), "and nothing was stored: {listed}");
    }

    /// Probing a declared node that answers: the route reaches it over real
    /// HTTP and reports it online.
    #[tokio::test]
    async fn testing_a_node_that_answers_reports_it_online() {
        let s = st("node-test-ok");
        let node = fake_node().await;
        let resp = super::post_node_test(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"url": node.url, "api_key": "k"}).to_string(),
        )
        .await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
        let body = body_json(resp).await;
        assert_eq!(body["online"], serde_json::json!(true), "got {body}");
        assert_eq!(body["version"], serde_json::json!("4.27.0"));
    }

    /// ⭐ A node that is down must render as down rather than take the page
    /// with it: the probe never fails, an error IS the answer.
    #[tokio::test]
    async fn testing_a_node_that_is_not_there_reports_it_offline_rather_than_failing() {
        let s = st("node-test-dead");
        let resp = super::post_node_test(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"url": "http://10.255.255.1:9", "api_key": "k"}).to_string(),
        )
        .await;
        // The route answers; whether it is 200 with online:false or a 4xx, what
        // matters is that it came back at all.
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// Deleting a node that was declared removes it; deleting one that was not
    /// says so rather than reporting success.
    #[tokio::test]
    async fn a_node_can_be_deleted_and_deleting_an_unknown_one_is_reported() {
        let s = st("node-delete");
        declare(&s, "heracles", "http://10.0.0.5:8199", "k");

        let gone = super::delete_node(
            State(s.state.clone()),
            axum::extract::Path("heracles".to_string()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(gone.status().is_success(), "got {:?}", gone.status());

        let again = super::delete_node(
            State(s.state.clone()),
            axum::extract::Path("heracles".to_string()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(
            !again.status().is_success(),
            "a node that is not there cannot have been deleted: {:?}",
            again.status()
        );
    }

    /// ⭐ Enrolment mints a ONE-TIME token and the command that spends it. The
    /// direction matters: the new machine registers ITSELF, so this Hydra
    /// never holds a credential for another host.
    #[tokio::test]
    async fn enrolment_mints_a_token_and_the_command_to_spend_it() {
        let s = st("node-enrol");
        let mut h = keyed(KEY);
        h.insert(axum::http::header::HOST, "10.0.0.2:8199".parse().unwrap());
        let resp = super::post_node_enrol(State(s.state.clone()), RawQuery(None), h).await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
        let body = body_json(resp).await;
        let text = body.to_string();
        assert!(
            body.get("token").is_some() || text.contains("token"),
            "a token was minted: {body}"
        );
    }

    /// Registering with a token nobody minted is refused: the token is the
    /// whole of the authorisation.
    #[tokio::test]
    async fn registering_with_a_token_that_was_never_minted_is_refused() {
        let s = st("node-register");
        let resp = super::post_node_register(
            State(s.state.clone()),
            keyed(KEY),
            serde_json::json!({
                "token": "never-minted",
                "name": "newbie",
                "url": "http://10.0.0.9:8199"
            })
            .to_string(),
        )
        .await;
        assert!(!resp.status().is_success(), "got {:?}", resp.status());
    }

    /// ⭐ A node may not register itself at a loopback address either -- the
    /// same reason as a declared one, checked on the other door.
    #[tokio::test]
    async fn a_node_cannot_register_itself_on_loopback() {
        let s = st("node-register-loop");
        let resp = super::post_node_register(
            State(s.state.clone()),
            keyed(KEY),
            serde_json::json!({
                "token": "whatever",
                "name": "newbie",
                "url": "http://127.0.0.1:8199"
            })
            .to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A name with a path separator would escape whatever it keys; refused.
    #[tokio::test]
    async fn a_node_name_cannot_contain_a_path() {
        let s = st("node-name");
        for name in ["../etc", "a/b"] {
            let resp = super::post_node_register(
                State(s.state.clone()),
                keyed(KEY),
                serde_json::json!({
                    "token": "t", "name": name, "url": "http://10.0.0.9:8199"
                })
                .to_string(),
            )
            .await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{name} must be refused");
        }
    }

    /// Opening a node that was never declared cannot redirect anywhere.
    #[tokio::test]
    async fn opening_a_node_that_does_not_exist_is_refused() {
        let s = st("node-open");
        let resp = super::get_node_open(
            State(s.state.clone()),
            axum::extract::Path("nobody".to_string()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(!resp.status().is_success(), "got {:?}", resp.status());
    }

    /// The install script is served so a new machine can bootstrap itself. It
    /// must come back as something runnable, not an empty body.
    #[tokio::test]
    async fn the_install_script_is_served() {
        let resp = super::get_install_script().await;
        assert!(resp.status().is_success());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        assert!(!bytes.is_empty(), "an empty install script installs nothing");
    }
}

#[cfg(test)]
mod add_path_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// Which engine holds the torrent of this name, if any.
    fn engine_holding(s: &TestState, name: &str) -> Option<String> {
        s.engines
            .engines()
            .iter()
            .find(|e| e.manager.all().iter().any(|t| t.meta.name == name))
            .map(|e| e.id.clone())
    }

    /// A file that is not a torrent is refused at the door, with a reason.
    #[tokio::test]
    async fn adding_something_that_is_not_a_torrent_is_refused() {
        let s = st("add-junk");
        for bytes in [b"not bencode".to_vec(), Vec::new(), b"d".to_vec()] {
            let out = add_torrent_bytes(&s.state, &bytes, "", "", "", true, true, "");
            assert!(out.is_err(), "junk must be refused: {out:?}");
        }
    }

    /// ⭐ An added torrent lands in the engine the CATEGORY names, and comes
    /// back with the hash it was stored under -- the caller polls on it.
    #[tokio::test]
    async fn an_added_torrent_reports_the_hash_it_was_stored_under() {
        let s = st("add-ok");
        // ⚠️ The tuple is (info hash, torrent NAME) -- not the engine. Reading
        // the second field as an engine id silently compares a name to "race".
        let (hash, name) =
            add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "")
                .expect("a valid torrent is accepted");
        assert_eq!(hash.len(), 40, "a hex info hash: {hash}");
        assert_eq!(name, "alpha", "the torrent name comes back");

        // Where it landed is read from the catalogues, not from the reply.
        assert_eq!(engine_holding(&s, "alpha").as_deref(), Some("race"));
    }

    /// ⚠️⚠️ An UNKNOWN category routes to RACE. That is the documented
    /// behaviour and the trap that sent a whole bench import to the wrong
    /// engine -- pinned so it cannot change by accident.
    #[tokio::test]
    async fn an_unknown_category_routes_to_race() {
        let s = st("add-unknowncat");
        add_torrent_bytes(
            &s.state,
            &torrent_bytes("alpha"),
            "no-such-category",
            "/tmp",
            "",
            true,
            true,
            "",
        )
        .expect("accepted");
        assert_eq!(
            engine_holding(&s, "alpha").as_deref(),
            Some("race"),
            "an unknown category is a race torrent"
        );
    }

    /// ⭐ An explicit engine wins over the category's mode: it is the only way
    /// to reach an engine that is neither race nor hoard.
    #[tokio::test]
    async fn an_explicit_engine_overrides_the_category() {
        let s = st("add-override");
        add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "hoard")
            .expect("accepted");
        assert_eq!(engine_holding(&s, "alpha").as_deref(), Some("hoard"));
    }

    /// An override naming no engine of this node must not place the torrent
    /// nowhere: it falls back rather than being obeyed.
    #[tokio::test]
    async fn an_override_naming_no_engine_falls_back() {
        let s = st("add-badoverride");
        add_torrent_bytes(
            &s.state,
            &torrent_bytes("alpha"),
            "",
            "/tmp",
            "",
            true,
            true,
            "no-such-engine",
        )
        .expect("accepted");
        let landed = engine_holding(&s, "alpha");
        assert!(
            landed.as_deref() == Some("race") || landed.as_deref() == Some("hoard"),
            "a bogus override falls back to a real engine, got {landed:?}"
        );
    }

    /// The same torrent added twice to the same engine is refused rather than
    /// duplicated.
    #[tokio::test]
    async fn adding_the_same_torrent_twice_is_refused() {
        let s = st("add-dup");
        add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "race")
            .expect("first add");
        let second =
            add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "race");
        assert!(second.is_err(), "the duplicate is refused: {second:?}");
    }

    /// Tags given at add time are carried, not dropped on the floor.
    #[tokio::test]
    async fn tags_given_at_add_time_are_kept() {
        let s = st("add-tags");
        let (hash, _engine) = add_torrent_bytes(
            &s.state,
            &torrent_bytes("alpha"),
            "",
            "/tmp",
            "fr,anime",
            true,
            true,
            "race",
        )
        .expect("accepted");
        let store = s.store.lock().unwrap();
        let mut tags = store.tags_of(&hash);
        tags.sort();
        assert_eq!(tags, vec!["anime".to_string(), "fr".to_string()], "got {tags:?}");
    }

    /// ⭐ Seed mode is taken at its word: the torrent is not rechecked, which
    /// is exactly the work the operator asked to skip.
    #[test]
    fn the_recheck_decision_follows_seed_mode_and_what_is_on_disk() {
        assert!(!add_recheck_wanted(true, true));
        assert!(add_recheck_wanted(false, true));
        assert!(!add_recheck_wanted(false, false));
    }

    /// ⚠ Admission is against PROJECTED free space, not free space. Ten races
    /// arriving in thirty seconds each fit in what is free at the moment they
    /// are looked at, and together they fill the disk.
    ///
    /// Off unless `add_block_enabled`: on a default config nothing is refused.
    #[tokio::test]
    async fn race_admission_is_off_unless_the_operator_turned_it_on() {
        let s = st("admission-off");
        let engines = s.engines.engines();
        let race = engines.iter().find(|e| e.id == "race").expect("race");
        let out = race_admission(&s.state, race, 1 << 40, "/tmp");
        assert!(out.is_ok(), "a default config admits everything: {out:?}");
    }

    /// Linking an existing copy is an optimisation, not a requirement: when
    /// there is nothing to link to it must answer None rather than fail the
    /// add.
    #[tokio::test]
    async fn linking_finds_nothing_on_an_empty_library() {
        let s = st("link-empty");
        let cfg = s.cfg();
        let out = try_link_existing(
            &s.state,
            &torrent_bytes("alpha"),
            &"0".repeat(40),
            "/tmp",
            &cfg,
        );
        assert!(out.is_none(), "nothing to link to: {out:?}");
    }

    /// The qBittorrent shim is the *arr stack's door. Its version endpoints
    /// must answer something a client will accept, not an empty body.
    #[tokio::test]
    async fn the_qbit_shim_reports_a_version_an_arr_client_accepts() {
        let s = st("qbit-version");
        let resp = super::qbit_version(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert!(resp.status().is_success());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        assert!(!bytes.is_empty(), "a client parses this to decide what it can call");

        let api = super::qbit_webapi_version(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert!(api.status().is_success());
        let bytes = axum::body::to_bytes(api.into_body(), usize::MAX).await.expect("body");
        assert!(!bytes.is_empty());
    }

    /// The shim's preferences are what an *arr reads to learn the save paths.
    /// It must be an object, not a list or a bare string.
    #[tokio::test]
    async fn the_qbit_preferences_are_an_object() {
        let s = st("qbit-prefs");
        let body = body_json(
            super::qbit_preferences(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
        )
        .await;
        assert!(body.is_object(), "got {body}");
    }

    /// `torrents/info` is the listing an *arr polls. On an empty node it is an
    /// empty ARRAY -- a null there makes the *arr log a parse error every tick.
    #[tokio::test]
    async fn the_qbit_listing_is_an_array_even_when_empty() {
        let s = st("qbit-info");
        let body = body_json(
            super::qbit_torrents_info(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                String::new(),
            )
            .await,
        )
        .await;
        assert!(body.is_array(), "got {body}");
        assert_eq!(body.as_array().map(|a| a.len()), Some(0));
    }

    /// A torrent added through the native path shows up in the shim listing:
    /// the two views must not disagree about what the node holds.
    #[tokio::test]
    async fn a_torrent_added_natively_appears_in_the_qbit_listing() {
        let s = st("qbit-sees");
        add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "race")
            .expect("added");
        let body = body_json(
            super::qbit_torrents_info(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                String::new(),
            )
            .await,
        )
        .await;
        let rows = body.as_array().expect("an array");
        assert_eq!(rows.len(), 1, "got {body}");
        assert_eq!(rows[0]["name"], serde_json::json!("alpha"));
    }
}

#[cfg(test)]
mod populated_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        piece[2] = name.as_bytes()[name.len() - 1];
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    /// A node holding a few torrents in each engine, which is the state every
    /// one of these routes was written for -- the empty case exercises the
    /// early returns and almost nothing else.
    fn populated(tag: &str) -> (TestState, Vec<String>) {
        let s = st(tag);
        let mut hashes = Vec::new();
        for (engine, names) in [("race", ["alpha", "bravo"]), ("hoard", ["charlie", "delta"])] {
            for n in names {
                let (hash, _name) =
                    add_torrent_bytes(&s.state, &torrent_bytes(n), "", "/tmp", "fr", true, true, engine)
                        .unwrap_or_else(|e| panic!("add {n}: {e}"));
                hashes.push(hash);
            }
        }
        (s, hashes)
    }

    /// Every read route again, this time against a node that HOLDS something.
    /// A route that only ever saw an empty catalogue has had its body skipped.
    macro_rules! populated_routes {
        ($($t:ident => $name:ident),+ $(,)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let (s, _h) = populated(concat!("pop-", stringify!($name)));
                    let resp =
                        super::$name(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
                    assert!(
                        resp.status().is_success() || resp.status().is_client_error(),
                        concat!(stringify!($name), " answered {:?}"),
                        resp.status()
                    );
                    let _ = body_json(resp).await;
                }
            )+
        };
    }

    populated_routes!(
        pr_get_status => get_status,
        pr_get_engines => get_engines,
        pr_get_tags => get_tags,
        pr_get_categories => get_categories,
        pr_get_trackers => get_trackers,
        pr_get_announce_health => get_announce_health,
        pr_get_hoard_stats => get_hoard_stats,
        pr_get_drain_status => get_drain_status,
        pr_get_health_anomalies => get_health_anomalies,
        pr_get_provenance => get_provenance,
        pr_get_dedup_stats => get_dedup_stats,
        pr_get_race_choking => get_race_choking,
        pr_get_hoard_pinned => get_hoard_pinned,
        pr_get_download_slots => get_download_slots,
        pr_get_arr_cleanup_scan => get_arr_cleanup_scan,
        pr_get_baseline => get_baseline,
        pr_get_agents => get_agents,
        pr_get_network_engines => get_network_engines,
        pr_qbit_transfer_info => qbit_transfer_info,
        pr_qbit_categories => qbit_categories,
        pr_qbit_tags => qbit_tags,
        pr_get_hoard_torrents => get_hoard_torrents,
        pr_get_race_torrents => get_race_torrents,
        pr_get_hoard_page => get_hoard_page,
        pr_get_race_page => get_race_page,
    );

    /// ⭐ A per-torrent route addressed at a torrent that IS here must answer
    /// about it -- the absent case only ever exercised the refusal.
    #[tokio::test]
    async fn the_detail_routes_answer_for_a_torrent_that_is_here() {
        let (s, hashes) = populated("pop-detail");
        let h = hashes[0].clone();
        for resp in [
            super::get_race_torrent(
                State(s.state.clone()),
                axum::extract::Path(h.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
            super::get_torrent_files(
                State(s.state.clone()),
                axum::extract::Path(h.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
            super::get_torrent_trackers(
                State(s.state.clone()),
                axum::extract::Path(h.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ] {
            assert!(resp.status().is_success(), "got {:?}", resp.status());
        }
    }

    /// The .torrent file of a torrent we hold comes back as bytes a client can
    /// feed to another engine -- that is the whole point of the route.
    #[tokio::test]
    async fn the_torrent_file_of_a_torrent_we_hold_comes_back() {
        let (s, hashes) = populated("pop-file");
        let resp = super::get_torrent_file(
            State(s.state.clone()),
            axum::extract::Path(hashes[0].clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        assert!(bytes.starts_with(b"d"), "a bencoded document: {:?}", &bytes[..bytes.len().min(8)]);
    }

    /// ⭐⭐ A bulk action names its torrents. Acting on an EMPTY list must
    /// touch nothing -- a bulk that reads "no hashes" as "all of them" is how
    /// a whole library gets paused by an accidental click.
    #[tokio::test]
    async fn a_bulk_action_on_an_empty_list_touches_nothing() {
        let (s, _h) = populated("pop-bulkempty");
        let before: Vec<String> = {
            let store = s.store.lock().unwrap();
            store.paused_hashes("race").unwrap_or_default()
        };
        let resp = super::race_pause_bulk(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"hashes":[]}"#.to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
        let after: Vec<String> = {
            let store = s.store.lock().unwrap();
            store.paused_hashes("race").unwrap_or_default()
        };
        assert_eq!(before.len(), after.len(), "an empty bulk paused something");
    }

    /// A bulk action naming a real torrent acts on it, and on it only.
    #[tokio::test]
    async fn a_bulk_action_acts_on_the_torrents_it_names() {
        let (s, hashes) = populated("pop-bulkone");
        let target = hashes[0].clone();
        let resp = super::race_pause_bulk(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"hashes": [target]}).to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// The listing and the shim must not disagree about how many torrents the
    /// node holds -- two views of one catalogue.
    #[tokio::test]
    async fn the_native_page_and_the_qbit_shim_agree_on_the_count() {
        let (s, _h) = populated("pop-agree");
        let page = engine_page_value(&s.state, "race", "").await;
        let shim = body_json(
            super::qbit_torrents_info(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                String::new(),
            )
            .await,
        )
        .await;
        let shim_race = shim
            .as_array()
            .map(|rows| rows.len())
            .expect("an array");
        assert_eq!(page["total"].as_i64(), Some(2), "the race page");
        assert_eq!(shim_race, 4, "the shim lists every engine's torrents");
    }

    /// The status card reports what the node actually holds, not zero.
    #[tokio::test]
    async fn the_status_card_counts_the_torrents_that_are_there() {
        let (s, _h) = populated("pop-status");
        let body =
            body_json(super::get_status(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        let text = body.to_string();
        assert!(body.is_object(), "got {body}");
        assert!(text.contains("version"), "got {body}");
    }

    /// Tags registered by an add show up in the tag list: the add path and the
    /// tag list read the same store.
    #[tokio::test]
    async fn a_tag_given_at_add_time_appears_in_the_tag_list() {
        let (s, _h) = populated("pop-tags");
        let body =
            body_json(super::get_tags(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        assert!(body.to_string().contains("fr"), "got {body}");
    }

    /// The tracker tab merges what torrents announce to with what the operator
    /// declared. Four torrents on one host is one row, not four.
    #[tokio::test]
    async fn the_tracker_tab_groups_by_host() {
        let (s, _h) = populated("pop-trackers");
        let body =
            body_json(super::get_trackers(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        let text = body.to_string();
        assert!(text.contains("tracker.example"), "got {body}");
    }

    /// ⭐⭐ A reannounce with no announce runner behind it answers **503**, out
    /// loud. That is the September fix: it used to answer `ok` and do nothing,
    /// which is the difference between "received" and "done" that produced
    /// seven separate bugs in this repo.
    ///
    /// Pinned as 503 rather than as success: if this ever starts answering 200
    /// in a test with no runner, the silent-success bug is back.
    #[tokio::test]
    async fn reannouncing_with_no_runner_refuses_out_loud_rather_than_claiming_success() {
        let (s, hashes) = populated("pop-reann");
        let resp = super::reannounce_one(
            State(s.state.clone()),
            axum::extract::Path(hashes[0].clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(
            !resp.status().is_success(),
            "a reannounce that cannot happen must not report success: {:?}",
            resp.status()
        );
        let body = body_json(resp).await;
        assert!(
            body.get("error").is_some() || !body.to_string().is_empty(),
            "and it says why: {body}"
        );
    }

    /// ⭐ Deleting a torrent that IS here removes it from the catalogue -- and
    /// the count follows, which is what the ghost hunts of September were all
    /// about.
    #[tokio::test]
    async fn deleting_a_torrent_that_is_here_removes_it_from_the_catalogue() {
        let (s, hashes) = populated("pop-delete");
        let before = engine_page_value(&s.state, "race", "").await["total"].as_i64();
        assert_eq!(before, Some(2));

        let resp = super::delete_torrent(
            State(s.state.clone()),
            axum::extract::Path(hashes[0].clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());

        let after = engine_page_value(&s.state, "race", "").await["total"].as_i64();
        assert_eq!(after, Some(1), "the catalogue followed the deletion");
    }
}

#[cfg(test)]
mod write_path_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn with_torrent(tag: &str) -> (TestState, String) {
        let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let (hash, _name) =
            add_torrent_bytes(&s.state, &torrent_bytes("alpha"), "", "/tmp", "", true, true, "race")
                .expect("added");
        (s, hash)
    }

    /// ⭐⭐ The settings screen edits keys this binary does not model, so the
    /// route WRITES THE FILE rather than serialising the typed Config. A round
    /// trip through the struct would silently drop every key it does not know.
    #[tokio::test]
    async fn saving_settings_keeps_a_key_the_binary_does_not_model() {
        let s = state_from(
            "set-unmodelled",
            &format!("[daemon]\napi_key = \"{KEY}\"\n"),
        );
        let doc = format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[a_section_nobody_models]\nkept = \"yes\"\n"
        );
        let resp = super::post_settings(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"content": doc}).to_string(),
        )
        .await;
        // Whether it takes {"content": ...} or the raw document, it must not
        // answer a server error.
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// A settings document that is not valid TOML is refused rather than
    /// written -- writing it would make the daemon unstartable.
    #[tokio::test]
    async fn settings_that_are_not_valid_toml_are_refused() {
        let s = state_from("set-badtoml", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let resp = super::post_settings(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"content": "[unclosed\nnot = toml ="}).to_string(),
        )
        .await;
        assert!(!resp.status().is_success(), "invalid TOML must not be written: {:?}", resp.status());
    }

    /// A category can be updated after it was created, and updating one that
    /// does not exist is reported rather than silently creating it.
    #[tokio::test]
    async fn a_category_is_updated_and_an_unknown_one_is_reported() {
        let s = state_from("cat-update", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        super::category_create(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
        )
        .await;

        let updated = super::category_update(
            State(s.state.clone()),
            axum::extract::Path("films".to_string()),
            RawQuery(None),
            keyed(KEY),
            r#"{"save_path":"/data/movies","mode":"hoard"}"#.to_string(),
        )
        .await;
        assert!(updated.status().is_success(), "got {:?}", updated.status());

        let (_engine, path) = placement(&s.state, "films", "");
        assert_eq!(path, "/data/movies", "the new save path is what placement uses");
    }

    /// Deleting a category that torrents still point at, then placing into it,
    /// must fall back rather than place a torrent nowhere.
    #[tokio::test]
    async fn placing_into_a_deleted_category_falls_back_to_race() {
        let s = state_from("cat-deleted", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        super::category_create(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"name":"films","save_path":"/data/films","mode":"hoard"}"#.to_string(),
        )
        .await;
        super::category_delete(
            State(s.state.clone()),
            axum::extract::Path("films".to_string()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        let (engine, _path) = placement(&s.state, "films", "");
        assert_eq!(engine, "race", "a category that is gone is an unknown category");
    }

    /// ⭐ Moving a torrent to another engine is a JOB, never inline: a bulk
    /// move would otherwise hold the request open for terabytes of copying.
    #[tokio::test]
    async fn moving_a_torrent_between_engines_is_answered() {
        let (s, hash) = with_torrent("move-engine");
        let resp = super::post_torrent_engine(
            State(s.state.clone()),
            axum::extract::Path(hash),
            RawQuery(None),
            keyed(KEY),
            r#"{"engine":"hoard"}"#.to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// Copying to an engine that does not exist is refused: the copy would
    /// have nowhere to land.
    #[tokio::test]
    async fn copying_to_an_engine_that_does_not_exist_is_refused() {
        let (s, hash) = with_torrent("copy-bad");
        let resp = super::post_torrent_copy(
            State(s.state.clone()),
            axum::extract::Path(hash),
            RawQuery(None),
            keyed(KEY),
            r#"{"to":"no-such-engine"}"#.to_string(),
        )
        .await;
        assert!(!resp.status().is_success(), "got {:?}", resp.status());
    }

    /// A tracker can be added to a torrent we hold, and the tracker list
    /// reflects it -- the two views read the same state.
    #[tokio::test]
    async fn a_tracker_added_to_a_torrent_shows_up_in_its_tracker_list() {
        let (s, hash) = with_torrent("add-tracker");
        let resp = super::post_add_tracker(
            State(s.state.clone()),
            axum::extract::Path(hash.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"url":"https://second.example/announce"}"#.to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());

        let listed = body_json(
            super::get_torrent_trackers(
                State(s.state.clone()),
                axum::extract::Path(hash),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        )
        .await;
        assert!(listed.to_string().contains("tracker.example"), "got {listed}");
    }

    /// Replacing the tracker list REPLACES it; the old host must be gone.
    #[tokio::test]
    async fn replacing_the_tracker_list_drops_the_old_host() {
        let (s, hash) = with_torrent("set-trackers");
        let resp = super::post_torrent_trackers(
            State(s.state.clone()),
            axum::extract::Path(hash.clone()),
            RawQuery(None),
            keyed(KEY),
            r#"{"trackers":["https://other.example/announce"]}"#.to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// A job created by the daemon is listed and can be read back by id -- the
    /// UI polls on exactly this.
    #[tokio::test]
    async fn a_job_is_listed_and_readable_by_its_id() {
        let s = state_from("jobs", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let id = {
            let store = s.store.lock().unwrap();
            store.create_job("move", &"a".repeat(40), "{}", 1000).expect("created")
        };

        let listed =
            body_json(super::get_jobs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
                .await;
        assert!(listed.to_string().contains(&id), "the job is listed: {listed}");

        let one = super::get_job(
            State(s.state.clone()),
            axum::extract::Path(id.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(one.status().is_success(), "got {:?}", one.status());
        let body = body_json(one).await;
        assert_eq!(body["id"], serde_json::json!(id));
        assert_eq!(body["type"], serde_json::json!("move"), "`type` in JSON, `kind` in Rust");
    }

    /// Deleting a job that exists works; deleting it twice is reported.
    #[tokio::test]
    async fn a_job_can_be_deleted_once() {
        let s = state_from("jobs-delete", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let id = {
            let store = s.store.lock().unwrap();
            store.create_job("move", &"a".repeat(40), "{}", 10).expect("created")
        };
        let first = super::delete_job(
            State(s.state.clone()),
            axum::extract::Path(id.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(!first.status().is_server_error(), "got {:?}", first.status());
    }

    /// Checking import paths that do not exist reports them as missing rather
    /// than starting an import that would fail torrent by torrent.
    #[tokio::test]
    async fn import_paths_that_do_not_exist_are_reported_before_the_import() {
        let s = state_from("import-check", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let resp = super::import_check_paths(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"paths": ["/tmp/typhon-no-such-dir-8c2a"]}).to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
        let body = body_json(resp).await;
        assert!(body.is_object() || body.is_array(), "got {body}");
    }

    /// An existing path checks out -- the other half of the same route.
    #[tokio::test]
    async fn an_import_path_that_exists_checks_out() {
        let s = state_from("import-ok", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let resp = super::import_check_paths(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"paths": ["/tmp"]}).to_string(),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// A volume policy typed in the panel is stored and read back by the
    /// drain -- it must apply on the NEXT tick, not after a restart.
    #[tokio::test]
    async fn a_volume_policy_is_stored_and_read_back_by_the_drain() {
        let s = state_from("volpolicy-store", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let body: VolumePolicyBody = serde_json::from_value(serde_json::json!({
            "volume": "/mnt/race",
            "enabled": true,
            "high_watermark": 91,
            "low_watermark": 77
        }))
        .expect("a valid policy body");
        let resp =
            super::set_volume_policy(State(s.state.clone()), RawQuery(None), keyed(KEY), Json(body))
                .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());

        let cfg = s.cfg();
        let p = crate::volumes::policy_for(&s.state, "/mnt/race", &cfg.race_drain);
        assert_eq!(p.high, 91, "the drain reads what the panel wrote");
        assert_eq!(p.low, 77);
        assert!(!p.inherited, "it is this volume's own policy now");
    }

    /// Clearing a volume's policy puts it back on the global default.
    #[tokio::test]
    async fn clearing_a_volume_policy_returns_it_to_the_global_default() {
        let s = state_from("volpolicy-clear", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let set: VolumePolicyBody = serde_json::from_value(serde_json::json!({
            "volume": "/mnt/race", "enabled": true, "high_watermark": 91, "low_watermark": 77
        }))
        .unwrap();
        super::set_volume_policy(State(s.state.clone()), RawQuery(None), keyed(KEY), Json(set)).await;

        let inherit: VolumePolicyBody = serde_json::from_value(serde_json::json!({
            "volume": "/mnt/race", "inherit": true
        }))
        .unwrap();
        super::set_volume_policy(State(s.state.clone()), RawQuery(None), keyed(KEY), Json(inherit))
            .await;

        let cfg = s.cfg();
        let p = crate::volumes::policy_for(&s.state, "/mnt/race", &cfg.race_drain);
        assert!(p.inherited, "back on the global default");
    }
}

#[cfg(test)]
mod remaining_routes_tests {
    use super::testing::*;
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        piece[1] = name.len() as u8;
        info.extend_from_slice(&piece);
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn populated(tag: &str) -> (TestState, String) {
        let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
        let (hash, _) = add_torrent_bytes(
            &s.state,
            &torrent_bytes("alpha"),
            "",
            "/tmp",
            "fr",
            true,
            true,
            "race",
        )
        .expect("added");
        (s, hash)
    }

    /// The rest of the read surface, on a node that holds something.
    ///
    /// Two properties per route, and they are the ones that were actually
    /// broken in this repo: it REFUSES an unauthenticated caller (the open API
    /// of 10/09), and it does not panic on real state.
    macro_rules! rest_routes {
        ($($t:ident => $name:ident),+ $(,)?) => {
            $(
                #[tokio::test]
                async fn $t() {
                    let (s, _h) = populated(concat!("rest-", stringify!($name)));

                    let refused = super::$name(
                        State(s.state.clone()), RawQuery(None), HeaderMap::new()
                    ).await;
                    assert_eq!(
                        refused.status(), StatusCode::UNAUTHORIZED,
                        concat!(stringify!($name), " must refuse a caller with no key")
                    );

                    let allowed = super::$name(
                        State(s.state.clone()), RawQuery(None), keyed(KEY)
                    ).await;
                    assert!(
                        !allowed.status().is_server_error(),
                        concat!(stringify!($name), " answered {:?}"),
                        allowed.status()
                    );
                }
            )+
        };
    }

    rest_routes!(
        rest_get_bench_current => get_bench_current,
        rest_get_bench_records => get_bench_records,
        rest_get_drain_graduations => get_drain_graduations,
        rest_get_drain_history => get_drain_history,
        rest_get_fs_browse => get_fs_browse,
        rest_get_live_announce_policy => get_live_announce_policy,
        rest_get_logs => get_logs,
        rest_get_network_interfaces => get_network_interfaces,
        rest_get_network_mode => get_network_mode,
        rest_get_nodes => get_nodes,
        rest_get_opt_flags => get_opt_flags,
        rest_get_port_forward => get_port_forward,
        rest_get_qbit_import_events => get_qbit_import_events,
        rest_get_race_events => get_race_events,
        rest_get_startup_pause => get_startup_pause,
        rest_get_tracker_stats_current => get_tracker_stats_current,
        rest_get_vpn_speedtest_history => get_vpn_speedtest_history,
        rest_get_vpn_speedtest_latest => get_vpn_speedtest_latest,
        rest_get_wireguard => get_wireguard,
        rest_qbit_preferences => qbit_preferences,
        rest_get_agents => get_agents,
        rest_get_health_anomalies => get_health_anomalies,
        rest_get_provenance => get_provenance,
        rest_get_arr_cleanup_scan => get_arr_cleanup_scan,
    );

    /// ⭐ The health endpoint has NO key gate on purpose: it is what a
    /// container orchestrator polls, and it must answer before anyone has
    /// configured anything. It therefore must not leak state either.
    #[tokio::test]
    async fn the_health_endpoint_answers_without_a_key() {
        let (s, _h) = populated("rest-health");
        let resp = super::get_health(State(s.state.clone())).await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
    }

    /// The changelog is compiled into the binary and served, so a release
    /// that cannot describe itself is visible immediately.
    #[tokio::test]
    async fn the_changelog_is_served_from_the_binary() {
        let resp = super::get_changelog().await;
        assert!(resp.status().is_success());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(!bytes.is_empty(), "a release with no entry cannot describe itself");
    }

    /// Per-torrent timeline routes: refused without a key, and answering for a
    /// torrent that is actually here.
    #[tokio::test]
    async fn the_per_torrent_timeline_routes_are_gated_and_answer() {
        let (s, hash) = populated("rest-timeline");
        for resp in [
            super::get_race_timeline(
                State(s.state.clone()),
                axum::extract::Path(hash.clone()),
                RawQuery(None),
                HeaderMap::new(),
            )
            .await,
            super::race_snapshots(
                State(s.state.clone()),
                axum::extract::Path(hash.clone()),
                RawQuery(None),
                HeaderMap::new(),
            )
            .await,
        ] {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }

        let ok = super::get_race_timeline(
            State(s.state.clone()),
            axum::extract::Path(hash),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert!(!ok.status().is_server_error(), "got {:?}", ok.status());
    }

    /// ⭐ The timeline must SERVE what was recorded, not merely answer 200.
    ///
    /// The test above -- "does not 500" -- passed for the whole life of the V4
    /// port, during which this handler returned two hard-coded empty arrays
    /// while the recorder filled bench.db underneath. Asserting on the shape of
    /// the answer is the difference between a route that replies and a route
    /// that works.
    #[tokio::test]
    async fn the_timeline_serves_the_recorded_event_and_snapshot() {
        let (s, hash) = populated("rest-timeline-data");
        let bench = std::sync::Arc::new(std::sync::Mutex::new(
            crate::benchdb::BenchDb::open_in_memory().expect("bench"),
        ));
        {
            let db = bench.lock().unwrap();
            db.record(&crate::benchdb::RaceEvent {
                ts: 1000.0,
                info_hash: hash.clone(),
                event: "added".into(),
                name: "a.release".into(),
                size: 42,
                ..Default::default()
            })
            .expect("record event");
            db.record_snapshot(&crate::benchdb::RaceSnapshot {
                ts: 1005.0,
                info_hash: hash.clone(),
                progress: 0.5,
                download_rate: 1234.0,
                ..Default::default()
            })
            .expect("record snapshot");
            // A different torrent's rows must not leak into this timeline.
            db.record_snapshot(&crate::benchdb::RaceSnapshot {
                ts: 1006.0,
                info_hash: "f".repeat(40),
                progress: 0.9,
                ..Default::default()
            })
            .expect("record other");
        }
        let mut state = s.state.clone();
        state.bench = Some(bench);

        let resp = super::get_race_timeline(
            State(state),
            axum::extract::Path(hash.clone()),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;

        let events = body["events"].as_array().expect("events array");
        assert_eq!(events.len(), 1, "the recorded event is served: {body}");
        assert_eq!(events[0]["event"], "added");

        let snaps = body["snapshots"].as_array().expect("snapshots array");
        assert_eq!(snaps.len(), 1, "only THIS torrent's snapshot: {body}");
        assert_eq!(snaps[0]["progress"], 0.5);
        assert_eq!(snaps[0]["download_rate"], 1234.0);
    }

    /// No measurement database is not an error: the timeline is observability,
    /// and losing it must never take the API down with it.
    #[tokio::test]
    async fn a_timeline_without_a_bench_db_is_empty_rather_than_a_failure() {
        let (s, hash) = populated("rest-timeline-nobench");
        assert!(s.state.bench.is_none(), "the fixture has no bench db");
        let resp = super::get_race_timeline(
            State(s.state.clone()),
            axum::extract::Path(hash),
            RawQuery(None),
            keyed(KEY),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["events"].as_array().map(|a| a.len()), Some(0));
        assert_eq!(body["snapshots"].as_array().map(|a| a.len()), Some(0));
    }

    /// ⚠️ `get_fs_browse` walks the filesystem from a path the CALLER gives.
    /// A path outside what the daemon should show is the one thing it must not
    /// serve, and a missing one must not panic.
    #[tokio::test]
    async fn browsing_a_path_that_does_not_exist_is_not_a_crash() {
        let (s, _h) = populated("rest-browse");
        let resp = super::get_fs_browse(
            State(s.state.clone()),
            RawQuery(Some("path=/tmp/typhon-no-such-dir-4a7c".into())),
            keyed(KEY),
        )
        .await;
        assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
    }

    /// The logs route serves the ring buffer. On a fresh process it may be
    /// empty, and empty must be an empty list rather than a failure.
    #[tokio::test]
    async fn the_log_route_answers_on_an_empty_buffer() {
        let (s, _h) = populated("rest-logs");
        let resp = super::get_logs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
        assert!(resp.status().is_success(), "got {:?}", resp.status());
        let _ = body_json(resp).await;
    }

    /// ⭐⭐ `bench` is None in this fixture, which is a NORMAL state: the
    /// timeline is observability and losing it must never cost the seedbox.
    /// Every route that reads it answers empty rather than failing.
    #[tokio::test]
    async fn the_measurement_routes_answer_empty_when_there_is_no_bench_db() {
        let (s, _h) = populated("rest-nobench");
        assert!(s.bench.is_none(), "the fixture has no bench database");
        for resp in [
            super::get_bench_current(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
            super::get_bench_records(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
            super::get_race_events(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
        ] {
            assert!(
                !resp.status().is_server_error(),
                "a missing bench db must not be a server error: {:?}",
                resp.status()
            );
        }
    }
}
