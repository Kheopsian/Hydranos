use std::error::Error as StdError;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

use crate::torrent::metainfo::{bencode_decode, BencodeValue};

/// Flatten a reqwest / io error chain into a single string. `reqwest::Error`'s
/// Display implementation stops at the outer layer ("error sending request for
/// url (...)"), which hides the real cause (dns, timeout, connection reset,
/// tls, etc.). Walk the `source()` chain to surface it.
fn fmt_err_chain<E: StdError + ?Sized>(e: &E) -> String {
    let mut out = e.to_string();
    let mut src: Option<&(dyn StdError + 'static)> = e.source();
    while let Some(c) = src {
        let s = c.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        src = c.source();
    }
    out
}

/// Lazy-initialized reqwest Client that routes through an IPv6 SOCKS5h proxy.
/// Configured via env `TYPHON_ANNOUNCE_V6_PROXY` = e.g.
/// `socks5h://user:pass@172.17.0.1:1080`. When set, every successful primary
/// announce also fires a parallel announce through this client so the tracker
/// registers us BOTH as v4 peer (main path via FOU) and v6 peer (proxy exits
/// the VPS in IPv6). `socks5h` makes the proxy resolve the hostname — key for
/// trackers behind CloudFlare whose AAAA is only selected when resolution
/// happens on the proxy side. Set to empty string to disable.
static V6_PROXY_CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();

fn v6_proxy_client() -> Option<&'static reqwest::Client> {
    V6_PROXY_CLIENT
        .get_or_init(|| {
            let url = match std::env::var("TYPHON_ANNOUNCE_V6_PROXY") {
                Ok(u) if !u.is_empty() => u,
                _ => { eprintln!("[tracker] TYPHON_ANNOUNCE_V6_PROXY not set — no secondary announce"); return None; }
            };
            let proxy = match reqwest::Proxy::all(&url) {
                Ok(p) => p,
                Err(e) => { eprintln!("[tracker] TYPHON_ANNOUNCE_V6_PROXY parse failed ({}): {}", url, e); return None; }
            };
            match reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("Hydra/2.4.3-typhon")
                .proxy(proxy)
                .build()
            {
                Ok(c) => { eprintln!("[tracker] secondary announce proxied via {}", url); Some(c) }
                Err(e) => { eprintln!("[tracker] secondary client build failed: {}", e); None }
            }
        })
        .as_ref()
}

/// Primary reqwest client — can optionally route through a SOCKS5 proxy
/// via env `TYPHON_ANNOUNCE_PROXY` (same syntax as V6_PROXY). Lets us kill
/// the IPv6 Freebox leak: without this, the default reqwest client would
/// dial tracker.example.net AAAA straight from the styx netns source
/// (2001:db8:abc:d12::3) — visible in tracker peer lists.
static PRIMARY_PROXY: OnceLock<Option<reqwest::Proxy>> = OnceLock::new();

fn primary_proxy() -> Option<&'static reqwest::Proxy> {
    PRIMARY_PROXY
        .get_or_init(|| {
            let url = std::env::var("TYPHON_ANNOUNCE_PROXY").ok()?;
            if url.is_empty() {
                eprintln!("[tracker] TYPHON_ANNOUNCE_PROXY empty — primary announce goes direct (leak risk)");
                return None;
            }
            match reqwest::Proxy::all(&url) {
                Ok(p) => {
                    eprintln!("[tracker] primary announce proxied via {}", url);
                    Some(p)
                }
                Err(e) => {
                    eprintln!("[tracker] TYPHON_ANNOUNCE_PROXY parse failed ({}): {}", url, e);
                    None
                }
            }
        })
        .as_ref()
}

pub struct AnnounceResponse {
    pub interval: u32,
    pub peers: Vec<SocketAddr>,
    pub complete: u32,
    pub incomplete: u32,
    pub failure: Option<String>,
}

