//! The IP filter's sources, bans and routes. The filter itself -- parsing,
//! lookup, the checks on every connection -- is `typhon_engine::ipfilter`.
//!
//! Two inputs, combined into the one list the engine checks:
//! - **Sources**: block lists, from a file on this node or an http(s) URL, in
//!   PeerGuardian P2P, eMule `.dat` or CIDR form, gzip- or zip-compressed or
//!   not. Reloaded every `refresh_hours`, and at once when the settings change.
//!   Only while the filter is enabled.
//! - **Bans**: addresses or ranges an operator named, kept in the store. Always
//!   applied: someone who banned a peer by hand did not mean "unless the block
//!   lists are off".
//!
//! A source that fails to load keeps its last good copy rather than dropping
//! out: a block list server being down for an hour must not open the filter
//! for an hour.

use std::sync::Mutex;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use typhon_engine::ipfilter::IpFilter;

use crate::api::AppState;

const SETTINGS_KEY: &str = "ip_filter";
/// A block list bigger than this is not a block list.
const MAX_SOURCE_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default = "default_refresh")]
    pub refresh_hours: u64,
}

fn default_refresh() -> u64 {
    24
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: false, sources: Vec::new(), refresh_hours: default_refresh() }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceStatus {
    pub source: String,
    pub ranges: usize,
    pub unreadable: usize,
    pub loaded_at: i64,
    pub error: String,
}

#[derive(Default)]
struct Loaded {
    /// The settings the sources below were loaded for.
    settings: Option<Settings>,
    lists: Vec<(String, IpFilter)>,
    status: Vec<SourceStatus>,
    loaded_at: i64,
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

pub fn settings(state: &AppState) -> Settings {
    state
        .store
        .lock()
        .ok()
        .and_then(|s| s.setting(SETTINGS_KEY).ok().flatten())
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default()
}

/// Bytes of a list, decompressed if they are gzip or zip.
fn decompress(bytes: Vec<u8>) -> Result<String, String> {
    use std::io::Read;
    let raw = if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(MAX_SOURCE_BYTES as u64 * 4)
            .read_to_end(&mut out)
            .map_err(|e| format!("gzip: {e}"))?;
        out
    } else if bytes.starts_with(b"PK\x03\x04") {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("zip: {e}"))?;
        let mut out = Vec::new();
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| format!("zip: {e}"))?;
            if f.is_file() {
                (&mut f).take(MAX_SOURCE_BYTES as u64 * 4).read_to_end(&mut out).map_err(|e| format!("zip: {e}"))?;
                out.push(b'\n');
            }
        }
        out
    } else {
        bytes
    };
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

async fn fetch(source: &str) -> Result<Vec<u8>, String> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .user_agent(typhon_engine::config::user_agent())
            .build()
            .map_err(|e| e.to_string())?;
        let resp = client.get(source).send().await.map_err(|e| format!("download: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("download: HTTP {}", resp.status().as_u16()));
        }
        let b = resp.bytes().await.map_err(|e| format!("download: {e}"))?;
        if b.len() > MAX_SOURCE_BYTES {
            return Err(format!("larger than {} MB", MAX_SOURCE_BYTES / 1024 / 1024));
        }
        Ok(b.to_vec())
    } else {
        let path = source.to_string();
        tokio::task::spawn_blocking(move || {
            let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
            if meta.len() as usize > MAX_SOURCE_BYTES {
                return Err(format!("larger than {} MB", MAX_SOURCE_BYTES / 1024 / 1024));
            }
            std::fs::read(&path).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

/// Load every source again, keeping the last good copy of any that fails.
pub async fn reload(state: &AppState) {
    let s = settings(state);
    let previous: Vec<(String, IpFilter)> = LOADED
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|l| l.lists.clone()))
        .unwrap_or_default();
    let mut lists = Vec::new();
    let mut status = Vec::new();
    if s.enabled {
        for src in &s.sources {
            let src = src.trim().to_string();
            if src.is_empty() {
                continue;
            }
            let parsed = match fetch(&src).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    decompress(bytes).map(|text| IpFilter::parse(&text))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e),
            };
            match parsed {
                Ok((f, st)) => {
                    status.push(SourceStatus {
                        source: src.clone(),
                        ranges: st.ranges,
                        unreadable: st.unreadable,
                        loaded_at: crate::store::now_secs(),
                        error: String::new(),
                    });
                    lists.push((src, f));
                }
                Err(e) => {
                    tracing::warn!(source = %src, error = %e, "ip filter source failed to load");
                    let kept = previous.iter().find(|(p, _)| *p == src).map(|(_, f)| f.clone());
                    status.push(SourceStatus {
                        source: src.clone(),
                        ranges: kept.as_ref().map_or(0, |f| f.len()),
                        error: if kept.is_some() { format!("{e} (kept the last copy)") } else { e },
                        ..Default::default()
                    });
                    if let Some(f) = kept {
                        lists.push((src, f));
                    }
                }
            }
        }
    }
    if let Ok(mut g) = LOADED.lock() {
        *g = Some(Loaded { settings: Some(s), lists, status, loaded_at: crate::store::now_secs() });
    }
    rebuild(state);
}

