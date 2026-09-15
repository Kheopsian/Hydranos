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

/// Primary reqwest client — can optionally route through a SOCKS5 proxy
/// via env `TYPHON_ANNOUNCE_PROXY`. Lets us kill
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

#[derive(Debug)]
pub struct AnnounceResponse {
    pub interval: u32,
    /// BEP 3 `min interval`: the floor the tracker imposes. Below this it wants
    /// no request at all, forced re-announce included. Zero when the tracker
    /// did not state one.
    pub min_interval: u32,
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
        .user_agent(crate::config::user_agent());
    if let Some(px) = primary_proxy() {
        builder = builder.proxy(px.clone());
    }
    let client = builder
        .build()
        .map_err(|e| format!("http client: {}", fmt_err_chain(&e)))?;


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

    // Bencode spells it with a space. A tracker that omits it leaves us with
    // zero, which means "no floor stated" and not "no floor".
    let min_interval = dict
        .get("min interval")
        .and_then(|v| v.as_int())
        .unwrap_or(0)
        .max(0) as u32;

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
        min_interval,
        peers,
        complete,
        incomplete,
        failure: None,
    })
}

/// One announce over HTTP, with the transport this module already knows about:
/// the primary proxy, the timeout, and the bencode response.
///
/// The URL is built by the caller. Policy -- passkeys, client spoofing, the
/// `ip=` parameter, rate limiting -- belongs to the announcer, not here; this
/// only has to put a request on the wire and read the answer.
/// Which address families an announce is sent from.
///
/// libtorrent opens one listen socket per family and announces from each, with
/// the SAME peer id: BEP 7 describes one peer holding two addresses, not two
/// peers. A tracker that merges them lists us in `peers` and `peers6` both, so
/// an IPv4-only leecher can still reach us. That is the behaviour this mirrors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IpMode {
    /// Both families, one announce each. The default.
    Auto,
    V4,
    V6,
}

impl IpMode {
    pub fn parse(s: &str) -> IpMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "v4" | "ipv4" | "4" => IpMode::V4,
            "v6" | "ipv6" | "6" => IpMode::V6,
            _ => IpMode::Auto,
        }
    }
}

/// One client per address family, built once.
///
/// Binding the socket to the unspecified address of a family is what pins the
/// connection to it -- the equivalent of 3.x's `ipv4Network()`, which narrowed
/// the dial network before handing it to the Go transport.
///
/// Two things were wrong before this. `send_announce` built a whole
/// `Client` per announce -- a connection pool, a resolver and a fresh load of
/// the root certificate store, ninety times a second. And it constrained no
/// family at all, so happy eyeballs took IPv6 on every dual-stack tracker and
/// the tracker recorded only our v6 address. Verified from a VPN on 2026-09-08:
/// announcing as a leecher to a tracker we seed returned our
/// `[2a01:...]:16172` in `peers6` and nothing of ours in `peers`. Every
/// IPv4-only leecher in those swarms could not see us at all.
static ANNOUNCE_CLIENT_V4: OnceLock<reqwest::Client> = OnceLock::new();
static ANNOUNCE_CLIENT_V6: OnceLock<reqwest::Client> = OnceLock::new();

fn family_client(v6: bool) -> &'static reqwest::Client {
    let cell = if v6 { &ANNOUNCE_CLIENT_V6 } else { &ANNOUNCE_CLIENT_V4 };
    cell.get_or_init(|| {
        let bind = if v6 {
            std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
        } else {
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
        };
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .http1_only()
            .pool_max_idle_per_host(64)
            .local_address(bind);
        if let Some(px) = primary_proxy() {
            builder = builder.proxy(px.clone());
        }
        builder.build().unwrap_or_else(|e| {
            eprintln!("[tracker] announce client build failed ({e}), falling back to default");
            reqwest::Client::new()
        })
    })
}

/// One announce, from one family.
async fn send_announce_family(
    url: &str,
    user_agent: &str,
    v6: bool,
) -> Result<AnnounceResponse, String> {
    let resp = family_client(v6)
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .send()
        .await
        .map_err(|e| format!("http request: {}", fmt_err_chain(&e)))?;
    finish_announce(resp).await
}

