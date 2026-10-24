//! The fleet: other Hydra instances, reached over their own HTTP API.
//!
//! There is no agent protocol here, and that is the design. A node is a whole
//! Hydra, and every capability the fleet needs is a route this build already
//! serves to its own UI. A remote feature therefore cannot rot separately from
//! the local one -- which is exactly what happened to the 42-method gRPC agent
//! surface, where ten handlers ended up answering a plausible error and doing
//! nothing.
//!
//! Two ways in, on purpose:
//!
//!   * `probe` and the aggregating handlers call the remote SERVER-SIDE, so the
//!     remote's key never leaves this process.
//!   * the browser is handed the key only when the operator asks to open a
//!     node's own front, and then it lands in that origin's localStorage --
//!     the same place it would sit had they typed it in by hand.

use std::time::Duration;

/// What a node answered when we last asked.
///
/// `online: false` carries the reason rather than dropping it: a node that is
/// unreachable and a node whose key we have wrong are the same picture in the
/// UI otherwise, and they need opposite fixes.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Health {
    pub online: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub engines: Vec<String>,
    pub torrents: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// A short timeout on purpose: the nodes list is rendered on demand and a dead
/// node must not hold the page. Long enough for a LAN round trip and a
/// catalogue-sized status, short enough that six dead nodes cost six seconds.
const PROBE_TIMEOUT: Duration = Duration::from_secs(4);

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// Ask a node what it is. Never fails: an error IS the answer.
pub async fn probe(url: &str, api_key: &str) -> Health {
    let target = format!("{}/api/status", url.trim_end_matches('/'));
    let res = client()
        .get(&target)
        .header("X-API-Key", api_key)
        .send()
        .await;
    let res = match res {
        Ok(r) => r,
        Err(e) => {
            return Health {
                online: false,
                // The reqwest Display carries the URL, and the URL is where a
                // passkey would be on other endpoints. Status has none, but the
                // habit is worth keeping: report the class, not the string.
                error: if e.is_timeout() {
                    "timeout".into()
                } else if e.is_connect() {
                    "connexion refusee".into()
                } else {
                    "injoignable".into()
                },
                ..Default::default()
            };
        }
    };
    if res.status() == reqwest::StatusCode::UNAUTHORIZED
        || res.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Health {
            online: false,
            error: "cle API refusee".into(),
            ..Default::default()
        };
    }
    if !res.status().is_success() {
        return Health {
            online: false,
            error: format!("HTTP {}", res.status().as_u16()),
            ..Default::default()
        };
    }
    let body: serde_json::Value = match res.json().await {
        Ok(v) => v,
        Err(_) => {
            // A 200 that is not our JSON means something else is on that port.
            return Health {
                online: false,
                error: "reponse non-Hydra".into(),
                ..Default::default()
            };
        }
    };

    // The engine list comes from /api/engines, which names every engine the
    // node hosts. /api/status only ever carries `race` and `hoard` -- its shape
    // is frozen for 3.x compatibility -- so a third engine is invisible there.
    let mut engines: Vec<String> = Vec::new();
    if let Ok(r) = client()
        .get(format!("{}/api/engines", url.trim_end_matches('/')))
        .header("X-API-Key", api_key)
        .send()
        .await
    {
        if let Ok(list) = r.json::<serde_json::Value>().await {
            if let Some(arr) = list.as_array() {
                engines = arr
                    .iter()
                    .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
                    .collect();
            }
        }
    }

    // Totals still come from status, and so does the engine list when the node
    // is old enough to answer `[]` there: falling back keeps a 4.12 node
    // legible instead of reporting it as hosting nothing.
    let mut torrents = 0i64;
    let mut shape_engines = Vec::new();
    if let Some(map) = body.as_object() {
        for (k, v) in map {
            let Some(obj) = v.as_object() else { continue };
            // An engine section is one that counts torrents. Discovered rather
            // than hardcoded to race and hoard: a node may host neither, or six.
            let n = obj
                .get("total_torrents")
                .or_else(|| obj.get("torrents"))
                .and_then(|x| x.as_i64());
            if let Some(n) = n {
                shape_engines.push(k.clone());
                torrents += n;
            }
        }
    }
    if engines.is_empty() {
        engines = shape_engines;
    }
    engines.sort();

    Health {
        online: true,
        version: body
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        engines,
        torrents,
        error: String::new(),
    }
}

/// Forward one request to a node, injecting its key.
///
/// Used for aggregation, where the answer is consumed by this process. It is
/// NOT how the operator browses a remote UI: the front asks for absolute paths
/// (`/static/app.js`, `/api/hoard/page`), so serving it under a `/node/<name>/`
/// prefix would send those to the wrong Hydra. Opening a node's own origin is
/// handled by `open`, below.
pub async fn forward(
    url: &str,
    api_key: &str,
    method: reqwest::Method,
    path_and_query: &str,
    body: Vec<u8>,
) -> Result<(reqwest::StatusCode, Vec<u8>, String), String> {
    let target = format!(
        "{}/{}",
        url.trim_end_matches('/'),
        path_and_query.trim_start_matches('/')
    );
    let mut req = client()
        .request(method, &target)
        .header("X-API-Key", api_key);
    if !body.is_empty() {
        // Declared as JSON: a relayed body that arrives without a content type
        // is parsed as nothing and the far side sees an empty request.
        req = req
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    let res = req.send().await.map_err(|e| {
        if e.is_timeout() {
            "timeout".to_string()
        } else {
            "injoignable".to_string()
        }
    })?;
    let status = res.status();
    let ctype = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = res
        .bytes()
        .await
        .map_err(|_| "corps illisible".to_string())?;
    Ok((status, bytes.to_vec(), ctype))
}

#[cfg(test)]
mod tests {
    /// The engine sections are found by shape, not by name.
    ///
    /// `/api/status` puts race and hoard beside `baseline`, `day_uploaded` and
    /// friends. Picking sections that count torrents is what lets a node with a
    /// `vpn1` engine report it without this file knowing the name -- the very
    /// case the old code got wrong by matching on "race" and "hoard".
    #[test]
    fn engine_sections_are_recognised_by_shape() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"version":"4.12.1",
                "day_uploaded":123,
                "baseline":{"global_uploaded":5},
                "hoard":{"total_torrents":300000,"running":true},
                "race":{"torrents":486},
                "vpn1":{"total_torrents":12}}"#,
        )
        .unwrap();

        let mut engines = Vec::new();
        let mut torrents = 0i64;
        for (k, v) in body.as_object().unwrap() {
            let Some(obj) = v.as_object() else { continue };
            if let Some(n) = obj
                .get("total_torrents")
                .or_else(|| obj.get("torrents"))
                .and_then(|x| x.as_i64())
            {
                engines.push(k.clone());
                torrents += n;
            }
        }
        engines.sort();
        assert_eq!(engines, vec!["hoard", "race", "vpn1"]);
        assert_eq!(torrents, 300_498);
        // `baseline` counts bytes, not torrents, and must not be an engine.
        assert!(!engines.contains(&"baseline".to_string()));
    }
}