/// Combine the loaded lists and the bans, and hand the result to the engine.
pub fn rebuild(state: &AppState) {
    let mut parts: Vec<IpFilter> = LOADED
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|l| l.lists.iter().map(|(_, f)| f.clone()).collect()))
        .unwrap_or_default();
    let bans = state.store.lock().ok().and_then(|s| s.bans().ok()).unwrap_or_default();
    let text: String = bans.iter().map(|(ip, _, _)| format!("{ip}\n")).collect();
    parts.push(IpFilter::parse(&text).0);
    let all = IpFilter::union(&parts);
    tracing::info!(ranges = all.len(), bans = bans.len(), "ip filter installed");
    typhon_engine::ipfilter::install(Some(all));
    // The peers already connected: each session re-checks on its next turn,
    // and these are woken so the next turn is now.
    let woken: usize = state.engines.engines().iter().map(|e| e.manager.wake_filtered_peers()).sum();
    if woken > 0 {
        tracing::info!(peers = woken, "ip filter: disconnecting peers it now blocks");
    }
}

/// Reload at start, every `refresh_hours`, and within a minute of a change
/// to the settings.
pub fn spawn(state: AppState) {
    // The bans at once, before a list download that can take a minute. A
    // peer that connected in between is dropped when the full list lands:
    // every install moves the generation the sessions watch.
    rebuild(&state);
    tokio::spawn(async move {
        reload(&state).await;
        let mut last = crate::store::now_secs();
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let s = settings(&state);
            let changed = LOADED
                .lock()
                .ok()
                .map(|g| g.as_ref().and_then(|l| l.settings.clone()) != Some(s.clone()))
                .unwrap_or(true);
            let due = s.enabled && crate::store::now_secs() - last >= s.refresh_hours.max(1) as i64 * 3600;
            if changed || due {
                reload(&state).await;
                last = crate::store::now_secs();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn unauthorised() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid or missing API key"}))).into_response()
}

fn bad(msg: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": msg.to_string()}))).into_response()
}

/// What the filter holds and has done.
pub fn status_json(state: &AppState) -> serde_json::Value {
    let (sources, loaded_at) = LOADED
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|l| (l.status.clone(), l.loaded_at)))
        .unwrap_or_default();
    let bans: Vec<serde_json::Value> = state
        .store
        .lock()
        .ok()
        .and_then(|s| s.bans().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(ip, reason, at)| serde_json::json!({"ip": ip, "reason": reason, "added_at": at}))
        .collect();
    use std::sync::atomic::Ordering::Relaxed;
    serde_json::json!({
        "settings": settings(state),
        "ranges": typhon_engine::ipfilter::installed_len(),
        "sources": sources,
        "loaded_at": loaded_at,
        "bans": bans,
        "blocked_in": typhon_engine::ipfilter::BLOCKED_IN.load(Relaxed),
        "blocked_out": typhon_engine::ipfilter::BLOCKED_OUT.load(Relaxed),
        "dropped": typhon_engine::ipfilter::DROPPED.load(Relaxed),
    })
}

async fn status(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    Json(status_json(&state)).into_response()
}

async fn put_settings(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap, body: String) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    let s: Settings = match serde_json::from_str(&body) {
        Ok(s) => s,
        Err(e) => return bad(e),
    };
    for src in &s.sources {
        let src = src.trim();
        if !(src.starts_with("http://") || src.starts_with("https://") || src.starts_with('/')) {
            return bad(format!("{src:?}: a source is an absolute path on this node or an http(s) URL"));
        }
    }
    let json = serde_json::to_string(&s).unwrap_or_default();
    if let Err(e) = state.store.lock().map_err(|_| "store lock".to_string()).and_then(|st| st.put_setting(SETTINGS_KEY, &json).map_err(|e| e.to_string())) {
        return bad(e);
    }
    // Applied now rather than at the next minute: the operator is looking.
    reload(&state).await;
    Json(status_json(&state)).into_response()
}

#[derive(Deserialize)]
struct BanBody {
    ip: String,
    #[serde(default)]
    reason: String,
}

async fn ban(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap, body: String) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    let b: BanBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => return bad(e),
    };
    let ip = b.ip.trim();
    // One address, one CIDR or one range -- exactly one, or it is a typo.
    let (_, st) = IpFilter::parse(ip);
    if st.ranges != 1 || st.unreadable != 0 {
        return bad(format!("{ip:?} is not an address, a CIDR or a range"));
    }
    if let Err(e) = state.store.lock().map_err(|_| "store lock".to_string()).and_then(|s| s.put_ban(ip, &b.reason).map_err(|e| e.to_string())) {
        return bad(e);
    }
    rebuild(&state);
    Json(serde_json::json!({"status": "ok", "ip": ip})).into_response()
}

