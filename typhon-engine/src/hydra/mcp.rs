//! Model Context Protocol endpoint: `POST /mcp`.
//!
//! An agent (Claude Code, Silas, anything that speaks MCP) talks JSON-RPC 2.0
//! here instead of stitching `curl` calls against two hundred routes it has to
//! guess. The guessing was measured to go wrong: a `?category=` that
//! `/api/hoard/torrents` silently ignores cost a 247 MB download to filter
//! locally, and engine routes are nested as `/api/engines/:id/*`, which no
//! caller finds on the first try.
//!
//! Stateless "streamable HTTP": every POST gets one JSON answer. No session,
//! no SSE stream -- nothing here needs the server to speak first.
//!
//! A curated set of tools, not one per route: an agent shown two hundred
//! tools picks badly, and every schema costs it context on every session.
//! Three tiers:
//!   - reads, which answer an AGGREGATE or a PAGE -- at 900k torrents a tool
//!     that returned the library would fill an agent's context before it
//!     could read the answer;
//!   - reversible writes (pause, labels, add...), on by default;
//!   - destructive writes (delete, purge), not even LISTED unless
//!     `[mcp] allow_destructive = true`, and marked destructive so the client
//!     asks before calling them.
//! Every write names its torrents by info_hash. None takes a filter: an agent
//! must not be able to delete "everything in error" by misreading one.
//!
//! Writes go through the real `/api` routes, dispatched in-process: the same
//! validation, the same refusals, the same store writes the UI gets. A second
//! implementation here would be a second place for them to drift.
//!
//! Kept out of api.rs like the workflow routes. Same `authorised` gate as the
//! rest of `/api`, plus `Authorization: Bearer`, which is the header MCP
//! clients know how to send.

use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};

use crate::api::{self, AppState};

/// Newest first: an unknown version from the client is answered with ours.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// A page an agent asked for without saying how big. Small, because every row
/// is context the agent pays for.
const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 200;

/// How many torrents one write may name. Pause and resume are one store
/// transaction whatever the count; the per-torrent calls are not, and a call
/// that runs for minutes looks hung to the agent waiting on it.
const MAX_BULK: usize = 1000;
const MAX_PER_TORRENT: usize = 100;
const MAX_RECHECK: usize = 50;

/// A .torrent is a few hundred KB; Calewood refuses above 15 MiB, so will we.
const MAX_TORRENT_BYTES: usize = 15 << 20;

const INSTRUCTIONS: &str = "Hydranos is a BitTorrent seedbox. Each torrent lives in one \
engine (usually `race` for fresh releases and `hoard` for the long-term library). \
Start with `overview` to see engines, counts and error classes. Use `find_torrents` \
to page through torrents with filters (never expect the whole library in one call) \
and pass the info_hashes it returns to the other tools. Writes always name torrents \
by info_hash; there is no write by filter.";

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/mcp",
        axum::routing::post(post_mcp).get(no_stream).delete(no_stream),
    )
}

/// GET would open a server-to-client stream and DELETE would end a session;
/// this endpoint has neither. The spec's answer to both is 405.
async fn no_stream() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

/// The `/api` gate, or the same key sent as a bearer token.
fn allowed(state: &AppState, headers: &HeaderMap, query: &str) -> bool {
    if api::authorised(state, headers, query) {
        return true;
    }
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return false;
    };
    // Re-enter the one gate rather than compare here: a second comparison
    // would be a second place for the empty-key and timing rules to drift.
    let mut h = HeaderMap::new();
    match token.parse() {
        Ok(v) => {
            h.insert("X-Api-Key", v);
            api::authorised(state, &h, "")
        }
        Err(_) => false,
    }
}

fn destructive_allowed(state: &AppState) -> bool {
    state.cfg().mcp.allow_destructive
}

fn rpc_result(id: Value, result: Value) -> Response {
    Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn rpc_error(status: StatusCode, id: Value, code: i64, message: &str) -> Response {
    (
        status,
        Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})),
    )
        .into_response()
}

pub(crate) async fn post_mcp(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let query = query.unwrap_or_default();
    if !allowed(&state, &headers, &query) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer")],
            Json(json!({"error": "Invalid or missing API key"})),
        )
            .into_response();
    }

    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(StatusCode::BAD_REQUEST, Value::Null, -32700, "parse error"),
    };
    // Batches were dropped from the protocol in 2025-06-18; answering one
    // half-way would be worse than refusing it.
    if msg.is_array() {
        return rpc_error(StatusCode::BAD_REQUEST, Value::Null, -32600, "batching is not supported");
    }
    // No id: a notification (or a response to a request we never send).
    // Nothing to answer, and the spec wants 202 with no body.
    let Some(id) = msg.get("id").cloned().filter(|v| !v.is_null()) else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "hydranos", "title": "Hydranos", "version": api::HYDRANOS_VERSION},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": tools(destructive_allowed(&state))})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            if !tool_names(true).iter().any(|n| *n == name) {
                return rpc_error(StatusCode::OK, id, -32602, &format!("unknown tool: {name}"));
            }
            let outcome = if DESTRUCTIVE.iter().any(|n| *n == name) && !destructive_allowed(&state) {
                // Not listed, and refused if called anyway: a client that
                // cached an older tool list must not get around the switch.
                Err(format!(
                    "`{name}` is disabled on this node; set `allow_destructive = true` under `[mcp]` in the configuration to enable it"
                ))
            } else {
                call_tool(&state, name, &args).await
            };
            // A tool that fails answers a RESULT flagged isError, not a protocol
            // error: the agent is meant to read the message and correct itself.
            match outcome {
                Ok(v) => rpc_result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": v.to_string()}],
                        "structuredContent": v,
                        "isError": false,
                    }),
                ),
                Err(e) => rpc_result(
                    id,
                    json!({"content": [{"type": "text", "text": e}], "isError": true}),
                ),
            }
        }
        _ => rpc_error(StatusCode::OK, id, -32601, &format!("method not found: {method}")),
    }
}

async fn call_tool(state: &AppState, name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "overview" => overview(state).await,
        "find_torrents" => find_torrents(state, args).await,
        "torrent_detail" => torrent_detail(state, args).await,
        "torrent_files" => torrent_files(state, args).await,
        "tracker_errors" => tracker_errors(state, args).await,
        "trackers" => trackers(state, args).await,
        "categories" => categories(state).await,
        "health" => health(state).await,
        "drain" => drain(state, args).await,
        "jobs" => jobs(state, args).await,
        "logs" => logs(state, args).await,
        "pause" => pause_resume(state, args, true).await,
        "resume" => pause_resume(state, args, false).await,
        "reannounce" => reannounce(state, args).await,
        "recheck" => recheck(state, args).await,
        "set_category" => set_category(state, args).await,
        "set_tags" => set_tags(state, args).await,
        "move_to_category" => move_to_category(state, args).await,
        "add_torrent" => add_torrent(state, args).await,
        "delete_torrents" => delete_torrents(state, args).await,
        "purge_race" => purge_race(state, args).await,
        _ => Err(format!("unknown tool: {name}")),
    }
}

// ---------------------------------------------------------------------------
// Tool catalogue
// ---------------------------------------------------------------------------

const DESTRUCTIVE: &[&str] = &["delete_torrents", "purge_race"];

fn tool_names(with_destructive: bool) -> Vec<&'static str> {
    ALL_NAMES
        .iter()
        .copied()
        .filter(|n| with_destructive || !DESTRUCTIVE.contains(n))
        .collect()
}

const ALL_NAMES: &[&str] = &[
    "overview", "find_torrents", "torrent_detail", "torrent_files", "tracker_errors",
    "trackers", "categories", "health", "drain", "jobs", "logs",
    "pause", "resume", "reannounce", "recheck", "set_category", "set_tags", "move_to_category",
    "add_torrent",
    "delete_torrents", "purge_race",
];

fn read_only(title: &str) -> Value {
    json!({"title": title, "readOnlyHint": true, "destructiveHint": false,
           "idempotentHint": true, "openWorldHint": false})
}

fn reversible(title: &str, idempotent: bool, open_world: bool) -> Value {
    json!({"title": title, "readOnlyHint": false, "destructiveHint": false,
           "idempotentHint": idempotent, "openWorldHint": open_world})
}