/// Hand a torrent to another node, and let BitTorrent move the bytes.
///
/// The metainfo goes over HTTP because it must arrive before anything else can
/// happen; the DATA does not. The receiving node is told the sender holds it,
/// and pulls it over the protocol both ends already speak -- parallel,
/// resumable, throttled by the same knobs as any other transfer, and
/// hash-checked piece by piece because that is what BitTorrent does.
///
/// The alternative, which 3.x used, was `read_piece` on one side and
/// `write_piece` on the other: every byte relayed through the control plane,
/// twice over the wire, with a correctness argument to make from scratch.
///
/// `from` is supplied by the caller and not inferred. This node cannot know
/// which of its addresses the target can reach -- it may sit behind a tunnel,
/// a NAT, or several interfaces with different fates -- and guessing would
/// produce a handoff that transfers nothing while reporting success.
pub async fn handoff(
    url: &str,
    api_key: &str,
    info_hash: &str,
    torrent: Vec<u8>,
    from: &str,
    category: &str,
    engine: &str,
) -> Result<serde_json::Value, String> {
    let base = url.trim_end_matches('/');

    // 0. Does the far side know this category?
    //
    // It routes incoming torrents BY category, and one it does not know is not
    // a detail: the shim answers a bare `HTTP 400`, which reaches the operator
    // as "the node refused the torrent" with nothing to act on. Worse, on some
    // paths an unknown category lands the torrent in RACE, so a hoard torrent
    // would quietly change tier on arrival.
    //
    // Checked here so the refusal can name the category and list the ones that
    // would work. Creating it on the far side would need its save path, which
    // is that node's decision, not this one's.
    if !category.is_empty() {
        if let Ok(r) = client()
            .get(format!("{base}/api/categories"))
            .header("X-API-Key", api_key)
            .send()
            .await
        {
            if let Ok(list) = r.json::<serde_json::Value>().await {
                if let Some(arr) = list.as_array() {
                    let names: Vec<&str> = arr
                        .iter()
                        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                        .collect();
                    if !names.is_empty() && !names.contains(&category) {
                        return Err(format!(
                            "the node has no category {category}. Create it there, or send with one of: {}",
                            names.join(", ")
                        ));
                    }
                }
            }
        }
    }

    // 1. The metainfo. Through the qBit shim: the native upload route is one of
    //    the handlers the port left refusing everything.
    let part = reqwest::multipart::Part::bytes(torrent)
        .file_name(format!("{info_hash}.torrent"))
        .mime_str("application/x-bittorrent")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::multipart::Form::new().part("torrents", part);
    if !category.is_empty() {
        form = form.text("category", category.to_string());
    }
    // A named engine only travels on the NATIVE route: the qBit shim places by
    // category, and a category carries a mode -- "hoard" or "race" -- which
    // cannot name the third engine of a multi-tunnel node.
    if !engine.is_empty() {
        form = form.text("engine", engine.to_string());
    }
    let res = client()
        .post(format!("{base}/api/torrents/upload"))
        .header("X-API-Key", api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|_| "the node did not accept the metainfo".to_string())?;
    if !res.status().is_success() {
        return Err(format!(
            "the node refused the torrent: HTTP {}",
            res.status().as_u16()
        ));
    }

    // 2. Where to fetch it from. Without this the torrent sits there knowing
    //    nobody, since nothing else will tell it about us.
    let res = client()
        .post(format!("{base}/api/torrents/{info_hash}/peers"))
        .header("X-API-Key", api_key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({ "peers": [from] }).to_string())
        .send()
        .await
        .map_err(|_| "the node took the torrent but not the peer".to_string())?;
    let queued: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);

    Ok(serde_json::json!({
        "status": "ok",
        "info_hash": info_hash,
        "from": from,
        "peer": queued,
    }))
}

/// How long a move to a node waits for the far side to hold a full copy.
///
/// Six hours: long enough for a large payload over a home link, short enough
/// that a forgotten wait does not outlive the reason for it. Counted from the
/// handoff and stored with the job, so a restart does not start it over.
pub const HANDOFF_WAIT_SECS: i64 = 6 * 3600;

/// Ask a node, once, how far it is with a torrent.
///
/// `Ok(Some(progress))` (0.0..=1.0) when one of its engines lists the hash,
/// `Ok(None)` when none does yet, `Err` when the node did not answer. Used by
/// a MOVE: the target has the metainfo long before it has the bytes, so
/// nothing local may be deleted until the far side holds a full copy.
///
/// Asked per engine through `/api/engines/<id>/page`. 4.3 polled
/// `/api/<engine>/page`, a route that exists for `race` and `hoard` only, so a
/// move to a node's third engine (`vpn1`) was never confirmed and the local
/// copy stayed forever. With no engine named, every engine of the node is
/// asked: the far side's category decided where the torrent went, and the
/// hoard-only default missed every handoff it routed to race.
pub async fn remote_progress(
    url: &str,
    api_key: &str,
    info_hash: &str,
    engine: &str,
) -> Result<Option<f64>, String> {
    let base = url.trim_end_matches('/');
    let engines: Vec<String> = if engine.is_empty() {
        let list: serde_json::Value = client()
            .get(format!("{base}/api/engines"))
            .header("X-API-Key", api_key)
            .send()
            .await
            .map_err(|_| "the node is unreachable".to_string())?
            .json()
            .await
            .map_err(|_| "the node's engine list did not parse".to_string())?;
        list.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        vec![engine.to_string()]
    };
    let mut best: Option<f64> = None;
    let mut answered = false;
    for e in &engines {
        let res = client()
            .get(format!(
                "{base}/api/engines/{e}/page?limit=1&search={info_hash}"
            ))
            .header("X-API-Key", api_key)
            .send()
            .await;
        let Ok(res) = res else { continue };
        if !res.status().is_success() {
            continue;
        }
        let Ok(body) = res.json::<serde_json::Value>().await else {
            continue;
        };
        answered = true;
        // The search also matches names, so the row must carry OUR hash: a
        // torrent whose name happens to contain these hex digits is not ours.
        let row = body.get("rows").and_then(|r| r.as_array()).and_then(|a| {
            a.iter().find(|r| {
                r.get("info_hash")
                    .and_then(|h| h.as_str())
                    .map(|h| h.eq_ignore_ascii_case(info_hash))
                    .unwrap_or(false)
            })
        });
        if let Some(row) = row {
            let p = row.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0);
            best = Some(best.map_or(p, |b: f64| b.max(p)));
        }
    }
    if !answered {
        return Err("the node did not answer for any engine".into());
    }
    Ok(best)
}

// ---------------------------------------------------------------------------
// Key rotation
// ---------------------------------------------------------------------------
//
// Two steps, so that no moment exists where neither side holds a working key.
// The controller asks the node for a NEW key, authenticated with the old one;
// the node generates it (the controller never chooses another machine's
// secret) and keeps it PENDING, in memory, beside the old one. The controller
// then confirms by presenting the new key: only that call makes it the node's
// key, on disk and live, and from then on the old one is refused.
//
// A controller that dies between the two leaves the node on its old key, the
// pending one forgotten after `PENDING_KEY_TTL` or a restart, and the
// controller's stored key still valid. Nothing to repair.