async fn unban(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap, body: String) -> Response {
    let query = q.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return unauthorised();
    }
    let b: BanBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => return bad(e),
    };
    let gone = state.store.lock().ok().map(|s| s.delete_ban(b.ip.trim()).unwrap_or(false)).unwrap_or(false);
    if !gone {
        return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such ban"}))).into_response();
    }
    rebuild(&state);
    Json(serde_json::json!({"status": "ok"})).into_response()
}

async fn reload_route(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    reload(&state).await;
    Json(serde_json::json!({"status": "ok", "ranges": typhon_engine::ipfilter::installed_len()})).into_response()
}

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/ipfilter", get(status).put(put_settings))
        .route("/api/ipfilter/bans", post(ban).delete(unban))
        .route("/api/ipfilter/reload", post(reload_route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};
    use std::net::IpAddr;

    const KEY: &str = "0123456789abcdef0123456789abcdef";
    /// The filter is one per process: these tests take turns.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    async fn put(s: &TestState, body: serde_json::Value) -> Response {
        put_settings(State(s.state.clone()), RawQuery(None), keyed(KEY), body.to_string()).await
    }

    /// ⭐ A list from a file, a gzip one beside it, both applied; a ban on
    /// top; the status says what came from where.
    #[tokio::test]
    async fn sources_and_bans_become_one_filter() {
        let _one = SERIAL.lock().await;
        let s = st("ipf-sources");
        let plain = s.dir.join("list.p2p");
        std::fs::write(&plain, "Test range:198.51.100.0-198.51.100.127\n").unwrap();
        let gz = s.dir.join("list.gz");
        {
            use std::io::Write;
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(b"198.51.100.200/30\n").unwrap();
            std::fs::write(&gz, e.finish().unwrap()).unwrap();
        }
        let r = put(&s, serde_json::json!({"enabled": true, "sources": [plain, gz]})).await;
        assert!(r.status().is_success());
        let v = body_json(r).await;
        assert_eq!(v["sources"][0]["ranges"], 1);
        assert_eq!(v["sources"][1]["ranges"], 1, "the gzip one is read too");
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.5")));
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.201")));
        assert!(!typhon_engine::ipfilter::blocked(ip("198.51.100.150")));

        let b = ban(State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"ip":"198.51.100.150","reason":"leecher"}"#.into()).await;
        assert!(b.status().is_success());
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.150")), "a ban applies at once");
        let bad_ban = ban(State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"ip":"not-an-ip"}"#.into()).await;
        assert_eq!(bad_ban.status(), StatusCode::BAD_REQUEST);

        // Off: the lists stop applying, the ban does not.
        put(&s, serde_json::json!({"enabled": false, "sources": [plain]})).await;
        assert!(!typhon_engine::ipfilter::blocked(ip("198.51.100.5")), "lists off");
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.150")), "a ban by hand is not a list");
        let u = unban(State(s.state.clone()), RawQuery(None), keyed(KEY), r#"{"ip":"198.51.100.150"}"#.into()).await;
        assert!(u.status().is_success());
        assert!(!typhon_engine::ipfilter::blocked(ip("198.51.100.150")));
    }

    /// A source that stops loading keeps its last good copy: a list server
    /// down for an hour must not open the filter for an hour.
    #[tokio::test]
    async fn a_source_that_fails_keeps_its_last_copy() {
        let _one = SERIAL.lock().await;
        let s = st("ipf-keep");
        let f = s.dir.join("keep.txt");
        std::fs::write(&f, "198.51.100.77\n").unwrap();
        put(&s, serde_json::json!({"enabled": true, "sources": [f]})).await;
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.77")));
        std::fs::remove_file(&f).unwrap();
        reload(&s.state).await;
        assert!(typhon_engine::ipfilter::blocked(ip("198.51.100.77")), "still filtered");
        let v = status_json(&s.state);
        assert!(v["sources"][0]["error"].as_str().unwrap().contains("kept the last copy"), "{v}");
        put(&s, serde_json::json!({"enabled": false, "sources": []})).await;
    }

    #[tokio::test]
    async fn a_source_must_be_a_path_or_a_url_and_the_routes_need_the_key() {
        let _one = SERIAL.lock().await;
        let s = st("ipf-refuse");
        let r = put(&s, serde_json::json!({"enabled": true, "sources": ["relative/list.txt"]})).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = status(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_zip_list_is_read_like_a_plain_one() {
        use std::io::Write;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            z.start_file("level1.p2p", zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(b"x:10.1.0.0-10.1.0.9\n").unwrap();
            z.finish().unwrap();
        }
        let text = decompress(buf.into_inner()).unwrap();
        assert!(IpFilter::parse(&text).0.contains(ip("10.1.0.3")));
    }
}