/// Perform an HTTP tracker announce.
pub async fn announce(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    uploaded: u64,
    downloaded: u64,
    left: u64,
    event: &str,
) -> Result<AnnounceResponse, String> {
    // URL-encode info_hash and peer_id (binary -> %XX)
    let ih_encoded = url_encode_binary(info_hash);
    let pid_encoded = url_encode_binary(peer_id);

    let sep = if tracker_url.contains('?') { "&" } else { "?" };
    let url = format!(
        "{}{}\
        info_hash={}&\
        peer_id={}&\
        port={}&\
        uploaded={}&\
        downloaded={}&\
        left={}&\
        compact=1&\
        numwant={}\
        {}",
        tracker_url,
        sep,
        ih_encoded,
        pid_encoded,
        port,
        uploaded,
        downloaded,
        left,
        // A complete torrent asks for NO peers. We are directly reachable, so a
        // leecher -- NAT or not -- opens the connection to us; there is nothing
        // for us to dial. Asking for 200 peers per announce across a catalogue
        // of seeding torrents is what produced thousands of idle sockets to the
        // same handful of large seedboxes, one per shared swarm.
        // NOTE: this makes a complete torrent PASSIVE. It relies on our listen
        // port staying reachable; if the port forward breaks, upload stops dead
        // rather than degrading.
        if left == 0 { 0 } else { 200 },
        if event.is_empty() { String::new() } else { format!("&event={}", event) },
    );

    // HTTP GET with timeout. Route via TYPHON_ANNOUNCE_PROXY if set to
    // avoid leaking the styx-netns v6 source IP on AAAA-only trackers.
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Hydra/2.4.3-typhon");
    if let Some(px) = primary_proxy() {
        builder = builder.proxy(px.clone());
    }
    let client = builder
        .build()
        .map_err(|e| format!("http client: {}", fmt_err_chain(&e)))?;

    // Fire-and-forget second announce through IPv6 SOCKS5h proxy if configured.
    // Utilisé pour ajouter le path v4 via gost-v4 (nft ip6 gost_v4_block).
    // On modifie le dernier byte du peer_id pour éviter le dédup tracker par
    // peer_id (certains trackers écrasent l'entrée au lieu de stocker v4+v6).
    if let Some(pxc) = v6_proxy_client() {
        let mut pid_secondary = *peer_id;
        pid_secondary[19] ^= 0x01;
        let pid_sec_encoded = url_encode_binary(&pid_secondary);
        let url2 = url.replace(&pid_encoded, &pid_sec_encoded);
        let pxc = pxc.clone();
        tokio::spawn(async move {
            match pxc.get(&url2).send().await {
                Ok(r) => {
                    let st = r.status();
                    if !st.is_success() {
                        eprintln!("[tracker] secondary announce HTTP {} on {}", st, url2);
                    }
                }
                Err(e) => eprintln!("[tracker] secondary announce FAIL on {} : {}", url2, e),
            }
        });
    }

    let resp = client.get(&url)
        .send()
        .await
        .map_err(|e| format!("http request: {}", fmt_err_chain(&e)))?;

    if !resp.status().is_success() {
        let st = resp.status();
        // Capture up to 200 chars of body so tracker-provided failure reasons
        // on non-2xx (403 banned, 502 cloudflare, etc.) reach the user.
        let body = resp.text().await.unwrap_or_default();
        let snip: String = body.chars().take(200).collect();
        return Err(format!("http {}: {}", st, snip.trim()));
    }

    let body = resp.bytes().await
        .map_err(|e| format!("http body: {}", fmt_err_chain(&e)))?;

    // Parse bencoded response
    parse_announce_response(&body)
}

fn parse_announce_response(data: &[u8]) -> Result<AnnounceResponse, String> {
    let value = bencode_decode(data)?;
    let dict = value.as_dict().ok_or("response not a dict")?;

    // Check for failure
    if let Some(reason) = dict.get("failure reason") {
        if let Some(msg) = reason.as_string() {
            return Err(format!("tracker: {}", msg));
        }
    }

    let interval = dict.get("interval")
        .and_then(|v| v.as_int())
        .unwrap_or(1800) as u32;

    let complete = dict.get("complete")
        .and_then(|v| v.as_int())
        .unwrap_or(0) as u32;

    let incomplete = dict.get("incomplete")
        .and_then(|v| v.as_int())
        .unwrap_or(0) as u32;

    // Parse compact peers (6 bytes each: 4 IP + 2 port)
    let mut peers = Vec::new();
    if let Some(peers_val) = dict.get("peers") {
        if let Some(compact) = peers_val.as_bytes() {
            // Compact format
            for chunk in compact.chunks(6) {
                if chunk.len() == 6 {
                    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                    peers.push(SocketAddr::new(IpAddr::V4(ip), port));
                }
            }
        } else if let Some(peer_list) = peers_val.as_list() {
            // Dict format
            for p in peer_list {
                if let Some(pd) = p.as_dict() {
                    let ip_str = pd.get("ip").and_then(|v| v.as_string()).unwrap_or("");
                    let port = pd.get("port").and_then(|v| v.as_int()).unwrap_or(0) as u16;
                    if let Ok(ip) = ip_str.parse::<IpAddr>() {
                        peers.push(SocketAddr::new(ip, port));
                    }
                }
            }
        }
    }

    // Parse compact peers6 (BEP 7: 18 bytes each = 16 IPv6 + 2 port).
    // Without this we silently drop every v6 peer returned by the tracker.
    if let Some(peers6_val) = dict.get("peers6") {
        if let Some(compact) = peers6_val.as_bytes() {
            for chunk in compact.chunks(18) {
                if chunk.len() == 18 {
                    let mut ip_bytes = [0u8; 16];
                    ip_bytes.copy_from_slice(&chunk[0..16]);
                    let ip = std::net::Ipv6Addr::from(ip_bytes);
                    let port = u16::from_be_bytes([chunk[16], chunk[17]]);
                    peers.push(SocketAddr::new(IpAddr::V6(ip), port));
                }
            }
        }
    }

    Ok(AnnounceResponse {
        interval,
        peers,
        complete,
        incomplete,
        failure: None,
    })
}

fn url_encode_binary(data: &[u8]) -> String {
    let mut result = String::with_capacity(data.len() * 3);
    for &b in data {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(b as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}