/// How long a pending key waits for its confirmation.
pub const PENDING_KEY_TTL: Duration = Duration::from_secs(600);

static PENDING_KEY: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// The pending key is process-wide, so a test that mints one (here, or through
/// the rotation routes) holds this for its whole length.
#[cfg(test)]
pub(crate) static PENDING_KEY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Node side: mint a pending key, replacing any earlier one.
pub fn begin_local_rotation() -> String {
    let key = crate::config::fresh_api_key();
    *PENDING_KEY.lock().unwrap_or_else(|p| p.into_inner()) =
        Some((key.clone(), std::time::Instant::now()));
    key
}

/// Node side: is `presented` the pending key, still in time? Consumes it when
/// it is, so a confirmation cannot be replayed.
pub fn take_pending_key(presented: &str) -> Option<String> {
    let mut g = PENDING_KEY.lock().unwrap_or_else(|p| p.into_inner());
    let (key, at) = g.as_ref()?;
    if at.elapsed() > PENDING_KEY_TTL {
        *g = None;
        return None;
    }
    if presented.is_empty() || !crate::api::constant_time_eq(presented.as_bytes(), key.as_bytes()) {
        return None;
    }
    g.take().map(|(k, _)| k)
}

/// Controller side: rotate a node's key. Returns the new key, already the
/// only one the node accepts.
pub async fn rotate_key(url: &str, old_key: &str) -> Result<String, String> {
    let base = url.trim_end_matches('/');
    let res = client()
        .post(format!("{base}/api/auth/api-key/rotate"))
        .header("X-API-Key", old_key)
        .send()
        .await
        .map_err(|_| "the node is unreachable".to_string())?;
    match res.status().as_u16() {
        200 => {}
        401 | 403 => {
            return Err("the node refused the current key: declare it again with its key".into())
        }
        404 | 405 => return Err("the node is too old to rotate its key (needs 4.4)".into()),
        s => return Err(format!("the node refused the rotation: HTTP {s}")),
    }
    let v: serde_json::Value = res
        .json()
        .await
        .map_err(|_| "the node's answer did not parse".to_string())?;
    let new_key = v
        .get("pending_key")
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    if new_key.len() < 16 {
        return Err("the node answered no usable key".into());
    }
    let res = client()
        .post(format!("{base}/api/auth/api-key/confirm"))
        .header("X-API-Key", &new_key)
        .send()
        .await
        .map_err(|_| {
            "the node stopped answering before the confirmation; it keeps its old key".to_string()
        })?;
    if !res.status().is_success() {
        return Err(format!(
            "the node did not confirm the new key (HTTP {}); it keeps its old key",
            res.status().as_u16()
        ));
    }
    Ok(new_key)
}

/// Fetch a node's copy of a .torrent, and the port the engine holding it
/// listens on.
///
/// The mirror image of `handoff`, and the easier direction: to PULL we already
/// know where the other side is, because its address is the node URL. Pushing
/// had to hand the target `auto:<port>` and let it work out our address.
pub async fn fetch_metainfo(
    url: &str,
    api_key: &str,
    info_hash: &str,
    from_engine: &str,
) -> Result<(Vec<u8>, u16), String> {
    let base = url.trim_end_matches('/');

    let res = client()
        .get(format!("{base}/api/torrents/{info_hash}/torrent"))
        .header("X-API-Key", api_key)
        .send()
        .await
        .map_err(|_| "the node is unreachable".to_string())?;
    if !res.status().is_success() {
        return Err(format!(
            "the node has no .torrent for {}: HTTP {}",
            &info_hash[..8.min(info_hash.len())],
            res.status().as_u16()
        ));
    }
    let blob = res
        .bytes()
        .await
        .map_err(|_| "the .torrent could not be read".to_string())?
        .to_vec();

    // Which port to dial. Read from the node rather than assumed: an engine on
    // its own tunnel listens where its own session says, and guessing 16372
    // would point at a different engine or at nothing.
    let res = client()
        .get(format!("{base}/api/engines"))
        .header("X-API-Key", api_key)
        .send()
        .await
        .map_err(|_| "the node stopped answering".to_string())?;
    let list: serde_json::Value = res
        .json()
        .await
        .map_err(|_| "the node's engine list did not parse".to_string())?;
    let port = list
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(from_engine))
                .and_then(|e| e.get("listen_port").and_then(|v| v.as_u64()))
        })
        .ok_or_else(|| format!("the node has no engine named {from_engine}"))?;
    if port == 0 {
        return Err(format!("{from_engine} has no listening port on that node"));
    }
    Ok((blob, port as u16))
}

#[cfg(test)]
mod relay_tests {
    use super::*;
    use axum::routing::{get, post};
    use axum::Router;

    /// A throwaway Hydra on loopback. Everything `nodes.rs` does is HTTP, so
    /// the honest fixture is a real server on a real port -- bound to :0 so
    /// tests never collide, and shut down with the test.
    struct FakeNode {
        url: String,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    async fn fake_node(app: Router) -> FakeNode {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        FakeNode {
            url: format!("http://{addr}"),
            _shutdown: tx,
        }
    }

    /// A node that answers a proper status.
    fn healthy() -> Router {
        Router::new().route(
            "/api/status",
            get(|| async {
                axum::Json(serde_json::json!({
                    "version": "4.27.0",
                    "engines": [{"id": "race"}, {"id": "hoard"}],
                    "hoard": {"total_torrents": 293194},
                    "race": {"torrents": 475}
                }))
            }),
        )
    }

    /// ⭐ A probe NEVER fails: an error IS the answer. The nodes list is
    /// rendered on demand, and a node that is down must render as down rather
    /// than take the page with it.
    #[tokio::test]
    async fn a_node_that_is_not_there_answers_offline_rather_than_erroring() {
        // Port 1 on loopback: nothing listens, and the connection is refused
        // immediately rather than hanging.
        let h = probe("http://127.0.0.1:1", "key").await;
        assert!(!h.online, "an unreachable node is offline");
        assert!(!h.error.is_empty(), "and says why: {h:?}");
    }

    #[tokio::test]
    async fn a_healthy_node_reports_its_version_and_its_torrents() {
        let node = fake_node(healthy()).await;
        let h = probe(&node.url, "key").await;
        assert!(h.online, "got {h:?}");
        assert_eq!(h.version, "4.27.0");
        assert!(h.error.is_empty(), "a healthy node reports no error: {h:?}");
        assert!(h.torrents > 0, "the catalogue size came back: {h:?}");
    }

