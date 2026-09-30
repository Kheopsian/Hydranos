//! Magnet links: read, resolve in the background, add when the metadata is in.
//!
//! The engine already knows how to resolve one (`typhon_engine::magnet`:
//! peers from the magnet's trackers and the DHT, then BEP 9 for the info
//! dict). What this module adds is everything around it that an operator
//! relies on:
//!
//! - **The request is kept in the store.** Resolution takes seconds to
//!   minutes; a restart in between must not lose "add this, in that category,
//!   paused". The row is the request, and it goes only once the torrent is
//!   added or the operator gives up on it.
//! - **A failure is shown, not dropped.** A magnet nobody seeds finds no peer.
//!   It is retried a few times with a growing pause, then left as `failed`
//!   with the reason, where the UI lists it until someone retries or removes
//!   it.
//! - **The add is the add everything else uses.** Once the info dict arrives
//!   it becomes a `.torrent` -- the dict byte for byte, so the info hash is the
//!   magnet's, with the magnet's trackers -- and goes through
//!   `api::add_torrent_bytes`: placement, dedup, the store row, workflows.

use std::net::SocketAddr;
use std::time::Duration;

use sha1::{Digest, Sha1};

use crate::api::AppState;

/// A magnet link, read.
#[derive(Debug, Clone, PartialEq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: String,
    pub trackers: Vec<String>,
    pub peers: Vec<SocketAddr>,
}

/// How many times a resolution is started before the request is left failed.
pub const MAX_ATTEMPTS: i64 = 4;

/// Wait before attempt `n` (1-based): nothing, then 1, 5 and 15 minutes. A
/// magnet that found no peer at 20:00 may well find one at 20:15; one that
/// found none for half an hour is waiting for an operator.
fn backoff(attempts: i64) -> i64 {
    match attempts {
        0 => 0,
        1 => 60,
        2 => 300,
        _ => 900,
    }
}

fn hex_nibble(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `%XX` and `+`, as a query string encodes them.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h * 16 + l);
                        i += 3;
                        continue;
                    }
                    _ => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 4648 base32, which a `btih` may use instead of hex (32 characters).
fn base32_20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 20];
    let mut acc: u64 = 0;
    let mut bits = 0;
    let mut n = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    (n == 20).then_some(out)
}

fn hex_20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, p) in s.as_bytes().chunks(2).enumerate() {
        out[i] = hex_nibble(p[0])? * 16 + hex_nibble(p[1])?;
    }
    Some(out)
}

/// Read a `magnet:?` link.
///
/// Needs a v1 info hash (`xt=urn:btih:`). A magnet carrying only a v2 one
/// (`urn:btmh:`) is refused by name: resolving by a v2 hash needs the v2
/// metadata exchange, and answering "added" to something that will never
/// resolve would be worse than saying so. A hybrid magnet has both and is fine.
pub fn parse(uri: &str) -> Result<Magnet, String> {
    let uri = uri.trim();
    let query = match uri.get(..8) {
        Some(p) if p.eq_ignore_ascii_case("magnet:?") => &uri[8..],
        _ => return Err("not a magnet link (magnet:?...)".into()),
    };
    let mut info_hash = None;
    let mut v2_only = false;
    let mut m = Magnet { info_hash: [0; 20], name: String::new(), trackers: Vec::new(), peers: Vec::new() };
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // `tr.1`, `xt.2`: numbered repeats, same meaning.
        let key = k.split('.').next().unwrap_or(k).to_ascii_lowercase();
        let v = percent_decode(v);
        match key.as_str() {
            "xt" => {
                let lower = v.to_ascii_lowercase();
                if let Some(h) = lower.strip_prefix("urn:btih:") {
                    let raw = &v[9..];
                    info_hash = hex_20(h).or_else(|| base32_20(raw));
                    if info_hash.is_none() {
                        return Err(format!("{v:?} is not a 40-hex or 32-base32 info hash"));
                    }
                } else if lower.starts_with("urn:btmh:") {
                    v2_only = true;
                }
            }
            "dn" => m.name = v,
            "tr" => {
                if !v.is_empty() && !m.trackers.contains(&v) {
                    m.trackers.push(v);
                }
            }
            "x" if k.eq_ignore_ascii_case("x.pe") => {
                if let Ok(a) = v.parse() {
                    m.peers.push(a);
                }
            }
            _ => {}
        }
    }
    match info_hash {
        Some(h) => m.info_hash = h,
        None if v2_only => {
            return Err("this magnet has only a v2 info hash (btmh); add the .torrent file instead".into())
        }
        None => return Err("the magnet has no info hash (xt=urn:btih:...)".into()),
    }
    Ok(m)
}