/// Merge two answers about the same swarm.
///
/// The counts come from the tracker and are identical either way, so the v4
/// answer is the base and v6 only contributes peers the v4 list did not carry.
/// One family failing is not a failure: an A-only tracker has no v6 to reach
/// and a AAAA-only one has no v4, and both are normal.
fn merge_announce(
    v4: Result<AnnounceResponse, String>,
    v6: Result<AnnounceResponse, String>,
) -> Result<AnnounceResponse, String> {
    match (v4, v6) {
        (Ok(mut a), Ok(b)) => {
            for p in b.peers {
                if !a.peers.contains(&p) {
                    a.peers.push(p);
                }
            }
            Ok(a)
        }
        (Ok(a), Err(_)) => Ok(a),
        (Err(_), Ok(b)) => Ok(b),
        (Err(e4), Err(e6)) => Err(format!("v4: {e4} | v6: {e6}")),
    }
}

pub async fn send_announce(
    url: &str,
    user_agent: &str,
    mode: IpMode,
) -> Result<AnnounceResponse, String> {
    match mode {
        IpMode::V4 => return send_announce_family(url, user_agent, false).await,
        IpMode::V6 => return send_announce_family(url, user_agent, true).await,
        IpMode::Auto => {}
    }
    // Same peer id on both, as libtorrent does: one peer, two addresses.
    let (a, b) = tokio::join!(
        send_announce_family(url, user_agent, false),
        send_announce_family(url, user_agent, true),
    );
    return merge_announce(a, b);
}