    /// ⭐ A node that refuses our key is ONLINE but unusable. Reporting it as
    /// offline would send the operator looking at the network when the problem
    /// is the credential.
    #[tokio::test]
    async fn a_node_that_refuses_the_key_is_not_reported_as_offline_without_a_reason() {
        let app = Router::new().route(
            "/api/status",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    axum::Json(serde_json::json!({"error": "Invalid or missing API key"})),
                )
            }),
        );
        let node = fake_node(app).await;
        let h = probe(&node.url, "wrong-key").await;
        assert!(!h.error.is_empty(), "the refusal is reported: {h:?}");
    }

    /// A node answering something that is not JSON must not panic the probe.
    #[tokio::test]
    async fn a_node_answering_garbage_is_an_error_not_a_panic() {
        let app = Router::new().route("/api/status", get(|| async { "this is not json" }));
        let node = fake_node(app).await;
        let h = probe(&node.url, "key").await;
        assert!(!h.error.is_empty() || !h.online, "got {h:?}");
    }

    /// A trailing slash on the declared URL must not produce `//api/status`.
    #[tokio::test]
    async fn a_trailing_slash_in_the_node_url_is_tolerated() {
        let node = fake_node(healthy()).await;
        let h = probe(&format!("{}/", node.url), "key").await;
        assert!(h.online, "a trailing slash must not break the path: {h:?}");
    }

    /// ⭐ The relay carries the remote's key server-side: it never reaches a
    /// browser and never sits in a URL. Here we prove it is actually sent.
    #[tokio::test]
    async fn the_relay_sends_the_nodes_own_key_rather_than_ours() {
        let app = Router::new().route(
            "/api/engines",
            get(|headers: axum::http::HeaderMap| async move {
                let key = headers
                    .get("X-API-Key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                axum::Json(serde_json::json!({"seen_key": key}))
            }),
        );
        let node = fake_node(app).await;
        let (status, body, _ctype) = forward(
            &node.url,
            "the-remote-key",
            reqwest::Method::GET,
            "/api/engines",
            vec![],
        )
        .await
        .expect("the relay reached the node");
        assert!(status.is_success());
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(v["seen_key"], serde_json::json!("the-remote-key"));
    }

    /// The relay passes the node's status through rather than flattening every
    /// answer to 200: a 404 on the far side is a 404 here.
    #[tokio::test]
    async fn the_relay_passes_the_remote_status_through() {
        let app = Router::new().route(
            "/api/nope",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "nope") }),
        );
        let node = fake_node(app).await;
        let (status, _body, _ct) =
            forward(&node.url, "k", reqwest::Method::GET, "/api/nope", vec![])
                .await
                .expect("the relay reached the node");
        assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
    }

    /// A POST body must arrive intact, or a bulk action on a remote node acts
    /// on nothing.
    #[tokio::test]
    async fn the_relay_carries_the_body_it_was_given() {
        let app = Router::new().route("/api/echo", post(|body: String| async move { body }));
        let node = fake_node(app).await;
        let (status, body, _ct) = forward(
            &node.url,
            "k",
            reqwest::Method::POST,
            "/api/echo",
            br#"{"hashes":["a","b"]}"#.to_vec(),
        )
        .await
        .expect("the relay reached the node");
        assert!(status.is_success());
        assert_eq!(String::from_utf8_lossy(&body), r#"{"hashes":["a","b"]}"#);
    }

    /// A node that is not there is an Err, not a silent empty answer that the
    /// fleet page would merge as "this node has nothing".
    #[tokio::test]
    async fn relaying_to_a_node_that_is_not_there_is_an_error() {
        let out = forward(
            "http://127.0.0.1:1",
            "k",
            reqwest::Method::GET,
            "/api/engines",
            vec![],
        )
        .await;
        assert!(
            out.is_err(),
            "an unreachable node must not look like an empty one"
        );
    }

    /// Fetching a metainfo the node does not have is a reported failure, never
    /// an empty torrent file that would be stored as if it were real.
    #[tokio::test]
    async fn fetching_a_metainfo_the_node_does_not_have_is_refused() {
        let app = Router::new().route(
            "/api/torrents/{hash}/torrent",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "no such torrent") }),
        );
        let node = fake_node(app).await;
        let out = fetch_metainfo(&node.url, "k", &"0".repeat(40), "race").await;
        assert!(out.is_err(), "a missing metainfo is an error: {out:?}");
    }

    #[tokio::test]
    async fn fetching_a_metainfo_from_a_node_that_is_not_there_says_it_is_unreachable() {
        let out = fetch_metainfo("http://127.0.0.1:1", "k", &"0".repeat(40), "race").await;
        match out {
            Err(e) => assert!(e.contains("unreachable"), "got {e}"),
            Ok(_) => panic!("there is nothing to fetch from"),
        }
    }

    // ---- probe: what each kind of refusal is called ------------------------

    use axum::extract::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn status_only(code: u16) -> Router {
        Router::new().route(
            "/api/status",
            get(move || async move { (axum::http::StatusCode::from_u16(code).unwrap(), "x") }),
        )
    }

    /// Nothing listening is reported as a refused connection, a class the
    /// operator can act on, and not as the generic "unreachable".
    #[tokio::test]
    async fn a_refused_connection_is_named_as_such() {
        let h = probe("http://127.0.0.1:1", "key").await;
        assert!(!h.online);
        assert_eq!(h.error, "connexion refusee");
        assert!(h.version.is_empty() && h.engines.is_empty() && h.torrents == 0);
    }

    /// 401 and 403 are the credential's fault; any other failing status is
    /// reported by its code. They need opposite fixes.
    #[tokio::test]
    async fn a_probe_tells_a_bad_key_from_a_failing_node() {
        for code in [401u16, 403] {
            let node = fake_node(status_only(code)).await;
            let h = probe(&node.url, "key").await;
            assert!(!h.online, "{code}");
            assert_eq!(h.error, "cle API refusee", "{code}");
        }
        let node = fake_node(status_only(500)).await;
        let h = probe(&node.url, "key").await;
        assert!(!h.online);
        assert_eq!(h.error, "HTTP 500");
        let node = fake_node(status_only(404)).await;
        assert_eq!(probe(&node.url, "key").await.error, "HTTP 404");
    }

    /// A 200 that is not JSON means another program sits on that port.
    #[tokio::test]
    async fn a_200_that_is_not_json_is_not_a_hydra() {
        let app = Router::new().route("/api/status", get(|| async { "<html>hello</html>" }));
        let node = fake_node(app).await;
        let h = probe(&node.url, "key").await;
        assert!(!h.online);
        assert_eq!(h.error, "reponse non-Hydra");
    }

    /// The node's own engine list wins over the shape of /api/status, and it
    /// is sorted; the torrent total still comes from the status sections.
    #[tokio::test]
    async fn the_engine_list_comes_from_the_engines_route_when_it_answers() {
        let app = Router::new()
            .route(
                "/api/status",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "version": "4.30.0",
                        "hoard": {"total_torrents": 10},
                        "race": {"torrents": 5},
                        "baseline": {"global_uploaded": 7}
                    }))
                }),
            )
            .route(
                "/api/engines",
                get(|| async {
                    axum::Json(serde_json::json!([{"id": "vpn2"}, {"id": "hoard"}, {"id": "race"}, {"nameless": 1}]))
                }),
            );
        let node = fake_node(app).await;
        let h = probe(&node.url, "key").await;
        assert!(h.online);
        assert_eq!(
            h.engines,
            vec!["hoard", "race", "vpn2"],
            "sorted, entries without an id skipped"
        );
        assert_eq!(h.torrents, 15);
        assert_eq!(h.version, "4.30.0");
    }

    /// A node old enough to answer `[]`, or not to have the route at all,
    /// is still legible through the sections of /api/status.
    #[tokio::test]
    async fn an_old_node_falls_back_to_the_shape_of_its_status() {
        for engines_route in [
            Some(serde_json::json!([])),
            Some(serde_json::json!({"not": "a list"})),
            None,
        ] {
            let mut app = Router::new().route(
                "/api/status",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "race": {"torrents": 2},
                        "hoard": {"total_torrents": 3},
                        "uptime": 12
                    }))
                }),
            );
            if let Some(body) = engines_route {
                app = app.route(
                    "/api/engines",
                    get(move || {
                        let b = body.clone();
                        async move { axum::Json(b) }
                    }),
                );
            }
            let node = fake_node(app).await;
            let h = probe(&node.url, "key").await;
            assert!(h.online);
            assert_eq!(h.engines, vec!["hoard", "race"]);
            assert_eq!(h.torrents, 5);
            assert!(
                h.version.is_empty(),
                "no version declared is an empty one, not a guess"
            );
        }
    }

    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// The key travels on both calls of a probe, in the header and never in
    /// the URL.
    #[tokio::test]
    async fn a_probe_sends_its_key_on_every_call_and_never_in_the_url() {
        let seen: Seen = Arc::default();
        let rec = |seen: Seen| {
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let seen = seen.clone();
                async move {
                    let k = headers
                        .get("x-api-key")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().push((
                        uri.path().to_string(),
                        uri.query().unwrap_or("").to_string(),
                        k,
                    ));
                    axum::Json(serde_json::json!({"hoard": {"total_torrents": 1}}))
                }
            }
        };
        let app = Router::new()
            .route("/api/status", get(rec(seen.clone())))
            .route("/api/engines", get(rec(seen.clone())));
        let node = fake_node(app).await;
        probe(&node.url, "0123456789abcdef0123456789abcdef").await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for (path, query, key) in seen.iter() {
            assert_eq!(key, "0123456789abcdef0123456789abcdef", "{path}");
            assert!(query.is_empty(), "{path} carries no query");
        }
    }

    /// The UI reads absent fields as "unknown": an offline node serialises
    /// without version or engines, only with its reason.
    #[test]
    fn an_offline_health_omits_what_it_does_not_know() {
        let v = serde_json::to_value(Health {
            online: false,
            error: "timeout".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"online": false, "torrents": 0, "error": "timeout"})
        );
        let v = serde_json::to_value(Health {
            online: true,
            version: "1".into(),
            engines: vec!["race".into()],
            torrents: 3,
            error: String::new(),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"online": true, "version": "1", "engines": ["race"], "torrents": 3})
        );
    }

    // ---- forward ------------------------------------------------------------

    /// A relayed body is declared as JSON, and an empty one carries no
    /// content type at all.
    #[tokio::test]
    async fn the_relay_declares_a_body_as_json_and_an_empty_request_has_none() {
        let app = Router::new().route(
            "/api/ct",
            post(|headers: axum::http::HeaderMap, body: String| async move {
                let ct = headers
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("none")
                    .to_string();
                axum::Json(serde_json::json!({"ct": ct, "len": body.len()}))
            }),
        );
        let node = fake_node(app).await;
        let (_, body, _) = forward(
            &node.url,
            "k",
            reqwest::Method::POST,
            "api/ct",
            b"{}".to_vec(),
        )
        .await
        .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ct"], "application/json");
        assert_eq!(v["len"], 2);
        let (_, body, _) = forward(&node.url, "k", reqwest::Method::POST, "/api/ct", vec![])
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ct"], "none");
        assert_eq!(v["len"], 0);
    }

    /// The far side's content type comes back with the bytes, and an answer
    /// that names none is taken for JSON.
    #[tokio::test]
    async fn the_relay_returns_the_content_type_of_the_answer() {
        let app = Router::new()
            .route("/text", get(|| async { "plain words" }))
            .route(
                "/bare",
                get(|| async {
                    axum::http::Response::builder()
                        .body(axum::body::Body::from("{}"))
                        .unwrap()
                }),
            );
        let node = fake_node(app).await;
        let (_, body, ct) = forward(&node.url, "k", reqwest::Method::GET, "/text", vec![])
            .await
            .unwrap();
        assert_eq!(body, b"plain words");
        assert!(ct.starts_with("text/plain"), "{ct}");
        let (_, _, ct) = forward(&node.url, "k", reqwest::Method::GET, "/bare", vec![])
            .await
            .unwrap();
        assert_eq!(ct, "application/json");
    }

    /// The URL's trailing slash and the path's leading one are both absorbed.
    #[tokio::test]
    async fn the_relay_joins_url_and_path_with_exactly_one_slash() {
        let app = Router::new().route("/a/b", get(|| async { "ok" }));
        let node = fake_node(app).await;
        for (url, path) in [
            (node.url.clone(), "/a/b"),
            (format!("{}/", node.url), "a/b"),
            (format!("{}/", node.url), "//a/b"),
        ] {
            let (status, _, _) = forward(&url, "k", reqwest::Method::GET, path, vec![])
                .await
                .unwrap();
            assert!(status.is_success(), "{url} + {path}");
        }
    }

    // ---- handoff -----------------------------------------------------------

    type Upload = (String, Option<String>, Vec<u8>);

    /// A receiving node: knows `categories`, records what its upload route
    /// is sent, answers `upload_status`, and echoes peers.
    struct Receiver {
        node: FakeNode,
        uploads: Arc<Mutex<Vec<Upload>>>,
        upload_key: Arc<Mutex<String>>,
        peers: Arc<Mutex<Vec<String>>>,
        categories_asked: Arc<AtomicUsize>,
    }

    async fn receiver(categories: serde_json::Value, upload_status: u16) -> Receiver {
        let uploads: Arc<Mutex<Vec<Upload>>> = Arc::default();
        let upload_key: Arc<Mutex<String>> = Arc::default();
        let peers: Arc<Mutex<Vec<String>>> = Arc::default();
        let categories_asked = Arc::new(AtomicUsize::new(0));
        let (u, uk, p, ca) = (
            uploads.clone(),
            upload_key.clone(),
            peers.clone(),
            categories_asked.clone(),
        );
        let app = Router::new()
            .route(
                "/api/categories",
                get(move || {
                    let (c, ca) = (categories.clone(), ca.clone());
                    async move {
                        ca.fetch_add(1, Ordering::SeqCst);
                        axum::Json(c)
                    }
                }),
            )
            .route(
                "/api/torrents/upload",
                post(
                    move |headers: axum::http::HeaderMap, mut mp: axum::extract::Multipart| {
                        let (u, uk) = (u.clone(), uk.clone());
                        async move {
                            *uk.lock().unwrap() = headers
                                .get("x-api-key")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                                .to_string();
                            while let Some(f) = mp.next_field().await.unwrap() {
                                let name = f.name().unwrap_or("").to_string();
                                let file = f.file_name().map(String::from);
                                let data = f.bytes().await.unwrap().to_vec();
                                u.lock().unwrap().push((name, file, data));
                            }
                            (
                                axum::http::StatusCode::from_u16(upload_status).unwrap(),
                                "x",
                            )
                        }
                    },
                ),
            )
            .route(
                "/api/torrents/:hash/peers",
                post(move |Path(hash): Path<String>, body: String| {
                    let p = p.clone();
                    async move {
                        p.lock().unwrap().push(format!("{hash} {body}"));
                        axum::Json(serde_json::json!({"added": 1}))
                    }
                }),
            );
        Receiver {
            node: fake_node(app).await,
            uploads,
            upload_key,
            peers,
            categories_asked,
        }
    }

    const HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    /// The category is checked BEFORE the metainfo travels: a category the
    /// far side does not know is refused by name, with the ones that would
    /// work, and nothing is uploaded -- it could land in the wrong tier.
    #[tokio::test]
    async fn a_handoff_to_an_unknown_category_is_refused_before_anything_is_sent() {
        let r = receiver(
            serde_json::json!([{"name": "movies"}, {"name": "series"}, {"nope": 1}]),
            200,
        )
        .await;
        let err = handoff(
            &r.node.url,
            "k",
            HASH,
            b"meta".to_vec(),
            "192.0.2.7:16171",
            "books",
            "",
        )
        .await
        .unwrap_err();
        assert!(err.contains("no category books"), "{err}");
        assert!(
            err.contains("movies, series"),
            "lists what would work: {err}"
        );
        assert!(r.uploads.lock().unwrap().is_empty(), "nothing was uploaded");
        assert!(r.peers.lock().unwrap().is_empty());
    }

    /// A known category goes on, with the category, the engine and the file
    /// named after the hash; then the sender is announced as the peer.
    #[tokio::test]
    async fn a_handoff_uploads_the_metainfo_then_names_the_sender_as_a_peer() {
        let r = receiver(serde_json::json!([{"name": "movies"}]), 200).await;
        let out = handoff(
            &r.node.url,
            "remote-key",
            HASH,
            b"METAINFO".to_vec(),
            "192.0.2.7:16171",
            "movies",
            "vpn1",
        )
        .await
        .expect("handed off");
        assert_eq!(out["status"], "ok");
        assert_eq!(out["info_hash"], HASH);
        assert_eq!(out["from"], "192.0.2.7:16171");
        assert_eq!(
            out["peer"],
            serde_json::json!({"added": 1}),
            "the node's answer to the peer call comes back"
        );
        assert_eq!(*r.upload_key.lock().unwrap(), "remote-key");

        let up = r.uploads.lock().unwrap();
        let torrents = up
            .iter()
            .find(|(n, _, _)| n == "torrents")
            .expect("the metainfo part");
        assert_eq!(
            torrents.1.as_deref(),
            Some(format!("{HASH}.torrent").as_str())
        );
        assert_eq!(torrents.2, b"METAINFO");
        let text = |n: &str| {
            up.iter()
                .find(|(k, _, _)| k == n)
                .map(|(_, _, d)| String::from_utf8_lossy(d).into_owned())
        };
        assert_eq!(text("category").as_deref(), Some("movies"));
        assert_eq!(text("engine").as_deref(), Some("vpn1"));
        let peers = r.peers.lock().unwrap();
        assert_eq!(peers.len(), 1);
        let (h, body) = peers[0].split_once(' ').unwrap();
        assert_eq!(h, HASH);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({"peers": ["192.0.2.7:16171"]})
        );
    }

    /// With no category nor engine named, neither the category lookup nor
    /// those form fields exist: the far side decides.
    #[tokio::test]
    async fn a_handoff_with_no_category_asks_nothing_and_sends_neither_field() {
        let r = receiver(serde_json::json!([]), 200).await;
        handoff(&r.node.url, "k", HASH, b"m".to_vec(), "192.0.2.7:1", "", "")
            .await
            .expect("handed off");
        assert_eq!(r.categories_asked.load(Ordering::SeqCst), 0);
        let up = r.uploads.lock().unwrap();
        assert!(
            up.iter().all(|(n, _, _)| n != "category" && n != "engine"),
            "only the torrent part: {:?}",
            up.iter().map(|u| &u.0).collect::<Vec<_>>()
        );
    }

    /// A node that lists no category at all cannot refuse one: the transfer
    /// goes on and the far side's own answer decides.
    #[tokio::test]
    async fn a_node_with_an_empty_category_list_does_not_block_the_handoff() {
        let r = receiver(serde_json::json!([]), 200).await;
        handoff(
            &r.node.url,
            "k",
            HASH,
            b"m".to_vec(),
            "192.0.2.7:1",
            "anything",
            "",
        )
        .await
        .expect("handed off");
        assert_eq!(r.categories_asked.load(Ordering::SeqCst), 1);
        assert_eq!(
            r.uploads.lock().unwrap().len(),
            2,
            "torrent part + category"
        );
    }

    /// The far side refusing the upload is reported with its code, and the
    /// sender is NOT announced for a torrent that was not taken.
    #[tokio::test]
    async fn a_refused_upload_stops_the_handoff_with_the_status() {
        let r = receiver(serde_json::json!([]), 503).await;
        let err = handoff(&r.node.url, "k", HASH, b"m".to_vec(), "192.0.2.7:1", "", "")
            .await
            .unwrap_err();
        assert_eq!(err, "the node refused the torrent: HTTP 503");
        assert!(r.peers.lock().unwrap().is_empty());
    }

    /// A node that is gone reads as one that did not accept the metainfo.
    #[tokio::test]
    async fn a_handoff_to_a_node_that_is_gone_says_the_metainfo_was_not_accepted() {
        let err = handoff(
            "http://127.0.0.1:1",
            "k",
            HASH,
            b"m".to_vec(),
            "192.0.2.7:1",
            "",
            "",
        )
        .await
        .unwrap_err();
        assert_eq!(err, "the node did not accept the metainfo");
    }

    // ---- remote_progress --------------------------------------------------------

    /// A node whose engines each answer a page with `rows`, as
    /// `/api/engines/<id>/page` does. `None` for an engine = answers 500.
    async fn paged_node(engines: Vec<(&'static str, Option<serde_json::Value>)>) -> FakeNode {
        let ids: Vec<serde_json::Value> = engines
            .iter()
            .map(|(id, _)| serde_json::json!({"id": id}))
            .collect();
        let pages: Arc<std::collections::HashMap<String, Option<serde_json::Value>>> = Arc::new(
            engines
                .into_iter()
                .map(|(id, p)| (id.to_string(), p))
                .collect(),
        );
        let app = Router::new()
            .route(
                "/api/engines",
                get(move || {
                    let ids = ids.clone();
                    async move { axum::Json(ids) }
                }),
            )
            .route(
                "/api/engines/:id/page",
                get(move |Path(id): Path<String>| {
                    let pages = pages.clone();
                    async move {
                        match pages.get(&id).cloned().flatten() {
                            Some(v) => (axum::http::StatusCode::OK, axum::Json(v)),
                            None => (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                axum::Json(serde_json::json!({})),
                            ),
                        }
                    }
                }),
            );
        fake_node(app).await
    }

    fn row(hash: &str, progress: f64) -> serde_json::Value {
        serde_json::json!({"info_hash": hash, "progress": progress})
    }

    /// With no engine named every engine is asked and the furthest copy
    /// counts: the far side's category decided where it went.
    #[tokio::test]
    async fn the_furthest_engine_copy_is_the_progress() {
        let node = paged_node(vec![
            ("race", Some(serde_json::json!({"rows": [row(HASH, 0.25)]}))),
            (
                "hoard",
                Some(serde_json::json!({"rows": [row(HASH, 0.75)]})),
            ),
            ("vpn1", Some(serde_json::json!({"rows": []}))),
        ])
        .await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "").await,
            Ok(Some(0.75))
        );
    }

    /// The page search also matches names: a row that is not this hash is
    /// not this torrent, and the hash compares without regard to case.
    #[tokio::test]
    async fn a_row_with_another_hash_is_not_our_torrent() {
        let other = "ffffffffffffffffffffffffffffffffffffffff";
        let node = paged_node(vec![(
            "race",
            Some(serde_json::json!({"rows": [row(other, 1.0)]})),
        )])
        .await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "race").await,
            Ok(None),
            "answered, but nothing of ours yet"
        );
        let node = paged_node(vec![(
            "race",
            Some(serde_json::json!({"rows": [row(other, 1.0), row(&HASH.to_uppercase(), 0.5)]})),
        )])
        .await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "race").await,
            Ok(Some(0.5))
        );
    }

    /// A row with no progress figure counts as nothing downloaded -- not as
    /// complete, which would delete the local copy.
    #[tokio::test]
    async fn a_row_without_progress_is_not_a_finished_copy() {
        let node = paged_node(vec![(
            "race",
            Some(serde_json::json!({"rows": [{"info_hash": HASH}]})),
        )])
        .await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "race").await,
            Ok(Some(0.0))
        );
    }

    /// One engine failing does not hide another's answer; every engine
    /// failing is an error, not "not there yet".
    #[tokio::test]
    async fn an_engine_that_fails_is_skipped_unless_all_do() {
        let node = paged_node(vec![
            ("race", None),
            ("hoard", Some(serde_json::json!({"rows": [row(HASH, 0.5)]}))),
        ])
        .await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "").await,
            Ok(Some(0.5))
        );
        let node = paged_node(vec![("race", None), ("hoard", None)]).await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "").await,
            Err("the node did not answer for any engine".to_string())
        );
    }

    #[tokio::test]
    async fn progress_from_a_node_that_is_gone_or_has_no_engine_list_is_an_error() {
        assert_eq!(
            remote_progress("http://127.0.0.1:1", "k", HASH, "").await,
            Err("the node is unreachable".to_string())
        );
        let app = Router::new().route("/api/engines", get(|| async { "not json" }));
        let node = fake_node(app).await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "").await,
            Err("the node's engine list did not parse".to_string())
        );
        // A list that is not an array means no engine to ask.
        let app = Router::new().route(
            "/api/engines",
            get(|| async { axum::Json(serde_json::json!({"x": 1})) }),
        );
        let node = fake_node(app).await;
        assert_eq!(
            remote_progress(&node.url, "k", HASH, "").await,
            Err("the node did not answer for any engine".to_string())
        );
    }

    // ---- key rotation ------------------------------------------------------------

    fn rotation_node(
        rotate: (u16, serde_json::Value),
        confirm: u16,
        confirm_seen: Arc<Mutex<Vec<String>>>,
    ) -> Router {
        Router::new()
            .route(
                "/api/auth/api-key/rotate",
                post(move || {
                    let (code, body) = rotate.clone();
                    async move {
                        (
                            axum::http::StatusCode::from_u16(code).unwrap(),
                            axum::Json(body),
                        )
                    }
                }),
            )
            .route(
                "/api/auth/api-key/confirm",
                post(move |headers: axum::http::HeaderMap| {
                    let seen = confirm_seen.clone();
                    async move {
                        let k = headers
                            .get("x-api-key")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        seen.lock().unwrap().push(k);
                        (axum::http::StatusCode::from_u16(confirm).unwrap(), "x")
                    }
                }),
            )
    }

    const NEW_KEY: &str = "fedcba9876543210fedcba9876543210";

    /// The new key is confirmed with ITSELF in the header -- proof the
    /// controller received it -- and is what comes back.
    #[tokio::test]
    async fn rotation_confirms_with_the_key_it_was_given() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let node = fake_node(rotation_node(
            (200, serde_json::json!({"pending_key": NEW_KEY})),
            200,
            seen.clone(),
        ))
        .await;
        assert_eq!(rotate_key(&node.url, "old").await, Ok(NEW_KEY.to_string()));
        assert_eq!(*seen.lock().unwrap(), vec![NEW_KEY.to_string()]);
    }

    /// Each way the first call can be refused has its own advice, and none
    /// goes on to the confirmation.
    #[tokio::test]
    async fn a_refused_rotation_says_why_and_never_confirms() {
        let cases: [(u16, &str); 5] = [
            (
                401,
                "the node refused the current key: declare it again with its key",
            ),
            (
                403,
                "the node refused the current key: declare it again with its key",
            ),
            (404, "the node is too old to rotate its key (needs 4.4)"),
            (405, "the node is too old to rotate its key (needs 4.4)"),
            (500, "the node refused the rotation: HTTP 500"),
        ];
        for (code, want) in cases {
            let seen: Arc<Mutex<Vec<String>>> = Arc::default();
            let node = fake_node(rotation_node(
                (code, serde_json::json!({})),
                200,
                seen.clone(),
            ))
            .await;
            assert_eq!(
                rotate_key(&node.url, "old").await,
                Err(want.to_string()),
                "{code}"
            );
            assert!(
                seen.lock().unwrap().is_empty(),
                "{code}: no confirmation was attempted"
            );
        }
    }

    /// A key too short to be a key is not adopted, and a missing one is the
    /// same: nothing is confirmed.
    #[tokio::test]
    async fn a_pending_key_that_is_unusable_is_not_confirmed() {
        for body in [
            serde_json::json!({"pending_key": "short"}),
            serde_json::json!({}),
            serde_json::json!({"pending_key": 7}),
        ] {
            let seen: Arc<Mutex<Vec<String>>> = Arc::default();
            let node = fake_node(rotation_node((200, body), 200, seen.clone())).await;
            assert_eq!(
                rotate_key(&node.url, "old").await,
                Err("the node answered no usable key".to_string())
            );
            assert!(seen.lock().unwrap().is_empty());
        }
        let app = Router::new().route("/api/auth/api-key/rotate", post(|| async { "not json" }));
        let node = fake_node(app).await;
        assert_eq!(
            rotate_key(&node.url, "old").await,
            Err("the node's answer did not parse".to_string())
        );
    }

    /// The length floor is exact: a 16-character key is the shortest adopted
    /// (and is confirmed with itself), a 15-character one is refused.
    #[tokio::test]
    async fn a_pending_key_is_adopted_from_sixteen_characters_and_not_below() {
        let shortest = "0123456789abcdef";
        assert_eq!(shortest.len(), 16);
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let node = fake_node(rotation_node(
            (200, serde_json::json!({"pending_key": shortest})),
            200,
            seen.clone(),
        ))
        .await;
        assert_eq!(rotate_key(&node.url, "old").await, Ok(shortest.to_string()));
        assert_eq!(
            *seen.lock().unwrap(),
            vec![shortest.to_string()],
            "confirmed with itself"
        );

        let too_short = &shortest[..15];
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let node = fake_node(rotation_node(
            (200, serde_json::json!({"pending_key": too_short})),
            200,
            seen.clone(),
        ))
        .await;
        assert_eq!(
            rotate_key(&node.url, "old").await,
            Err("the node answered no usable key".to_string())
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "a 15-character key is never confirmed"
        );
    }

    /// A node that does not confirm keeps its old key, and the error says so.
    #[tokio::test]
    async fn a_failed_confirmation_is_reported_as_keeping_the_old_key() {
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let node = fake_node(rotation_node(
            (200, serde_json::json!({"pending_key": NEW_KEY})),
            401,
            seen,
        ))
        .await;
        assert_eq!(
            rotate_key(&node.url, "old").await,
            Err(
                "the node did not confirm the new key (HTTP 401); it keeps its old key".to_string()
            )
        );
        assert_eq!(
            rotate_key("http://127.0.0.1:1", "old").await,
            Err("the node is unreachable".to_string())
        );
    }

    /// The node side: a pending key is single-use, exact, and expires.
    /// One test, one lock: the pending key is process-wide.
    #[test]
    fn a_pending_key_is_single_use_exact_and_expires() {
        let _g = PENDING_KEY_TEST_LOCK.blocking_lock();
        let k = begin_local_rotation();
        assert!(k.len() >= 16, "a usable key: {k}");
        assert_eq!(take_pending_key(""), None, "empty never matches");
        assert_eq!(take_pending_key("not-it"), None);
        assert_eq!(
            take_pending_key(&format!("{k}x")),
            None,
            "a longer string is not the key"
        );
        assert_eq!(take_pending_key(&k), Some(k.clone()));
        assert_eq!(take_pending_key(&k), None, "consumed: a replay is refused");

        // A newer rotation replaces the earlier pending key.
        let first = begin_local_rotation();
        let second = begin_local_rotation();
        assert_ne!(first, second);
        assert_eq!(take_pending_key(&first), None);
        assert_eq!(take_pending_key(&second), Some(second.clone()));

        // Past its time-to-live the right key is refused, and forgotten.
        let stale = begin_local_rotation();
        let aged = std::time::Instant::now()
            .checked_sub(PENDING_KEY_TTL + Duration::from_secs(1))
            .expect("the host has been up longer than the TTL");
        *PENDING_KEY.lock().unwrap() = Some((stale.clone(), aged));
        assert_eq!(take_pending_key(&stale), None);
        assert!(
            PENDING_KEY.lock().unwrap().is_none(),
            "an expired key is dropped"
        );
        assert_eq!(take_pending_key(&stale), None);
    }

    // ---- fetch_metainfo ----------------------------------------------------------

    fn metainfo_node(engines: serde_json::Value) -> Router {
        Router::new()
            .route(
                "/api/torrents/:hash/torrent",
                get(|| async { (axum::http::StatusCode::OK, vec![0x64u8, 0x65, 0x00, 0xff]) }),
            )
            .route(
                "/api/engines",
                get(move || {
                    let e = engines.clone();
                    async move { axum::Json(e) }
                }),
            )
    }

    /// The bytes are returned untouched, with the listening port of the
    /// engine named -- not of another one.
    #[tokio::test]
    async fn a_fetched_metainfo_comes_with_the_port_of_the_engine_asked() {
        let node = fake_node(metainfo_node(serde_json::json!([
            {"id": "race", "listen_port": 16171},
            {"id": "vpn1", "listen_port": 16380}
        ])))
        .await;
        let (blob, port) = fetch_metainfo(&node.url, "k", HASH, "vpn1").await.unwrap();
        assert_eq!(blob, vec![0x64, 0x65, 0x00, 0xff]);
        assert_eq!(port, 16380);
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "race")
                .await
                .unwrap()
                .1,
            16171
        );
    }

    #[tokio::test]
    async fn a_fetch_names_the_engine_or_port_it_cannot_use() {
        let node = fake_node(metainfo_node(
            serde_json::json!([{"id": "race", "listen_port": 0}, {"id": "hoard"}]),
        ))
        .await;
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "vpn9")
                .await
                .unwrap_err(),
            "the node has no engine named vpn9"
        );
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "race")
                .await
                .unwrap_err(),
            "race has no listening port on that node"
        );
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "hoard")
                .await
                .unwrap_err(),
            "the node has no engine named hoard",
            "no port field is no engine we can dial"
        );
        let node = fake_node(metainfo_node(serde_json::json!({"not": "a list"}))).await;
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "race")
                .await
                .unwrap_err(),
            "the node has no engine named race"
        );
    }

    /// The refusal quotes the first eight characters of the hash and survives
    /// a hash shorter than that.
    #[tokio::test]
    async fn a_missing_metainfo_quotes_a_short_hash_without_panicking() {
        let app = Router::new().route(
            "/api/torrents/:hash/torrent",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "no") }),
        );
        let node = fake_node(app).await;
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "race")
                .await
                .unwrap_err(),
            "the node has no .torrent for 01234567: HTTP 404"
        );
        assert_eq!(
            fetch_metainfo(&node.url, "k", "abc", "race")
                .await
                .unwrap_err(),
            "the node has no .torrent for abc: HTTP 404"
        );
    }

    /// The .torrent arrived but the engine list did not: each is its own error.
    #[tokio::test]
    async fn a_fetch_whose_engine_list_is_broken_says_so() {
        let app = Router::new()
            .route("/api/torrents/:hash/torrent", get(|| async { "bytes" }))
            .route("/api/engines", get(|| async { "<html>" }));
        let node = fake_node(app).await;
        assert_eq!(
            fetch_metainfo(&node.url, "k", HASH, "race")
                .await
                .unwrap_err(),
            "the node's engine list did not parse"
        );
    }
}