/// A `.torrent` around a resolved info dict.
///
/// The dict is written back byte for byte -- re-encoding it could reorder or
/// normalise a key, and the info hash is the SHA-1 of exactly these bytes. The
/// magnet's trackers become one tier each, as libtorrent does with `tr`.
pub fn torrent_from_dict(dict: &[u8], trackers: &[String]) -> Vec<u8> {
    let s = |x: &str| format!("{}:{}", x.len(), x).into_bytes();
    let mut out = b"d".to_vec();
    if let Some(first) = trackers.first() {
        out.extend(s("announce"));
        out.extend(s(first));
        out.extend(s("announce-list"));
        out.push(b'l');
        for t in trackers {
            out.push(b'l');
            out.extend(s(t));
            out.push(b'e');
        }
        out.push(b'e');
    }
    out.extend(s("info"));
    out.extend_from_slice(dict);
    out.push(b'e');
    out
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ask for a magnet to be added. Returns its info hash.
///
/// Everything that can be refused now is refused now -- a link that does not
/// parse, a category with no save path, an engine that does not exist, a
/// torrent already here -- so the only failure left for later is the one
/// nobody can predict: whether the swarm answers.
pub fn request(
    state: &AppState,
    uri: &str,
    category: &str,
    save_path: &str,
    tags: &str,
    paused: bool,
    engine_override: &str,
) -> Result<String, String> {
    let m = parse(uri)?;
    let hash = hex(&m.info_hash);
    let (engine_id, category_path) = crate::api::placement(state, category, engine_override);
    if save_path.is_empty() && category_path.is_empty() {
        return Err(format!("no save path: category {category:?} is unknown and no savepath was given"));
    }
    let engine = state.engines.get(&engine_id).ok_or_else(|| format!("no engine {engine_id}"))?;
    if engine.manager.get(&m.info_hash).is_some() {
        return Err(format!("{hash}: already added"));
    }
    let row = crate::store::MagnetRow {
        info_hash: hash.clone(),
        uri: uri.trim().to_string(),
        name: m.name.clone(),
        engine: engine_id,
        category: category.to_string(),
        save_path: save_path.to_string(),
        tags: tags.to_string(),
        paused,
        added_at: crate::store::now_secs(),
        attempts: 0,
        next_try: 0,
        state: "resolving".into(),
        error: String::new(),
    };
    {
        let store = state.store.lock().map_err(|_| "store lock")?;
        store.put_magnet(&row).map_err(|e| e.to_string())?;
    }
    // Started now rather than at the next tick: a magnet pasted by hand
    // should not sit two seconds doing nothing.
    let _ = kick(state, &row);
    Ok(hash)
}

/// Start a resolution in the engine the request is for.
fn kick(state: &AppState, row: &crate::store::MagnetRow) -> Result<(), String> {
    let m = parse(&row.uri)?;
    let engine = state.engines.get(&row.engine).ok_or_else(|| format!("no engine {}", row.engine))?;
    // The binding the peers are dialled from. An engine that is not on the
    // network has none, and resolving from the default route would show the
    // swarm this host's own address.
    let cfg = engine
        .engine_config
        .get()
        .ok_or_else(|| format!("engine {} is not on the network", row.engine))?;
    engine.manager.magnet().start(
        m.info_hash,
        m.trackers,
        m.peers,
        cfg,
        None,
        engine.manager.dht().map(|d| d.handle()),
    );
    Ok(())
}

/// One pass over the waiting requests. Returns how many were added.
pub fn drive(state: &AppState) -> usize {
    use typhon_engine::magnet::JobState;
    let rows = match state.store.lock() {
        Ok(s) => s.magnets().unwrap_or_default(),
        Err(_) => return 0,
    };
    let now = crate::store::now_secs();
    let mut added = 0;
    for mut row in rows.into_iter().filter(|r| r.state == "resolving") {
        let Some(engine) = state.engines.get(&row.engine) else {
            let why = format!("no engine {}", row.engine);
            fail(state, &mut row, &why);
            continue;
        };
        let Some(ih) = crate::store::hex20(&row.info_hash) else { continue };
        match engine.manager.magnet().state_of(&ih) {
            // Nothing running: first try after a restart, or the pause after
            // a failure is over.
            None => {
                if now >= row.next_try {
                    if let Err(e) = kick(state, &row) {
                        // Not on the network YET is the normal state for the
                        // first seconds after a start. Only give up once the
                        // request has waited long enough to know.
                        if now - row.added_at > 600 {
                            fail(state, &mut row, &e);
                        }
                    }
                }
            }
            Some(JobState::Resolving) => {}
            Some(JobState::Failed(e)) => {
                engine.manager.magnet().forget(&ih);
                row.attempts += 1;
                if row.attempts >= MAX_ATTEMPTS {
                    fail(state, &mut row, &e);
                } else {
                    row.next_try = now + backoff(row.attempts);
                    row.error = e;
                    save(state, &row);
                }
            }
            Some(JobState::Done(dict)) => {
                engine.manager.magnet().forget(&ih);
                if complete(state, &mut row, &dict, now) {
                    added += 1;
                }
            }
        }
    }
    added
}

/// The metadata is in: check it, build the .torrent, add it. True if added.
pub(crate) fn complete(state: &AppState, row: &mut crate::store::MagnetRow, dict: &[u8], now: i64) -> bool {
    let Some(ih) = crate::store::hex20(&row.info_hash) else { return false };
    // BEP 9 peers are strangers: the dict is only the torrent if it hashes
    // to the magnet's info hash.
    let got: [u8; 20] = Sha1::digest(dict).into();
    if got != ih {
        row.attempts += 1;
        row.next_try = now + backoff(row.attempts);
        row.error = "a peer sent metadata that does not match the info hash".into();
        if row.attempts >= MAX_ATTEMPTS {
            let e = row.error.clone();
            fail(state, row, &e);
        } else {
            save(state, row);
        }
        return false;
    }
    let trackers = parse(&row.uri).map(|m| m.trackers).unwrap_or_default();
    let bytes = torrent_from_dict(dict, &trackers);
    match crate::api::add_torrent_bytes(
        state, &bytes, &row.category, &row.save_path, &row.tags, row.paused, false, &row.engine,
    ) {
        Ok(_) => {
            tracing::info!(info_hash = %row.info_hash, name = %row.name, "magnet resolved and added");
            forget(state, &row.info_hash);
            true
        }
        Err(e) if e.contains("already added") => {
            forget(state, &row.info_hash);
            false
        }
        Err(e) => {
            fail(state, row, &e);
            false
        }
    }
}

fn save(state: &AppState, row: &crate::store::MagnetRow) {
    if let Ok(s) = state.store.lock() {
        let _ = s.put_magnet(row);
    }
}

fn fail(state: &AppState, row: &mut crate::store::MagnetRow, why: &str) {
    tracing::warn!(info_hash = %row.info_hash, error = %why, "magnet given up");
    row.state = "failed".into();
    row.error = why.to_string();
    save(state, row);
}

/// Drop a request, and whatever resolution is running for it.
pub fn forget(state: &AppState, info_hash: &str) -> bool {
    let row = state.store.lock().ok().and_then(|s| s.magnet(info_hash).ok().flatten());
    if let (Some(row), Some(ih)) = (&row, crate::store::hex20(info_hash)) {
        if let Some(engine) = state.engines.get(&row.engine) {
            engine.manager.magnet().forget(&ih);
        }
    }
    state.store.lock().map(|s| s.delete_magnet(info_hash).unwrap_or(false)).unwrap_or(false)
}

/// Put a failed request back in line, from the first attempt.
pub fn retry(state: &AppState, info_hash: &str) -> bool {
    let Some(mut row) = state.store.lock().ok().and_then(|s| s.magnet(info_hash).ok().flatten()) else {
        return false;
    };
    row.state = "resolving".into();
    row.attempts = 0;
    row.next_try = 0;
    row.error.clear();
    save(state, &row);
    true
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

fn unauthorised() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "Invalid or missing API key"}))).into_response()
}