/// Parse one tracker answer. Shared by both families.
async fn finish_announce(resp: reqwest::Response) -> Result<AnnounceResponse, String> {

    if !resp.status().is_success() {
        let st = resp.status();
        // Up to 200 characters of body: a tracker's own reason for a 403 or a
        // 502 is the only thing that tells an operator whether they are banned
        // or merely behind a broken CDN.
        let body = resp.text().await.unwrap_or_default();
        let snip: String = body.chars().take(200).collect();
        return Err(format!("http {}: {}", st, snip.trim()));
    }

    let body = resp
        .bytes()
        .await
        .map_err(|e| format!("http body: {}", fmt_err_chain(&e)))?;
    parse_announce_response(&body)
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

#[cfg(test)]
mod announce_wire_tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    struct FakeTracker {
        url: String,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// A tracker on loopback answering a canned bencoded body. Everything the
    /// announce path does is HTTP, so the honest fixture is a real server.
    async fn fake_tracker(body: &'static [u8]) -> FakeTracker {
        let app = Router::new().route("/announce", get(move || async move { body.to_vec() }));
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
        FakeTracker { url: format!("http://{addr}/announce"), _shutdown: tx }
    }

    /// A compact peer list is 6 bytes per peer: 4 of address, 2 of port, big
    /// endian. `d8:completei5e10:incompletei2e8:intervali1800e5:peers6:...e`
    const OK_BODY: &[u8] =
        b"d8:completei5e10:incompletei2e8:intervali1800e12:min intervali900e5:peers6:\x5d\xb8\xd8\x22\x1a\xe1e";

    /// ⭐⭐ `min interval` is the FLOOR the tracker imposes: below it, it wants
    /// no request at all, forced reannounce included. Never reading it is one
    /// of the four BEP defects found in September.
    #[tokio::test]
    async fn the_min_interval_the_tracker_states_is_read() {
        let t = fake_tracker(OK_BODY).await;
        let resp = announce(&t.url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, "started")
            .await
            .expect("the tracker answered");
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.min_interval, 900, "the floor is carried, not dropped");
    }

    /// The swarm counts and the compact peer list are decoded as BEP 3 spells
    /// them: 6 bytes a peer, port big-endian.
    #[tokio::test]
    async fn the_swarm_counts_and_the_compact_peers_are_decoded() {
        let t = fake_tracker(OK_BODY).await;
        let resp = announce(&t.url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, "")
            .await
            .expect("answered");
        assert_eq!(resp.complete, 5);
        assert_eq!(resp.incomplete, 2);
        assert_eq!(resp.peers.len(), 1, "got {:?}", resp.peers);
        assert_eq!(resp.peers[0].to_string(), "93.184.216.34:6881");
        assert!(resp.failure.is_none());
    }

    /// ⭐ A tracker refusing us answers 200 with a `failure reason`. Treating
    /// that as a success is how a torrent announces into the void forever.
    ///
    /// ⚠️ The bencode length is COMPUTED: "unregistered torrent pass" is 25
    /// bytes. Counting it by hand as 26 swallows the dict terminator and the
    /// parser then refuses the body for a reason unrelated to the test.
    #[tokio::test]
    async fn a_failure_reason_is_carried_rather_than_read_as_success() {
        const REASON: &str = "unregistered torrent pass";
        assert_eq!(REASON.len(), 25, "the fixture length is computed, not counted");
        const FAIL: &[u8] = b"d14:failure reason25:unregistered torrent passe";

        let t = fake_tracker(FAIL).await;
        let out = announce(&t.url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, "").await;

        // ⭐ A refusal comes back as an Err, not as an Ok carrying a failure:
        // the caller cannot mistake it for a successful announce with no peers.
        match out {
            Err(e) => assert!(e.contains(REASON), "the reason reaches the caller: {e}"),
            Ok(resp) => assert_eq!(
                resp.failure.as_deref(),
                Some(REASON),
                "if it is an Ok, the failure must be carried"
            ),
        }
    }

    /// A body that is not bencode is an error, not a silently empty swarm --
    /// an empty swarm looks exactly like a healthy tracker with no peers.
    #[tokio::test]
    async fn a_body_that_is_not_bencode_is_an_error() {
        const JUNK: &[u8] = b"<html>we moved</html>";
        let t = fake_tracker(JUNK).await;
        let out = announce(&t.url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, "").await;
        assert!(out.is_err(), "got {out:?}");
    }

    /// A tracker that is not there is an error the caller can class, not a
    /// panic and not an empty response.
    #[tokio::test]
    async fn a_tracker_that_is_not_there_is_an_error() {
        let out = announce(
            "http://127.0.0.1:1/announce",
            &[0xABu8; 20],
            &[0xCDu8; 20],
            16371,
            0,
            0,
            0,
            "",
        )
        .await;
        assert!(out.is_err(), "got {out:?}");
    }

    /// ⭐⭐ The info hash and peer id are RAW BYTES in the query string, each
    /// escaped byte by byte. Encoding them as UTF-8 text mangles every byte
    /// above 0x7F, and the tracker then looks up a torrent nobody has.
    #[test]
    fn binary_values_are_percent_encoded_byte_by_byte() {
        let raw = [0x00u8, 0x41, 0x7f, 0x80, 0xff];
        let out = url_encode_binary(&raw);
        assert!(out.contains("%00"), "got {out}");
        assert!(out.contains("%80"), "a high byte is escaped, not re-encoded: {out}");
        assert!(out.contains("%FF") || out.contains("%ff"), "got {out}");
        assert!(out.contains('A'), "an unreserved byte stays literal: {out}");
    }

    #[test]
    fn the_unreserved_set_is_left_literal() {
        let raw = b"AZaz09-_.~";
        assert_eq!(url_encode_binary(raw), "AZaz09-_.~");
    }

    /// A 20-byte hash always encodes to something a tracker accepts, whatever
    /// the bytes are.
    #[test]
    fn any_twenty_byte_hash_encodes_without_losing_a_byte() {
        let mut hash = [0u8; 20];
        for (i, b) in hash.iter_mut().enumerate() {
            *b = (i * 13) as u8;
        }
        let out = url_encode_binary(&hash);
        assert!(!out.is_empty());
        assert!(!out.contains(' '), "a space would break the query: {out}");
        assert!(!out.contains('&'), "an ampersand would break the query: {out}");
    }

    /// The response parser is what every announce goes through; a missing
    /// `min interval` is zero rather than a parse failure, because most
    /// trackers do not state one.
    #[test]
    fn a_response_without_a_min_interval_parses_with_zero() {
        let body = b"d8:completei1e10:incompletei0e8:intervali1800e5:peers0:e";
        let resp = parse_announce_response(body).expect("a valid response");
        assert_eq!(resp.interval, 1800);
        assert_eq!(resp.min_interval, 0, "absent means no floor, not a failure");
        assert!(resp.peers.is_empty());
    }

    /// A peer list whose length is not a multiple of 6 is malformed. Taking
    /// the prefix would hand back a peer built from another peer's bytes.
    #[test]
    fn a_truncated_compact_peer_list_does_not_invent_a_peer() {
        let body = b"d8:intervali1800e5:peers4:\x5d\xb8\xd8\x22e";
        match parse_announce_response(body) {
            Ok(resp) => assert!(resp.peers.is_empty(), "no peer invented: {:?}", resp.peers),
            Err(_) => {}
        }
    }

    #[test]
    fn an_empty_body_is_a_parse_error() {
        assert!(parse_announce_response(b"").is_err());
        assert!(parse_announce_response(b"not bencode").is_err());
    }
}