fn destructive(title: &str) -> Value {
    json!({"title": title, "readOnlyHint": false, "destructiveHint": true,
           "idempotentHint": true, "openWorldHint": false})
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

fn hashes_schema(max: usize) -> Value {
    json!({"type": "array", "minItems": 1, "maxItems": max,
           "items": {"type": "string", "pattern": "^[0-9a-fA-F]{40}$"},
           "description": format!("info_hashes (40 hex characters), at most {max}.")})
}

fn tool(name: &str, description: &str, schema: Value, annotations: Value) -> Value {
    let title = annotations["title"].clone();
    json!({"name": name, "title": title, "description": description,
           "inputSchema": schema, "annotations": annotations})
}

fn tools(with_destructive: bool) -> Value {
    let engine = json!({"type": "string",
        "description": "Engine id, as listed by `overview`. Defaults to `hoard`."});
    let limit = |def: usize, max: usize| json!({"type": "integer", "minimum": 1, "maximum": max,
        "description": format!("Default {def}.")});
    let mut v = vec![
        tool("overview",
            "Every engine on this node with its torrent count, live rates, and counts by state, \
             category, tracker and tracker-error class. The cheapest way to know what is there \
             before filtering. Takes a few seconds on a library near a million torrents.",
            obj(json!({}), &[]), read_only("Library overview")),
        tool("find_torrents",
            "One page of torrents from one engine, filtered and sorted. Filters are ANDed. \
             `filtered` in the answer is the number of matches; follow `next_offset` to page. \
             Values for category, tracker, state and error_class are the keys `overview` reports.",
            obj(json!({
                "engine": engine,
                "search": {"type": "string", "description": "Words matched against the name (all must match), or an info_hash of at least 6 hex characters."},
                "category": {"type": "string"},
                "tag": {"type": "string"},
                "tracker": {"type": "string", "description": "Tracker host, e.g. `tracker.example.org`."},
                "state": {"type": "string", "description": "seeding, downloading, stopped, queued, error..."},
                "error_class": {"type": "string", "description": "Tracker-error class: dead, auth, throttled, unreachable, other."},
                "sort": {"type": "string", "description": "Row field to sort on, e.g. added_time, total_size, ratio, total_upload, upload_rate, name. Default added_time."},
                "order": {"type": "string", "enum": ["asc", "desc"], "description": "Default desc."},
                "offset": {"type": "integer", "minimum": 0},
                "limit": limit(DEFAULT_LIMIT, MAX_LIMIT),
            }), &[]),
            read_only("Find torrents")),
        tool("torrent_detail",
            "Everything known about one torrent on this node: its full row, the engine it lives \
             in, and its live tracker list.",
            obj(json!({"info_hash": {"type": "string", "description": "40 hex characters."}}), &["info_hash"]),
            read_only("Torrent detail")),
        tool("torrent_files",
            "The files inside one torrent, with their sizes.",
            obj(json!({"info_hash": {"type": "string"}, "limit": limit(100, 1000)}), &["info_hash"]),
            read_only("Torrent files")),
        tool("tracker_errors",
            "Torrents whose tracker answers with an error, grouped by class (dead = unregistered \
             on the tracker, auth = passkey refused, throttled = rate limit, unreachable = \
             network), each with the trackers involved and a few sample messages.",
            obj(json!({
                "engine": engine,
                "error_class": {"type": "string", "description": "Only this class. Default: every class present."},
                "samples": {"type": "integer", "minimum": 0, "maximum": 20, "description": "Example torrents per class. Default 3."},
            }), &[]),
            read_only("Tracker errors")),
        tool("trackers",
            "Every tracker this node announces to: torrents on it, whether its last announce \
             worked, the last error, passkey and announce settings. Largest first.",
            obj(json!({
                "problems_only": {"type": "boolean", "description": "Only trackers whose status is not ok. Default false."},
                "include_hidden": {"type": "boolean", "description": "Include trackers hidden from the UI (public/DHT-style hosts). Default false."},
                "limit": limit(50, 500),
            }), &[]),
            read_only("Trackers")),
        tool("categories",
            "Every category: its save path and its mode (race or hoard). A torrent added with an \
             unknown category goes to race, so check here first.",
            obj(json!({}), &[]), read_only("Categories")),
        tool("health",
            "The anomaly scan: torrents seeding without data, stuck, re-downloading, ghost or \
             orphan files, tracker outages, and the efficiency figure. Takes a few seconds.",
            obj(json!({}), &[]), read_only("Health")),
        tool("drain",
            "The volume drain: per volume, used and allocated space against its watermarks, and \
             the most recent drain runs (what was freed, what was graduated).",
            obj(json!({"history": limit(10, 100)}), &[]), read_only("Drain status")),
        tool("jobs",
            "Background jobs (data moves, imports), newest first, with their progress.",
            obj(json!({
                "state": {"type": "string", "description": "Only jobs in this state, e.g. running, done, failed."},
                "limit": limit(20, 200),
            }), &[]),
            read_only("Jobs")),
        tool("logs",
            "The most recent lines of the daemon log, oldest first, filtered by level and text. \
             The buffer is in memory: it covers minutes, not days.",
            obj(json!({
                "level": {"type": "string", "enum": ["ERROR", "WARN", "INFO", "DEBUG"], "description": "Minimum level. Default WARN."},
                "contains": {"type": "string", "description": "Case-insensitive text the line must contain."},
                "limit": limit(50, 500),
            }), &[]),
            read_only("Logs")),
        tool("pause",
            "Stop torrents. They stay in the library with their data; `resume` starts them again.",
            obj(json!({"info_hashes": hashes_schema(MAX_BULK)}), &["info_hashes"]),
            reversible("Pause torrents", true, false)),
        tool("resume",
            "Start stopped torrents again.",
            obj(json!({"info_hashes": hashes_schema(MAX_BULK)}), &["info_hashes"]),
            reversible("Resume torrents", true, false)),
        tool("reannounce",
            "Announce these torrents to their trackers now instead of at the next interval.",
            obj(json!({"info_hashes": hashes_schema(MAX_PER_TORRENT)}), &["info_hashes"]),
            reversible("Reannounce", true, true)),
        tool("recheck",
            "Verify the data of these torrents against their piece hashes, in the background. \
             A paused torrent is checked and stays paused.",
            obj(json!({"info_hashes": hashes_schema(MAX_RECHECK)}), &["info_hashes"]),
            reversible("Recheck data", true, false)),
        tool("set_category",
            "Set the category of these torrents. A label only: the files are not moved. The \
             category must exist (see `categories`); an empty string removes it.",
            obj(json!({"info_hashes": hashes_schema(MAX_PER_TORRENT), "category": {"type": "string"}}),
                &["info_hashes", "category"]),
            reversible("Set category", true, false)),
        tool("set_tags",
            "Replace the tags of these torrents with exactly this list (an empty list clears them).",
            obj(json!({"info_hashes": hashes_schema(MAX_PER_TORRENT),
                       "tags": {"type": "array", "items": {"type": "string"}}}),
                &["info_hashes", "tags"]),
            reversible("Set tags", true, false)),
        tool("move_to_category",
            "Set the category of these torrents AND move their data to its save path. Each move \
             is a background job (see `jobs`); the torrent keeps seeding while data crossing \
             filesystems is copied. A file hardlinked elsewhere is only copied across \
             filesystems with allow_breaking_hardlinks, which doubles the space it takes.",
            obj(json!({"info_hashes": hashes_schema(MAX_PER_TORRENT), "category": {"type": "string"},
                       "allow_breaking_hardlinks": {"type": "boolean", "description": "Default false."}}),
                &["info_hashes", "category"]),
            reversible("Move to category", false, false)),
        tool("add_torrent",
            "Add one .torrent, from a path on this node or an http(s) URL. The category is \
             required and must exist: it decides the engine and the save path.",
            obj(json!({
                "torrent_path": {"type": "string", "description": "Path of a .torrent file on this node."},
                "torrent_url": {"type": "string", "description": "http(s) URL of a .torrent file."},
                "category": {"type": "string"},
                "save_path": {"type": "string", "description": "Override the category's save path. Must be where the data is, or will be."},
                "tags": {"type": "array", "items": {"type": "string"}},
                "paused": {"type": "boolean", "description": "Add stopped. Default false."},
                "engine": {"type": "string", "description": "Force an engine instead of the category's."},
            }), &["category"]),
            reversible("Add torrent", true, true)),
    ];
    if with_destructive {
        v.push(tool("delete_torrents",
            "Remove torrents from Hydranos. With `delete_files` their data is deleted from disk \
             too, which cannot be undone.",
            obj(json!({
                "info_hashes": hashes_schema(MAX_PER_TORRENT),
                "delete_files": {"type": "boolean", "description": "Also delete the data. Default false."},
                "engine": {"type": "string", "description": "Only the copy in this engine. Default: every copy."},
            }), &["info_hashes"]),
            destructive("Delete torrents")));
        v.push(tool("purge_race",
            "Remove race torrents and free their slot and data on the race volume.",
            obj(json!({"info_hashes": hashes_schema(MAX_PER_TORRENT)}), &["info_hashes"]),
            destructive("Purge race torrents")));
    }
    Value::Array(v)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Percent-encode a query value. Unreserved characters only pass through, so
/// a search for "a&b=c" cannot become two parameters.
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn arg_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn arg_usize(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn arg_bool(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn is_hash(h: &str) -> bool {
    h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The `info_hashes` of a write: present, well-formed, deduplicated, bounded.
/// Every one is checked before anything is touched -- a list that fails on
/// its tenth entry must not have acted on the first nine.
fn arg_hashes(args: &Value, max: usize) -> Result<Vec<String>, String> {
    let list = args
        .get("info_hashes")
        .and_then(Value::as_array)
        .ok_or("info_hashes must be a list of info_hashes")?;
    if list.is_empty() {
        return Err("info_hashes is empty".into());
    }
    let mut out: Vec<String> = Vec::new();
    for v in list {
        let h = v.as_str().unwrap_or("").trim().to_ascii_lowercase();
        if !is_hash(&h) {
            return Err(format!("not an info_hash: {v}"));
        }
        if !out.contains(&h) {
            out.push(h);
        }
    }
    if out.len() > max {
        return Err(format!("{} torrents named; this tool takes at most {max} per call", out.len()));
    }
    Ok(out)
}

/// The engine a call is about. Refused by name when unknown, listing the
/// real ones: an empty page would read as "no such torrents".
fn pick_engine(state: &AppState, args: &Value) -> Result<String, String> {
    let ids: Vec<String> = state.engines.engines().iter().map(|e| e.id.clone()).collect();
    let wanted = arg_str(args, "engine").unwrap_or_else(|| {
        if ids.iter().any(|i| i == "hoard") { "hoard".into() } else { ids.first().cloned().unwrap_or_default() }
    });
    if ids.iter().any(|i| *i == wanted) {
        Ok(wanted)
    } else {
        Err(format!("unknown engine `{wanted}`; this node runs: {}", ids.join(", ")))
    }
}

/// A facet map as a list, largest first, cut at `n`. A JSON object would
/// lose the order, and the order is the point.
fn top(facet: &Value, n: usize) -> Value {
    let mut v: Vec<(String, i64)> = facet
        .as_object()
        .map(|o| o.iter().map(|(k, c)| (k.clone(), c.as_i64().unwrap_or(0))).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = v.len();
    let list: Vec<Value> = v.into_iter().take(n).map(|(k, c)| json!({"name": k, "count": c})).collect();
    if total > n {
        json!({"top": list, "distinct": total})
    } else {
        Value::Array(list)
    }
}

const ROW_FIELDS: &[&str] = &[
    "name", "info_hash", "state", "progress", "total_size", "category", "tags",
    "tracker_host", "ratio", "total_upload", "upload_rate", "download_rate",
    "num_peers", "num_seeds", "added_time", "save_path", "agent", "user_paused",
];

/// The fields an agent reasons with. A full row is ~35 fields; twenty-five of
/// them on a page is context spent on counters nobody asked about.
fn compact(row: &Value) -> Value {
    let mut o = Map::new();
    for f in ROW_FIELDS {
        if let Some(v) = row.get(*f).filter(|v| !v.is_null()) {
            o.insert((*f).to_string(), v.clone());
        }
    }
    if row.get("tracker_error").and_then(Value::as_bool) == Some(true) {
        let msg = row.get("tracker_error_msg").and_then(Value::as_str).unwrap_or("");
        o.insert("tracker_error".into(), json!(msg));
        o.insert("error_class".into(), json!(crate::errclass::classify(msg)));
    }
    if row.get("torrent_error").and_then(Value::as_bool) == Some(true) {
        o.insert("torrent_error".into(), row.get("torrent_error_msg").cloned().unwrap_or(Value::Null));
    }
    Value::Object(o)
}

fn rows_of(page: &Value) -> Vec<Value> {
    page.get("rows").and_then(Value::as_array).cloned().unwrap_or_default()
}

/// Run one `/api` request through the real router, in-process.
///
/// The caller has already passed the gate; the request carries the node's own
/// key so the handler's `guard!` lets it through exactly as it would an
/// operator's. Built per call: a router is a table of two hundred routes, not
/// something that costs to make, and holding one would pin a state.
async fn internal(state: &AppState, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let key = state.cfg().daemon.api_key.clone();
    let mut req = axum::http::Request::builder().method(method).uri(uri).header("X-Api-Key", key);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            axum::body::Body::from(v.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let req = match req.body(body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})),
    };
    let resp = match api::router(state.clone()).oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap_or_default();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, v)
}

/// A read through `internal` that must succeed to be worth answering.
async fn get_json(state: &AppState, uri: &str) -> Result<Value, String> {
    match internal(state, Method::GET, uri, None).await {
        (s, v) if s.is_success() => Ok(v),
        (s, v) => Err(format!("{uri} answered {s}: {v}")),
    }
}

/// One outcome per torrent, and the totals an agent reads first.
fn per_torrent(results: Vec<Value>) -> Value {
    let ok = results.iter().filter(|r| r["ok"] == true).count();
    json!({"ok": ok, "failed": results.len() - ok, "results": results})
}

fn outcome(hash: &str, status: StatusCode, answer: Value) -> Value {
    json!({"info_hash": hash, "ok": status.is_success(), "status": status.as_u16(), "answer": answer})
}

fn not_here(hash: &str) -> Value {
    json!({"info_hash": hash, "ok": false, "status": 404, "answer": "no such torrent on this node"})
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

async fn overview(state: &AppState) -> Result<Value, String> {
    let mut engines = Vec::new();
    for e in state.engines.engines().iter() {
        let page = api::fleet_page(state, &e.id, "limit=1&facets=1").await;
        let f = page.get("facets").cloned().unwrap_or(Value::Null);
        let live = api::live_stats(state, &e.id);
        engines.push(json!({
            "id": e.id,
            "role": e.role,
            "listening": e.listening.load(std::sync::atomic::Ordering::Relaxed),
            "torrents": page.get("total").cloned().unwrap_or(json!(0)),
            "upload_rate": live.upload_rate,
            "download_rate": live.download_rate,
            "active_peers": live.active_peers,
            "torrents_uploading": live.torrents_uploading,
            "states": f.get("state").cloned().unwrap_or(Value::Null),
            "tracker_error": f.get("tracker_error").cloned().unwrap_or(Value::Null),
            "torrent_error": f.get("torrent_error").cloned().unwrap_or(Value::Null),
            "error_classes": f.get("error_class").cloned().unwrap_or(Value::Null),
            "uncategorized": f.get("uncategorized").cloned().unwrap_or(Value::Null),
            "categories": top(f.get("category").unwrap_or(&Value::Null), 15),
            "trackers": top(f.get("tracker").unwrap_or(&Value::Null), 15),
            "tags": top(f.get("tag").unwrap_or(&Value::Null), 15),
        }));
    }
    Ok(json!({"version": api::HYDRANOS_VERSION, "engines": engines}))
}

async fn find_torrents(state: &AppState, args: &Value) -> Result<Value, String> {
    let engine = pick_engine(state, args)?;
    let offset = arg_usize(args, "offset").unwrap_or(0);
    let limit = arg_usize(args, "limit").unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    let mut q = format!("offset={offset}&limit={limit}");
    for key in ["search", "category", "tag", "tracker", "state", "error_class", "sort"] {
        if let Some(v) = arg_str(args, key) {
            q.push_str(&format!("&{key}={}", enc(&v)));
        }
    }
    match arg_str(args, "order").as_deref() {
        None | Some("desc") => {}
        Some("asc") => q.push_str("&order=asc"),
        Some(other) => return Err(format!("order must be asc or desc, not `{other}`")),
    }

    let page = api::fleet_page(state, &engine, &q).await;
    let rows: Vec<Value> = rows_of(&page).iter().take(limit).map(compact).collect();
    let filtered = page.get("filtered").and_then(Value::as_u64).unwrap_or(0) as usize;
    let returned = rows.len();
    let mut out = json!({
        "engine": engine,
        "total": page.get("total").cloned().unwrap_or(json!(0)),
        "filtered": filtered,
        "offset": offset,
        "returned": returned,
        "rows": rows,
    });
    if offset + returned < filtered {
        out["next_offset"] = json!(offset + returned);
    }
    Ok(out)
}

fn one_hash(args: &Value) -> Result<String, String> {
    let hash = arg_str(args, "info_hash").unwrap_or_default().to_ascii_lowercase();
    if is_hash(&hash) { Ok(hash) } else { Err("info_hash must be 40 hexadecimal characters".into()) }
}

async fn torrent_detail(state: &AppState, args: &Value) -> Result<Value, String> {
    let hash = one_hash(args)?;
    let Some((engine, torrent)) = api::find_torrent(state, &hash) else {
        return Err(format!("no torrent {hash} on this node"));
    };
    let trackers = serde_json::to_value(&*torrent.live_trackers.read()).unwrap_or(Value::Null);
    let page = api::fleet_page(state, &engine, &format!("search={hash}&limit=5")).await;
    let row = rows_of(&page)
        .into_iter()
        .find(|r| r.get("info_hash").and_then(Value::as_str) == Some(hash.as_str()))
        .map(|r| match r {
            Value::Object(o) => Value::Object(o.into_iter().filter(|(_, v)| !v.is_null()).collect()),
            other => other,
        })
        .unwrap_or(Value::Null);
    Ok(json!({"engine": engine, "torrent": row, "trackers": trackers}))
}

async fn torrent_files(state: &AppState, args: &Value) -> Result<Value, String> {
    let hash = one_hash(args)?;
    let limit = arg_usize(args, "limit").unwrap_or(100).clamp(1, 1000);
    let v = get_json(state, &format!("/api/torrents/{hash}/files")).await?;
    let files = v.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    let total: u64 = files.iter().filter_map(|f| f["size"].as_u64()).sum();
    Ok(json!({
        "info_hash": hash,
        "count": files.len(),
        "total_size": total,
        "files": files.into_iter().take(limit).collect::<Vec<_>>(),
    }))
}

async fn tracker_errors(state: &AppState, args: &Value) -> Result<Value, String> {
    let engine = pick_engine(state, args)?;
    let samples = arg_usize(args, "samples").unwrap_or(3).min(20);

    let classes: Vec<(String, i64)> = match arg_str(args, "error_class") {
        Some(c) => vec![(c, -1)],
        None => {
            let page = api::fleet_page(state, &engine, "limit=1&facets=1").await;
            let mut v: Vec<(String, i64)> = page
                .pointer("/facets/error_class")
                .and_then(Value::as_object)
                .map(|o| o.iter().map(|(k, n)| (k.clone(), n.as_i64().unwrap_or(0))).collect())
                .unwrap_or_default();
            v.retain(|(_, n)| *n > 0);
            v.sort_by(|a, b| b.1.cmp(&a.1));
            v
        }
    };

    let mut out = Vec::new();
    for (class, _) in classes {
        let q = format!("error_class={}&limit={}&facets=1", enc(&class), samples.max(1));
        let page = api::fleet_page(state, &engine, &q).await;
        let examples: Vec<Value> = rows_of(&page)
            .iter()
            .take(samples)
            .map(|r| json!({
                "name": r.get("name"),
                "info_hash": r.get("info_hash"),
                "tracker_host": r.get("tracker_host"),
                "message": r.get("tracker_error_msg"),
            }))
            .collect();
        out.push(json!({
            "class": class,
            "torrents": page.get("filtered").cloned().unwrap_or(json!(0)),
            "trackers": top(page.pointer("/facets/tracker").unwrap_or(&Value::Null), 10),
            "samples": examples,
        }));
    }
    Ok(json!({"engine": engine, "classes": out}))
}

async fn trackers(state: &AppState, args: &Value) -> Result<Value, String> {
    let limit = arg_usize(args, "limit").unwrap_or(50).clamp(1, 500);
    let problems = arg_bool(args, "problems_only");
    let hidden = arg_bool(args, "include_hidden");
    let v = get_json(state, "/api/trackers").await?;
    let mut list: Vec<Value> = v.as_array().cloned().unwrap_or_default();
    list.retain(|t| hidden || t["hidden"] != true);
    list.retain(|t| !problems || t["status"] != "ok");
    list.sort_by(|a, b| b["torrents"].as_i64().unwrap_or(0).cmp(&a["torrents"].as_i64().unwrap_or(0)));
    let matched = list.len();
    list.truncate(limit);
    Ok(json!({"matched": matched, "returned": list.len(), "trackers": list}))
}

async fn categories(state: &AppState) -> Result<Value, String> {
    let v = get_json(state, "/api/categories").await?;
    Ok(json!({"categories": v}))
}

async fn health(state: &AppState) -> Result<Value, String> {
    get_json(state, "/api/health/anomalies").await
}

async fn drain(state: &AppState, args: &Value) -> Result<Value, String> {
    let n = arg_usize(args, "history").unwrap_or(10).clamp(1, 100);
    let status = get_json(state, "/api/drain/status").await?;
    let history = get_json(state, "/api/drain/history").await?;
    let mut runs: Vec<Value> = history.as_array().cloned().unwrap_or_default();
    runs.sort_by(|a, b| b["timestamp"].as_i64().unwrap_or(0).cmp(&a["timestamp"].as_i64().unwrap_or(0)));
    runs.truncate(n);
    Ok(json!({"status": status, "recent_runs": runs}))
}

async fn jobs(state: &AppState, args: &Value) -> Result<Value, String> {
    let limit = arg_usize(args, "limit").unwrap_or(20).clamp(1, 200);
    let want = arg_str(args, "state");
    let v = get_json(state, "/api/jobs").await?;
    let mut list: Vec<Value> = v.as_array().cloned().unwrap_or_default();
    if let Some(s) = &want {
        list.retain(|j| j["state"].as_str() == Some(s.as_str()));
    }
    list.sort_by(|a, b| b["updated_at"].as_i64().unwrap_or(0).cmp(&a["updated_at"].as_i64().unwrap_or(0)));
    let matched = list.len();
    list.truncate(limit);
    Ok(json!({"matched": matched, "returned": list.len(), "jobs": list}))
}

fn level_rank(l: &str) -> u8 {
    match l.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" | "WARNING" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    }
}

async fn logs(state: &AppState, args: &Value) -> Result<Value, String> {
    let limit = arg_usize(args, "limit").unwrap_or(50).clamp(1, 500);
    let min = level_rank(&arg_str(args, "level").unwrap_or_else(|| "WARN".into()));
    let needle = arg_str(args, "contains").map(|s| s.to_lowercase());
    let v = get_json(state, "/api/logs").await?;
    let mut lines: Vec<Value> = v
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|e| level_rank(e["level"].as_str().unwrap_or("")) >= min)
        .filter(|e| match &needle {
            Some(n) => e["msg"].as_str().unwrap_or("").to_lowercase().contains(n),
            None => true,
        })
        .collect();
    let matched = lines.len();
    // The newest `limit`, still in reading order.
    let skip = lines.len().saturating_sub(limit);
    lines.drain(..skip);
    Ok(json!({"matched": matched, "returned": lines.len(), "entries": lines}))
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Group hashes by the engine that holds them. A pause is sent to the engine
/// that runs the torrent; the bulk route refuses a hash it does not hold.
fn by_engine(state: &AppState, hashes: &[String]) -> (Vec<(String, Vec<String>)>, Vec<String>) {
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    let mut missing = Vec::new();
    for h in hashes {
        match api::find_torrent(state, h) {
            Some((engine, _)) => match groups.iter_mut().find(|(e, _)| *e == engine) {
                Some((_, list)) => list.push(h.clone()),
                None => groups.push((engine, vec![h.clone()])),
            },
            None => missing.push(h.clone()),
        }
    }
    (groups, missing)
}

async fn pause_resume(state: &AppState, args: &Value, stop: bool) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_BULK)?;
    let (groups, missing) = by_engine(state, &hashes);
    let action = if stop { "stop" } else { "start" };
    let mut engines = Vec::new();
    for (engine, list) in groups {
        let (status, answer) = internal(
            state,
            Method::POST,
            &format!("/api/engines/{}/torrents/bulk", enc(&engine)),
            Some(json!({"action": action, "hashes": list})),
        )
        .await;
        engines.push(json!({"engine": engine, "ok": status.is_success(), "status": status.as_u16(), "answer": answer}));
    }
    Ok(json!({"action": action, "engines": engines, "not_found": missing}))
}

async fn reannounce(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let mut results = Vec::new();
    for h in &hashes {
        let (s, v) = internal(state, Method::POST, &format!("/api/torrents/{h}/reannounce"), None).await;
        results.push(outcome(h, s, v));
    }
    Ok(per_torrent(results))
}

async fn recheck(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_RECHECK)?;
    let mut results = Vec::new();
    for h in &hashes {
        let Some((engine, _)) = api::find_torrent(state, h) else {
            results.push(not_here(h));
            continue;
        };
        let (s, v) = internal(
            state,
            Method::POST,
            &format!("/api/hoard/torrents/{h}/verify?engine={}", enc(&engine)),
            None,
        )
        .await;
        results.push(outcome(h, s, v));
    }
    Ok(per_torrent(results))
}

async fn category_names(state: &AppState) -> Result<Vec<String>, String> {
    let v = get_json(state, "/api/categories").await?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(|c| c["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// A category the node does not know is refused before anything is written:
/// the label route would store it, and the torrent would sit under a name no
/// save path or engine rule answers to.
async fn known_category(state: &AppState, category: &str) -> Result<(), String> {
    let names = category_names(state).await?;
    if names.iter().any(|n| n == category) {
        Ok(())
    } else {
        Err(format!("unknown category `{category}`; this node has: {}", names.join(", ")))
    }
}

/// One label write per torrent, on the engine that holds it.
async fn label_each(state: &AppState, hashes: &[String], what: &str, body: Value) -> Value {
    let mut results = Vec::new();
    for h in hashes {
        let Some((engine, _)) = api::find_torrent(state, h) else {
            results.push(not_here(h));
            continue;
        };
        let (s, v) = internal(
            state,
            Method::POST,
            &format!("/api/hoard/torrents/{h}/{what}?engine={}", enc(&engine)),
            Some(body.clone()),
        )
        .await;
        results.push(outcome(h, s, v));
    }
    per_torrent(results)
}

async fn set_category(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let category = args
        .get("category")
        .and_then(Value::as_str)
        .ok_or("category is required (an empty string removes it)")?
        .trim()
        .to_string();
    if !category.is_empty() {
        known_category(state, &category).await?;
    }
    Ok(label_each(state, &hashes, "category", json!({"category": category})).await)
}

async fn set_tags(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .ok_or("tags must be a list (an empty list clears them)")?
        .iter()
        .filter_map(|t| t.as_str().map(|s| s.trim().to_string()))
        .filter(|t| !t.is_empty())
        .collect();
    Ok(label_each(state, &hashes, "tags", json!({"tags": tags})).await)
}

async fn move_to_category(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let category = arg_str(args, "category").ok_or("category is required")?;
    known_category(state, &category).await?;
    let body = json!({"category": category, "move_files": true,
                      "allow_breaking_hardlinks": arg_bool(args, "allow_breaking_hardlinks")});
    let mut results = Vec::new();
    for h in &hashes {
        let Some((engine, _)) = api::find_torrent(state, h) else {
            results.push(not_here(h));
            continue;
        };
        let (s, v) = internal(
            state,
            Method::POST,
            &format!("/api/hoard/torrents/{h}/category?engine={}", enc(&engine)),
            Some(body.clone()),
        )
        .await;
        results.push(outcome(h, s, v));
    }
    Ok(per_torrent(results))
}

async fn fetch_torrent(url: &str) -> Result<Vec<u8>, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("torrent_url must be an http(s) URL".into());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} answered {}", resp.status()));
    }
    if resp.content_length().unwrap_or(0) as usize > MAX_TORRENT_BYTES {
        return Err(format!("{url} is larger than {MAX_TORRENT_BYTES} bytes"));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if bytes.len() > MAX_TORRENT_BYTES {
        return Err(format!("{url} is larger than {MAX_TORRENT_BYTES} bytes"));
    }
    Ok(bytes.to_vec())
}

async fn add_torrent(state: &AppState, args: &Value) -> Result<Value, String> {
    let category = arg_str(args, "category").ok_or("category is required")?;
    known_category(state, &category).await?;
    let bytes = match (arg_str(args, "torrent_path"), arg_str(args, "torrent_url")) {
        (Some(_), Some(_)) => return Err("give torrent_path or torrent_url, not both".into()),
        (None, None) => return Err("torrent_path or torrent_url is required".into()),
        (Some(p), None) => {
            let b = tokio::fs::read(&p).await.map_err(|e| format!("{p}: {e}"))?;
            if b.len() > MAX_TORRENT_BYTES {
                return Err(format!("{p} is larger than {MAX_TORRENT_BYTES} bytes"));
            }
            b
        }
        (None, Some(u)) => fetch_torrent(&u).await?,
    };
    let save_path = arg_str(args, "save_path").unwrap_or_default();
    let engine = arg_str(args, "engine").unwrap_or_default();
    if !engine.is_empty() {
        pick_engine(state, args)?;
    }
    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|t| t.as_str().map(|s| s.trim().to_string())).filter(|t| !t.is_empty()).collect())
        .unwrap_or_default();
    let paused = arg_bool(args, "paused");

    // The add parses the torrent and touches the store and the disk: off the
    // async workers, as the bulk writes are.
    let st = state.clone();
    let res = tokio::task::spawn_blocking(move || {
        api::add_torrent_bytes(&st, &bytes, &category, &save_path, &tags.join(","), paused, false, &engine)
    })
    .await
    .map_err(|e| format!("add panicked: {e}"))?;
    let (hash, name) = res?;
    let engine = api::find_torrent(state, &hash).map(|(e, _)| e);
    Ok(json!({"info_hash": hash, "name": name, "engine": engine}))
}