/// The magnets waiting for metadata, and the ones that gave up.
async fn list(State(state): State<AppState>, RawQuery(q): RawQuery, headers: HeaderMap) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    let rows = state.store.lock().ok().and_then(|s| s.magnets().ok()).unwrap_or_default();
    let out: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "info_hash": r.info_hash,
                "name": if r.name.is_empty() { &r.info_hash } else { &r.name },
                "engine": r.engine,
                "category": r.category,
                "added_at": r.added_at,
                "attempts": r.attempts,
                "next_try": r.next_try,
                "state": r.state,
                "error": r.error,
            })
        })
        .collect();
    Json(serde_json::json!({"magnets": out})).into_response()
}

async fn remove(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    if forget(&state, &hash.to_ascii_lowercase()) {
        Json(serde_json::json!({"status": "ok"})).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such magnet"}))).into_response()
    }
}

async fn retry_route(
    State(state): State<AppState>,
    Path(hash): Path<String>,
    RawQuery(q): RawQuery,
    headers: HeaderMap,
) -> Response {
    if !crate::api::authorised(&state, &headers, &q.unwrap_or_default()) {
        return unauthorised();
    }
    if retry(&state, &hash.to_ascii_lowercase()) {
        Json(serde_json::json!({"status": "resolving"})).into_response()
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such magnet"}))).into_response()
    }
}

