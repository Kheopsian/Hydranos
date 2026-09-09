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
            }
        }
    };
    if res.status() == reqwest::StatusCode::UNAUTHORIZED
        || res.status() == reqwest::StatusCode::FORBIDDEN
    {
        return Health { online: false, error: "cle API refusee".into(), ..Default::default() };
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
        if e.is_timeout() { "timeout".to_string() } else { "injoignable".to_string() }
    })?;
    let status = res.status();
    let ctype = res
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = res.bytes().await.map_err(|_| "corps illisible".to_string())?;
    Ok((status, bytes.to_vec(), ctype))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        return Err(format!("the node refused the torrent: HTTP {}", res.status().as_u16()));
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

/// Poll a node until it reports the torrent complete.
///
/// Used by a MOVE: the target has the metainfo long before it has the bytes, so
/// nothing local may be deleted until the far side actually holds a full copy.
///
/// Bounded and fail-safe. On a timeout, a network fault, or a restart of this
/// process, the answer is "not confirmed" and the caller keeps its copy -- a
/// duplicate to clean up is recoverable, a deletion is not.
pub async fn wait_until_complete(
    url: &str,
    api_key: &str,
    info_hash: &str,
    engine: &str,
) -> Result<bool, String> {
    let base = url.trim_end_matches('/');
    let engine = if engine.is_empty() { "hoard" } else { engine };
    // Six hours at half a minute: long enough for a large payload over a home
    // link, short enough that a forgotten task does not outlive the reason.
    let deadline = std::time::Instant::now() + Duration::from_secs(6 * 3600);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(30)).await;
        let res = client()
            .get(format!(
                "{base}/api/{engine}/page?limit=1&search={info_hash}"
            ))
            .header("X-API-Key", api_key)
            .send()
            .await;
        let Ok(res) = res else { continue };
        let Ok(body) = res.json::<serde_json::Value>().await else { continue };
        let Some(row) = body.get("rows").and_then(|r| r.as_array()).and_then(|a| a.first())
        else {
            // Not listed there: the torrent went to another engine, or was
            // removed on the far side. Either way this is not a confirmation.
            continue;
        };
        let progress = row.get("progress").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if progress >= 1.0 {
            return Ok(true);
        }
    }
    Ok(false)
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