async fn delete_torrents(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let delete_files = arg_bool(args, "delete_files");
    let engine = match arg_str(args, "engine") {
        Some(_) => format!("&engine={}", enc(&pick_engine(state, args)?)),
        None => String::new(),
    };
    let mut results = Vec::new();
    for h in &hashes {
        let (s, v) = internal(
            state,
            Method::DELETE,
            &format!("/api/torrents/{h}?delete_files={delete_files}{engine}"),
            None,
        )
        .await;
        results.push(outcome(h, s, v));
    }
    let mut out = per_torrent(results);
    out["delete_files"] = json!(delete_files);
    Ok(out)
}

async fn purge_race(state: &AppState, args: &Value) -> Result<Value, String> {
    let hashes = arg_hashes(args, MAX_PER_TORRENT)?;
    let mut results = Vec::new();
    for h in &hashes {
        let (s, v) = internal(state, Method::POST, &format!("/api/race/torrents/{h}/purge"), None).await;
        results.push(outcome(h, s, v));
    }
    Ok(per_torrent(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn st_destructive(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n\n[mcp]\nallow_destructive = true\n"))
    }

    async fn call(s: &TestState, headers: HeaderMap, body: Value) -> Response {
        post_mcp(
            State(s.state.clone()),
            RawQuery(None),
            headers,
            axum::body::Bytes::from(body.to_string()),
        )
        .await
    }

    fn rpc(method: &str, params: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    }

    /// `tools/call` and its result, asserted to be a result and not a
    /// protocol error.
    async fn tool_call(s: &TestState, name: &str, args: Value) -> Value {
        let v = body_json(call(s, keyed(KEY), rpc("tools/call", json!({"name": name, "arguments": args}))).await).await;
        assert!(v.get("error").is_none(), "{name} was a protocol error: {v}");
        v["result"].clone()
    }

    async fn ok_call(s: &TestState, name: &str, args: Value) -> Value {
        let r = tool_call(s, name, args).await;
        assert_eq!(r["isError"], false, "{name} failed: {}", r["content"][0]["text"]);
        r["structuredContent"].clone()
    }

    /// A minimal single-file torrent, lengths computed (see page_tests).
    fn torrent_bytes(name: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xCDu8; 20];
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

    /// Add through the same path the API uses, so the store has the row the
    /// label and pause writes act on.
    fn add(s: &TestState, name: &str) -> String {
        let (hash, _) = api::add_torrent_bytes(&s.state, &torrent_bytes(name), "", "/tmp", "", true, true, "hoard")
            .unwrap_or_else(|e| panic!("add {name}: {e}"));
        hash
    }

    // --- protocol ---------------------------------------------------------

    #[tokio::test]
    async fn no_key_is_refused() {
        let s = st("mcp-nokey");
        let r = call(&s, HeaderMap::new(), rpc("ping", json!({}))).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    /// The default install has no key at all. The gate fails closed there, and
    /// this endpoint must not be the one route that forgot.
    #[tokio::test]
    async fn empty_configured_key_refuses_even_an_empty_bearer() {
        let s = state_from("mcp-emptykey", "");
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        let r = call(&s, h, rpc("ping", json!({}))).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bearer_is_accepted() {
        let s = st("mcp-bearer");
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, format!("Bearer {KEY}").parse().unwrap());
        let r = call(&s, h, rpc("ping", json!({}))).await;
        assert_eq!(r.status(), StatusCode::OK);
        let wrong = {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, "Bearer nope".parse().unwrap());
            h
        };
        assert_eq!(call(&s, wrong, rpc("ping", json!({}))).await.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn initialize_negotiates_the_version() {
        let s = st("mcp-init");
        let v = body_json(call(&s, keyed(KEY), rpc("initialize", json!({"protocolVersion": "2025-03-26"}))).await).await;
        assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(v["result"]["serverInfo"]["name"], "hydranos");
        assert_eq!(v["result"]["serverInfo"]["version"], api::HYDRANOS_VERSION);
        let v = body_json(call(&s, keyed(KEY), rpc("initialize", json!({"protocolVersion": "1999-01-01"}))).await).await;
        assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
    }

    #[tokio::test]
    async fn notification_gets_202_and_no_body() {
        let s = st("mcp-notif");
        let r = call(&s, keyed(KEY), json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        assert_eq!(body_json(r).await, Value::Null);
    }

    #[tokio::test]
    async fn a_batch_is_refused() {
        let s = st("mcp-batch");
        let r = call(&s, keyed(KEY), json!([rpc("ping", json!({}))])).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(r).await["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn unknown_method_and_tool_are_protocol_errors() {
        let s = st("mcp-unknown");
        let v = body_json(call(&s, keyed(KEY), rpc("resources/list", json!({}))).await).await;
        assert_eq!(v["error"]["code"], -32601);
        let v = body_json(call(&s, keyed(KEY), rpc("tools/call", json!({"name": "rm_rf", "arguments": {}}))).await).await;
        assert_eq!(v["error"]["code"], -32602);
    }

    // --- catalogue --------------------------------------------------------

    fn listed(v: &Value) -> Vec<String> {
        v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    }

    #[tokio::test]
    async fn destructive_tools_are_hidden_by_default() {
        let s = st("mcp-list");
        let v = body_json(call(&s, keyed(KEY), rpc("tools/list", json!({}))).await).await;
        let names = listed(&v);
        assert_eq!(names.len(), ALL_NAMES.len() - DESTRUCTIVE.len());
        for d in DESTRUCTIVE {
            assert!(!names.iter().any(|n| n == d), "{d} listed without the switch");
        }
        for t in v["result"]["tools"].as_array().unwrap() {
            assert_eq!(t["annotations"]["destructiveHint"], false, "{}", t["name"]);
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }

    #[tokio::test]
    async fn the_switch_lists_them_marked_destructive() {
        let s = st_destructive("mcp-list-d");
        let v = body_json(call(&s, keyed(KEY), rpc("tools/list", json!({}))).await).await;
        let names = listed(&v);
        assert_eq!(names, ALL_NAMES.iter().map(|n| n.to_string()).collect::<Vec<_>>());
        for t in v["result"]["tools"].as_array().unwrap() {
            let d = DESTRUCTIVE.iter().any(|n| *n == t["name"].as_str().unwrap());
            assert_eq!(t["annotations"]["destructiveHint"], d, "{}", t["name"]);
        }
    }

    /// A client holding an older tool list must not get around the switch by
    /// calling a name it remembers.
    #[tokio::test]
    async fn a_hidden_tool_is_refused_when_called() {
        let s = st("mcp-hidden-call");
        let h = add(&s, "keepme");
        let r = tool_call(&s, "delete_torrents", json!({"info_hashes": [h]})).await;
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("allow_destructive"));
        assert!(api::find_torrent(&s.state, &h).is_some(), "the torrent was deleted anyway");
    }

    /// The catalogue and the name list are two literals; they must not drift,
    /// or a listed tool would answer "unknown tool" when called.
    #[test]
    fn the_catalogue_and_the_names_agree() {
        for with in [false, true] {
            let listed: Vec<String> = tools(with).as_array().unwrap().iter()
                .map(|t| t["name"].as_str().unwrap().to_string()).collect();
            let names: Vec<String> = tool_names(with).iter().map(|n| n.to_string()).collect();
            assert_eq!(listed, names);
        }
    }

    // --- reads ------------------------------------------------------------

    #[tokio::test]
    async fn overview_lists_the_engines() {
        let s = st("mcp-overview");
        let out = ok_call(&s, "overview", json!({})).await;
        assert_eq!(out["engines"].as_array().unwrap().len(), s.engines.engines().len());
    }

    #[tokio::test]
    async fn find_torrents_pages_and_filters() {
        let s = st("mcp-find");
        for n in ["alpha", "bravo", "charlie"] {
            add(&s, n);
        }
        let out = ok_call(&s, "find_torrents", json!({"limit": 2})).await;
        assert_eq!(out["filtered"], 3);
        assert_eq!(out["returned"], 2);
        assert_eq!(out["next_offset"], 2);
        let out = ok_call(&s, "find_torrents", json!({"offset": 2, "limit": 2})).await;
        assert_eq!(out["returned"], 1);
        assert!(out.get("next_offset").is_none());
        let out = ok_call(&s, "find_torrents", json!({"search": "bravo"})).await;
        assert_eq!(out["filtered"], 1);
        assert_eq!(out["rows"][0]["name"], "bravo");
        // The limit is clamped, not obeyed.
        let out = ok_call(&s, "find_torrents", json!({"limit": 100000})).await;
        assert_eq!(out["returned"], 3);
    }

    #[tokio::test]
    async fn detail_and_files_of_one_torrent() {
        let s = st("mcp-detail");
        let h = add(&s, "delta");
        let out = ok_call(&s, "torrent_detail", json!({"info_hash": h.to_uppercase()})).await;
        assert_eq!(out["engine"], "hoard");
        assert_eq!(out["torrent"]["info_hash"], h);
        assert_eq!(out["trackers"][0][0], "https://tracker.example/announce");
        let out = ok_call(&s, "torrent_files", json!({"info_hash": h})).await;
        assert_eq!(out["count"], 1);
        assert_eq!(out["total_size"], 16384);
    }

    #[tokio::test]
    async fn bad_arguments_are_a_tool_error_not_a_protocol_error() {
        let s = st("mcp-badargs");
        let r = tool_call(&s, "torrent_detail", json!({"info_hash": "xyz"})).await;
        assert_eq!(r["isError"], true);
        let r = tool_call(&s, "find_torrents", json!({"engine": "nope"})).await;
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("unknown engine"));
        let r = tool_call(&s, "find_torrents", json!({"order": "sideways"})).await;
        assert_eq!(r["isError"], true);
    }

    #[tokio::test]
    async fn the_small_reads_answer() {
        let s = st("mcp-reads");
        for (tool, key) in [("trackers", "trackers"), ("categories", "categories"), ("jobs", "jobs"),
                            ("logs", "entries"), ("drain", "status")] {
            let out = ok_call(&s, tool, json!({})).await;
            assert!(out.get(key).is_some(), "{tool} has no `{key}`: {out}");
        }
        let _ = ok_call(&s, "health", json!({})).await;
        let _ = ok_call(&s, "tracker_errors", json!({})).await;
    }

    #[test]
    fn log_levels_rank() {
        assert!(level_rank("ERROR") > level_rank("warn"));
        assert!(level_rank("WARN") > level_rank("INFO"));
        assert_eq!(level_rank("nonsense"), 0);
    }

    // --- writes -----------------------------------------------------------

    #[tokio::test]
    async fn hashes_are_all_checked_before_anything_is_done() {
        let s = st("mcp-hashes");
        let h = add(&s, "echo");
        let r = tool_call(&s, "pause", json!({"info_hashes": [h, "not-a-hash"]})).await;
        assert_eq!(r["isError"], true);
        let r = tool_call(&s, "pause", json!({"info_hashes": []})).await;
        assert_eq!(r["isError"], true);
        let too_many: Vec<String> = (0..=MAX_PER_TORRENT).map(|i| format!("{i:040x}")).collect();
        let r = tool_call(&s, "reannounce", json!({"info_hashes": too_many})).await;
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("at most"));
    }

    #[tokio::test]
    async fn pause_then_resume() {
        let s = st("mcp-pause");
        let h = add(&s, "foxtrot");
        let unknown = "0".repeat(40);
        let out = ok_call(&s, "resume", json!({"info_hashes": [h, unknown]})).await;
        assert_eq!(out["action"], "start");
        assert_eq!(out["engines"][0]["engine"], "hoard");
        assert_eq!(out["engines"][0]["ok"], true, "{out}");
        assert_eq!(out["engines"][0]["answer"]["matched"], 1);
        assert_eq!(out["not_found"][0], unknown);
        let out = ok_call(&s, "pause", json!({"info_hashes": [h]})).await;
        assert_eq!(out["action"], "stop");
        assert_eq!(out["engines"][0]["answer"]["applied"], 1, "{out}");
    }

    #[tokio::test]
    async fn category_must_exist_and_tags_replace() {
        let s = state_from(
            "mcp-labels",
            &format!("[daemon]\napi_key = \"{KEY}\"\n"),
        );
        let h = add(&s, "golf");
        let r = tool_call(&s, "set_category", json!({"info_hashes": [h], "category": "no-such-category"})).await;
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("unknown category"));
        // Clearing is always allowed.
        let out = ok_call(&s, "set_category", json!({"info_hashes": [h], "category": ""})).await;
        assert_eq!(out["ok"], 1, "{out}");
        let out = ok_call(&s, "set_tags", json!({"info_hashes": [h], "tags": ["one", " two ", ""]})).await;
        assert_eq!(out["ok"], 1, "{out}");
        let row = ok_call(&s, "find_torrents", json!({"search": "golf"})).await;
        let tags = row["rows"][0]["tags"].clone();
        assert!(tags.to_string().contains("one") && tags.to_string().contains("two"), "{tags}");
    }

    #[tokio::test]
    async fn add_torrent_needs_a_known_category_and_one_source() {
        let s = st("mcp-add");
        let r = tool_call(&s, "add_torrent", json!({"torrent_path": "/nonexistent.torrent"})).await;
        assert_eq!(r["isError"], true, "category is required");
        let r = tool_call(&s, "add_torrent", json!({"category": "nope", "torrent_path": "/x"})).await;
        assert_eq!(r["isError"], true);
        let r = tool_call(&s, "add_torrent", json!({"category": "x", "torrent_url": "ftp://x"})).await;
        assert_eq!(r["isError"], true);
    }

    #[tokio::test]
    async fn per_torrent_writes_report_each_hash() {
        let s = st("mcp-each");
        let h = add(&s, "hotel");
        let unknown = "1".repeat(40);
        let out = ok_call(&s, "recheck", json!({"info_hashes": [h, unknown]})).await;
        assert_eq!(out["results"].as_array().unwrap().len(), 2);
        assert_eq!(out["results"][1]["ok"], false);
        assert_eq!(out["results"][1]["status"], 404);
        let out = ok_call(&s, "reannounce", json!({"info_hashes": [unknown]})).await;
        assert_eq!(out["failed"], 1);
    }

    #[tokio::test]
    async fn delete_with_the_switch_keeps_files_unless_asked() {
        let s = st_destructive("mcp-delete");
        let h = add(&s, "india");
        let out = ok_call(&s, "delete_torrents", json!({"info_hashes": [h]})).await;
        assert_eq!(out["delete_files"], false);
        assert_eq!(out["ok"], 1, "{out}");
        assert!(api::find_torrent(&s.state, &h).is_none(), "still there after delete");
    }


    // --- moving data with a category ---------------------------------------

    /// A torrent whose one file really exists on disk, under `root`.
    fn add_on_disk(s: &TestState, root: &std::path::Path, name: &str) -> String {
        std::fs::create_dir_all(root).unwrap();
        std::fs::write(root.join(name), vec![0x11u8; 16384]).unwrap();
        let (hash, _) = api::add_torrent_bytes(
            &s.state, &torrent_bytes(name), "", &root.to_string_lossy(), "", true, true, "hoard",
        )
        .unwrap_or_else(|e| panic!("add {name}: {e}"));
        hash
    }

    async fn make_category(s: &TestState, name: &str, path: &std::path::Path) {
        let (st, v) = internal(&s.state, Method::POST, "/api/categories",
            Some(json!({"name": name, "save_path": path.to_string_lossy(), "mode": "hoard"}))).await;
        assert!(st.is_success(), "category {name}: {st} {v}");
    }

    async fn set_cat(s: &TestState, hash: &str, body: Value) -> (StatusCode, Value) {
        internal(&s.state, Method::POST, &format!("/api/hoard/torrents/{hash}/category"), Some(body)).await
    }

    /// Run the queued job the way the runner would, synchronously.
    fn run_next_job(s: &TestState) -> Result<(), String> {
        let job = s.store.lock().unwrap().claim_next_job().expect("a queued job");
        crate::jobsrun::run_job(&s.state, &job)
    }

    fn root_of(s: &TestState, hash: &str) -> std::path::PathBuf {
        let (_, t) = api::find_torrent(&s.state, hash).expect("still in an engine");
        let r = t.save_path.read().clone();
        r
    }

    #[tokio::test]
    async fn without_move_files_it_is_a_label_and_nothing_moves() {
        let s = st("mv-label");
        let src = s.dir.join("src");
        let h = add_on_disk(&s, &src, "label.bin");
        make_category(&s, "books", &s.dir.join("dst")).await;
        let (st_, v) = set_cat(&s, &h, json!({"category": "books"})).await;
        assert_eq!(st_, StatusCode::OK, "{v}");
        assert_eq!(v["moved"], false);
        assert!(src.join("label.bin").exists());
        assert_eq!(root_of(&s, &h), src);
    }

    /// The bug this exists for: "Move to category" answered 200 and moved
    /// nothing. Same filesystem here, so the move is a rename.
    #[tokio::test]
    async fn move_files_moves_the_data_and_keeps_the_torrent() {
        let s = st("mv-rename");
        let (src, dst) = (s.dir.join("src"), s.dir.join("dst"));
        let h = add_on_disk(&s, &src, "move.bin");
        make_category(&s, "books", &dst).await;

        let (st_, v) = internal(&s.state, Method::GET,
            &format!("/api/hoard/torrents/{h}/move-preview?category=books"), None).await;
        assert_eq!(st_, StatusCode::OK, "{v}");
        assert_eq!(v["kind"], "move_data");
        assert_eq!(v["plan"]["rename_files"], 1);
        assert_eq!(v["plan"]["copy_files"], 0);
        assert!(src.join("move.bin").exists(), "a preview must not move anything");

        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::ACCEPTED, "{v}");
        assert_eq!(v["kind"], "move_data");
        // A second request while the first is queued is refused, not doubled.
        let (st2, _) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st2, StatusCode::CONFLICT);

        run_next_job(&s).expect("the move");
        assert!(dst.join("move.bin").exists(), "the file is at the target");
        assert!(!src.join("move.bin").exists(), "and no longer at the source");
        assert_eq!(std::fs::read(dst.join("move.bin")).unwrap().len(), 16384);
        assert_eq!(root_of(&s, &h), dst, "the engine reads it from the new root");
        let (_, t) = api::find_torrent(&s.state, &h).unwrap();
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "a paused torrent stays paused");
        let row = ok_call(&s, "find_torrents", json!({"search": "move"})).await;
        assert_eq!(row["rows"][0]["category"], "books");
    }

    #[tokio::test]
    async fn data_already_in_place_is_a_relabel() {
        let s = st("mv-noop");
        let dst = s.dir.join("dst");
        let h = add_on_disk(&s, &dst, "here.bin");
        make_category(&s, "books", &dst).await;
        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::OK, "{v}");
        assert!(dst.join("here.bin").exists());
    }

    #[tokio::test]
    async fn unknown_category_is_refused_before_anything() {
        let s = st("mv-unknown");
        let src = s.dir.join("src");
        let h = add_on_disk(&s, &src, "stay.bin");
        let (st_, _) = set_cat(&s, &h, json!({"category": "nope", "move_files": true})).await;
        assert_eq!(st_, StatusCode::BAD_REQUEST);
        assert!(src.join("stay.bin").exists());
    }

    /// A rename that cannot happen puts everything back: the file where it
    /// was, the torrent re-added at the old root.
    #[tokio::test]
    async fn a_failed_move_rolls_back() {
        let s = st("mv-rollback");
        let src = s.dir.join("src");
        let h = add_on_disk(&s, &src, "back.bin");
        // The target's parent is a regular FILE: creating the directory fails.
        let blocker = s.dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        make_category(&s, "blocked", &blocker.join("inside")).await;
        let (st_, v) = set_cat(&s, &h, json!({"category": "blocked", "move_files": true})).await;
        assert_eq!(st_, StatusCode::ACCEPTED, "{v}");
        let err = run_next_job(&s).expect_err("the move cannot succeed");
        assert!(err.contains("rolled back"), "{err}");
        assert!(src.join("back.bin").exists(), "the file is where it was");
        assert_eq!(root_of(&s, &h), src, "the torrent is back at the old root");
    }

    /// Across filesystems a hardlinked file is copied, which breaks the link:
    /// asked first (409), done only with consent. Needs /dev/shm to be another
    /// filesystem than the temp dir, which it is in the build container; where
    /// it is not, there is no cross-filesystem move to test.
    #[tokio::test]
    async fn hardlinks_are_asked_before_a_copy_breaks_them() {
        let s = st("mv-hardlink");
        let shm = std::path::Path::new("/dev/shm");
        let src = shm.join(format!("hydra-mv-hl-{}", std::process::id()));
        let dst = s.dir.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        if !shm.exists() || crate::jobs::same_filesystem(shm, &dst) {
            eprintln!("skipped: /dev/shm is not a separate filesystem here");
            return;
        }
        let h = add_on_disk(&s, &src, "linked.bin");
        std::fs::hard_link(src.join("linked.bin"), src.join("library-copy.bin")).unwrap();
        make_category(&s, "books", &dst).await;

        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["reason"], "hardlinks");
        assert_eq!(v["hardlinked_files"], 1);

        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true,
                                              "allow_breaking_hardlinks": true})).await;
        assert_eq!(st_, StatusCode::ACCEPTED, "{v}");
        assert_eq!(v["plan"]["copy_files"], 1);
        run_next_job(&s).expect("the copy");
        assert!(dst.join("linked.bin").exists());
        assert!(!src.join("linked.bin").exists(), "the torrent's name is gone from the source");
        assert!(src.join("library-copy.bin").exists(), "the other name keeps its bytes");
        assert_eq!(root_of(&s, &h), dst);
        std::fs::remove_dir_all(&src).ok();
    }

    #[tokio::test]
    async fn move_to_category_through_mcp() {
        let s = st("mv-mcp");
        let (src, dst) = (s.dir.join("src"), s.dir.join("dst"));
        let h = add_on_disk(&s, &src, "agent.bin");
        make_category(&s, "books", &dst).await;
        let out = ok_call(&s, "move_to_category", json!({"info_hashes": [h], "category": "books"})).await;
        assert_eq!(out["ok"], 1, "{out}");
        assert_eq!(out["results"][0]["status"], 202);
        run_next_job(&s).expect("the move");
        assert!(dst.join("agent.bin").exists());
        let r = tool_call(&s, "move_to_category", json!({"info_hashes": [h], "category": "nope"})).await;
        assert_eq!(r["isError"], true);
    }


    // --- moves: what is NOT this torrent's stays put ------------------------

    /// A torrent from a list of (path components, length). Lengths and piece
    /// count computed, as in `torrent_bytes`; `salt` varies the info hash.
    fn multi_torrent(name: &str, files: &[(&[&str], usize)], salt: u8) -> Vec<u8> {
        let mut info = b"d5:filesl".to_vec();
        let mut total = 0usize;
        for (parts, len) in files {
            info.extend_from_slice(format!("d6:lengthi{len}e4:pathl").as_bytes());
            for p in *parts {
                info.extend_from_slice(format!("{}:{p}", p.len()).as_bytes());
            }
            info.extend_from_slice(b"ee");
            total += len;
        }
        info.extend_from_slice(format!("e4:name{}:{name}12:piece lengthi16384e", name.len()).as_bytes());
        let pieces = total.div_ceil(16384).max(1);
        info.extend_from_slice(format!("6:pieces{}:", pieces * 20).as_bytes());
        info.extend(std::iter::repeat(salt).take(pieces * 20));
        info.push(b'e');
        let announce = "https://tracker.example/announce";
        let mut out = format!("d8:announce{}:{announce}4:info", announce.len()).into_bytes();
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    fn add_bytes(s: &TestState, bytes: &[u8], root: &std::path::Path) -> String {
        api::add_torrent_bytes(&s.state, bytes, "", &root.to_string_lossy(), "", true, true, "hoard")
            .unwrap_or_else(|e| panic!("add: {e}"))
            .0
    }

    /// No sub-folder: the torrent's one file sits in a shared category folder
    /// next to other people's files. Only that file moves; the folder and
    /// everything else in it are left exactly as they were.
    #[tokio::test]
    async fn a_file_at_the_category_root_moves_alone() {
        let s = st("mv-flat");
        let (src, dst) = (s.dir.join("animes"), s.dir.join("dst"));
        let h = add_on_disk(&s, &src, "mine.mkv");
        std::fs::write(src.join("neighbour.mkv"), b"not mine").unwrap();
        std::fs::create_dir_all(src.join("other-show")).unwrap();
        std::fs::write(src.join("other-show").join("e01.mkv"), b"not mine either").unwrap();
        make_category(&s, "books", &dst).await;

        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::ACCEPTED, "{v}");
        assert_eq!(v["plan"]["files"], 1, "one file planned, not a folder");
        run_next_job(&s).expect("the move");

        assert!(dst.join("mine.mkv").exists());
        assert!(!src.join("mine.mkv").exists());
        assert_eq!(std::fs::read(src.join("neighbour.mkv")).unwrap(), b"not mine");
        assert!(src.join("other-show").join("e01.mkv").exists());
        assert!(src.is_dir(), "the category folder itself is never removed");
        assert!(!dst.join("neighbour.mkv").exists(), "nothing but the torrent's own file was copied");
    }

    /// A multi-file torrent's folder that also holds a file the torrent does
    /// not name: the torrent's files move, the stranger stays, and the folder
    /// survives because it is not empty. Emptied sub-folders go.
    #[tokio::test]
    async fn a_torrent_folder_keeps_what_is_not_the_torrents() {
        let s = st("mv-multi");
        let (src, dst) = (s.dir.join("src"), s.dir.join("dst"));
        let show = src.join("Show");
        std::fs::create_dir_all(show.join("s1")).unwrap();
        std::fs::write(show.join("e1.bin"), vec![1u8; 100]).unwrap();
        std::fs::write(show.join("s1").join("e2.bin"), vec![2u8; 100]).unwrap();
        std::fs::write(show.join("notes.txt"), b"added by hand").unwrap();
        let h = add_bytes(&s, &multi_torrent("Show", &[(&["e1.bin"], 100), (&["s1", "e2.bin"], 100)], 0x21), &src);
        make_category(&s, "books", &dst).await;

        let (st_, v) = set_cat(&s, &h, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::ACCEPTED, "{v}");
        run_next_job(&s).expect("the move");

        assert!(dst.join("Show").join("e1.bin").exists());
        assert!(dst.join("Show").join("s1").join("e2.bin").exists());
        assert!(!dst.join("Show").join("notes.txt").exists(), "a file the torrent does not name is not copied");
        assert_eq!(std::fs::read(show.join("notes.txt")).unwrap(), b"added by hand");
        assert!(!show.join("s1").exists(), "the emptied sub-folder is removed");
        assert!(src.is_dir(), "the old root is never removed");
    }

    /// Two torrents reading the very same file (not a hardlink: the same
    /// name). Moving one would pull the data out from under the other.
    #[tokio::test]
    async fn a_file_another_torrent_reads_is_not_moved() {
        let s = st("mv-shared");
        let (src, dst) = (s.dir.join("src"), s.dir.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("same.bin"), vec![0u8; 100]).unwrap();
        let a = add_bytes(&s, &torrent_bytes("same.bin"), &src);
        // Same file name and root, different info hash (a re-release, a
        // cross-seed that reuses the data in place).
        let b_bytes = {
            let mut v = torrent_bytes("same.bin");
            let i = v.len() - 3;
            v[i] ^= 0xFF;
            v
        };
        let b = add_bytes(&s, &b_bytes, &src);
        assert_ne!(a, b);
        make_category(&s, "books", &dst).await;

        let (st_, v) = set_cat(&s, &a, json!({"category": "books", "move_files": true})).await;
        assert_eq!(st_, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["reason"], "shared");
        assert_eq!(v["plan"]["shared_with"][0], b);
        // Consent to break hardlinks is not consent to this.
        let (st_, _) = set_cat(&s, &a, json!({"category": "books", "move_files": true,
                                             "allow_breaking_hardlinks": true})).await;
        assert_eq!(st_, StatusCode::CONFLICT);
        assert!(src.join("same.bin").exists());
    }

    /// A metainfo path with `..` points outside the torrent's folder. The
    /// parser refuses it at the door now; before, the engine accepted it, and
    /// a download would have written outside the save path.
    #[tokio::test]
    async fn a_path_that_climbs_out_is_refused_at_add() {
        let s = st("mv-unsafe");
        let src = s.dir.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let bytes = multi_torrent("evil", &[(&["..", "..", "victim.bin"], 10)], 7);
        let err = api::add_torrent_bytes(&s.state, &bytes, "", &src.to_string_lossy(), "", true, true, "hoard")
            .expect_err("a torrent that escapes its folder must not be added");
        assert!(err.contains("unsafe path"), "{err}");
    }

    // --- helpers ----------------------------------------------------------

    #[test]
    fn enc_keeps_a_value_in_one_parameter() {
        assert_eq!(enc("a&b=c d"), "a%26b%3Dc%20d");
        assert_eq!(enc("tracker.example-1_x~"), "tracker.example-1_x~");
        assert_eq!(enc("é"), "%C3%A9");
    }

    #[test]
    fn top_orders_and_cuts() {
        let f = json!({"a": 1, "b": 5, "c": 3});
        assert_eq!(top(&f, 5), json!([{"name": "b", "count": 5}, {"name": "c", "count": 3}, {"name": "a", "count": 1}]));
        assert_eq!(top(&f, 1), json!({"top": [{"name": "b", "count": 5}], "distinct": 3}));
    }

    #[test]
    fn compact_names_the_error_class() {
        let row = json!({"name": "x", "info_hash": "ab", "tracker_error": true,
            "tracker_error_msg": "tracker: Torrent has been deleted.", "facts": null, "injected_peers": 3});
        let c = compact(&row);
        assert_eq!(c["error_class"], "dead");
        assert!(c.get("injected_peers").is_none());
    }
}