pub fn routes() -> axum::Router<AppState> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route("/api/magnets", get(list))
        .route("/api/magnets/:hash", delete(remove))
        .route("/api/magnets/:hash/retry", post(retry_route))
}

/// The driver. Every two seconds: cheap when nothing waits (one indexed read
/// of an empty table), and a resolved magnet is added within two seconds.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || drive(&st)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "c9e15763f722f23e98a29decdfae341b98d53056";

    #[test]
    fn a_magnet_gives_its_hash_name_trackers_and_peers() {
        let m = parse(&format!(
            "magnet:?xt=urn:btih:{H}&dn=Some+Book%20%28EPUB%29&tr=udp%3A%2F%2Ft.example%3A1337%2Fannounce\
             &tr.1=https%3A%2F%2Ft2.example%2Fannounce&tr=udp%3A%2F%2Ft.example%3A1337%2Fannounce&x.pe=10.0.0.1:6881"
        ))
        .unwrap();
        assert_eq!(hex(&m.info_hash), H);
        assert_eq!(m.name, "Some Book (EPUB)");
        assert_eq!(m.trackers, vec!["udp://t.example:1337/announce", "https://t2.example/announce"], "deduplicated, in order");
        assert_eq!(m.peers, vec!["10.0.0.1:6881".parse().unwrap()]);
    }

    /// Base32 is the other spelling of the same 20 bytes.
    #[test]
    fn a_base32_info_hash_is_the_same_hash() {
        let b32 = "ZHQVOY7XELZD5GFCTXWN7LRUDOMNKMCW";
        assert_eq!(hex(&parse(&format!("magnet:?xt=urn:btih:{b32}")).unwrap().info_hash), H);
        assert_eq!(
            hex(&parse(&format!("MAGNET:?xt=urn:btih:{}", H.to_uppercase())).unwrap().info_hash),
            H,
            "case does not matter"
        );
    }

    #[test]
    fn what_is_not_a_usable_magnet_is_refused_by_name() {
        assert!(parse("http://example/x.torrent").unwrap_err().contains("not a magnet"));
        assert!(parse("magnet:?dn=nothing").unwrap_err().contains("no info hash"));
        assert!(parse("magnet:?xt=urn:btih:1234").unwrap_err().contains("not a 40-hex"));
        let v2 = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        assert!(parse(v2).unwrap_err().contains("v2"));
        let hybrid = format!("magnet:?xt=urn:btih:{H}&xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e");
        assert_eq!(hex(&parse(&hybrid).unwrap().info_hash), H, "a hybrid resolves by its v1 hash");
    }

    /// ⭐ The rebuilt .torrent has the magnet's info hash: the dict is kept
    /// byte for byte, and the file parses with the trackers as tiers.
    #[test]
    fn the_rebuilt_torrent_has_the_magnets_info_hash() {
        let dict = b"d6:lengthi16384e4:name4:book12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe".to_vec();
        let want: [u8; 20] = Sha1::digest(&dict).into();
        let bytes = torrent_from_dict(&dict, &["udp://t.example:1337/announce".into(), "https://t2.example/a".into()]);
        let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&bytes).expect("parses");
        assert_eq!(meta.info_hash, want);
        assert_eq!(meta.name, "book");
        let no_trackers = torrent_from_dict(&dict, &[]);
        assert_eq!(typhon_engine::torrent::metainfo::parse_torrent_bytes(&no_trackers).unwrap().info_hash, want);
    }

    #[test]
    fn the_pause_between_attempts_grows_and_stops_growing() {
        assert_eq!((backoff(0), backoff(1), backoff(2), backoff(3), backoff(9)), (0, 60, 300, 900, 900));
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::api::testing::{body_json, keyed, state_from, TestState};

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn st(tag: &str) -> TestState {
        state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
    }

    /// An info dict, and the magnet that names it.
    fn dict_and_magnet(name: &str) -> (Vec<u8>, String) {
        let dict = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:BBBBBBBBBBBBBBBBBBBBe", name.len())
            .into_bytes();
        let ih: [u8; 20] = Sha1::digest(&dict).into();
        let uri = format!("magnet:?xt=urn:btih:{}&dn={name}&tr=udp%3A%2F%2Ft.example%3A1337%2Fannounce", hex(&ih));
        (dict, uri)
    }

    fn row(s: &TestState, hash: &str) -> Option<crate::store::MagnetRow> {
        s.state.store.lock().unwrap().magnet(hash).unwrap()
    }

    /// Refused now, whatever can be refused now: a link that is not one, a
    /// request with nowhere to put the data, a torrent already here.
    #[test]
    fn what_can_be_refused_up_front_is() {
        let s = st("mag-refuse");
        assert!(request(&s.state, "not a magnet", "", "/tmp", "", false, "hoard").unwrap_err().contains("not a magnet"));
        let (_, uri) = dict_and_magnet("nowhere");
        assert!(request(&s.state, &uri, "no-such-category", "", "", false, "hoard").unwrap_err().contains("no save path"));
        assert!(s.state.store.lock().unwrap().magnets().unwrap().is_empty(), "nothing kept for a refusal");
    }

    /// ⭐ The whole path once the metadata is in: the rebuilt torrent is
    /// added with what was asked -- engine, save path, tags, paused -- under
    /// the magnet's info hash, with the magnet's tracker, and the request
    /// is gone.
    #[test]
    fn resolved_metadata_is_added_as_asked_and_the_request_goes() {
        let s = st("mag-done");
        let (dict, uri) = dict_and_magnet("resolved-book");
        let hash = request(&s.state, &uri, "", "/tmp", "books,new", true, "hoard").expect("accepted");
        let mut r = row(&s, &hash).expect("kept in the store");
        assert_eq!(r.state, "resolving");

        assert!(complete(&s.state, &mut r, &dict, crate::store::now_secs()));
        assert!(row(&s, &hash).is_none(), "the request is done with");
        let (engine, t) = crate::api::find_torrent(&s.state, &hash).expect("the torrent is here");
        assert_eq!(engine, "hoard");
        assert_eq!(t.meta.name, "resolved-book");
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "added paused, as asked");
        assert_eq!(t.live_trackers.read().concat(), vec!["udp://t.example:1337/announce".to_string()]);
        let tags = s.state.store.lock().unwrap().tags_of(&hash);
        assert_eq!(tags, vec!["books".to_string(), "new".to_string()]);
        // And a second request for it is refused: it is here now.
        assert!(request(&s.state, &uri, "", "/tmp", "", false, "hoard").unwrap_err().contains("already added"));
    }

    /// Metadata that does not hash to the magnet is a stranger's: not added,
    /// counted as a failed attempt, retried later.
    #[test]
    fn metadata_that_does_not_match_is_never_added() {
        let s = st("mag-forged");
        let (_, uri) = dict_and_magnet("genuine");
        let (other, _) = dict_and_magnet("forged");
        let hash = request(&s.state, &uri, "", "/tmp", "", false, "hoard").unwrap();
        let mut r = row(&s, &hash).unwrap();
        let now = crate::store::now_secs();
        assert!(!complete(&s.state, &mut r, &other, now));
        assert!(crate::api::find_torrent(&s.state, &hash).is_none());
        let r = row(&s, &hash).unwrap();
        assert_eq!((r.state.as_str(), r.attempts), ("resolving", 1));
        assert!(r.next_try > now, "and waits before the next try");
        assert!(r.error.contains("does not match"));
    }

    /// An engine that never gets on the network cannot resolve: the request
    /// waits a while (the first seconds after a start are normal), then is
    /// shown as failed with the reason rather than waiting forever.
    #[test]
    fn a_request_no_engine_can_serve_ends_up_failed_with_the_reason() {
        let s = st("mag-offline");
        let (_, uri) = dict_and_magnet("stuck");
        let hash = request(&s.state, &uri, "", "/tmp", "", false, "hoard").unwrap();
        drive(&s.state);
        assert_eq!(row(&s, &hash).unwrap().state, "resolving", "young: still waiting");
        let mut r = row(&s, &hash).unwrap();
        r.added_at -= 3600;
        s.state.store.lock().unwrap().put_magnet(&r).unwrap();
        drive(&s.state);
        let r = row(&s, &hash).unwrap();
        assert_eq!(r.state, "failed");
        assert!(r.error.contains("not on the network"), "{}", r.error);
        assert!(retry(&s.state, &hash));
        assert_eq!(row(&s, &hash).unwrap().state, "resolving");
        assert!(forget(&s.state, &hash));
        assert!(row(&s, &hash).is_none());
    }

    /// The native add answers 202 for a magnet and lists it; the routes
    /// remove it.
    #[tokio::test]
    async fn the_api_accepts_a_magnet_lists_it_and_removes_it() {
        let s = st("mag-api");
        let (_, uri) = dict_and_magnet("apibook");
        let body = serde_json::json!({"magnet_uri": uri, "save_path": "/tmp", "engine": "hoard"}).to_string();
        let r = crate::api::post_torrent_add(State(s.state.clone()), RawQuery(None), keyed(KEY), body).await;
        assert_eq!(r.status(), axum::http::StatusCode::ACCEPTED);
        let v = body_json(r).await;
        let hash = v["info_hash"].as_str().unwrap().to_string();
        assert_eq!(v["status"], "resolving");

        let listed = body_json(list(State(s.state.clone()), RawQuery(None), keyed(KEY)).await).await;
        assert_eq!(listed["magnets"][0]["info_hash"], hash);
        assert_eq!(listed["magnets"][0]["name"], "apibook");
        let gone = remove(State(s.state.clone()), Path(hash.clone()), RawQuery(None), keyed(KEY)).await;
        assert!(gone.status().is_success());
        let again = remove(State(s.state.clone()), Path(hash), RawQuery(None), keyed(KEY)).await;
        assert_eq!(again.status(), axum::http::StatusCode::NOT_FOUND);
        assert_eq!(list(State(s.state.clone()), RawQuery(None), axum::http::HeaderMap::new()).await.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
