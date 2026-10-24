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

/// The process-wide announce proxy, `TYPHON_ANNOUNCE_PROXY`, read once.
///
/// Kept as a FALLBACK only: an engine's own `announce_proxy` (or its SOCKS5
/// peer proxy) wins, and this applies to an engine that has neither. It
/// predates the per-engine key and still sits in the environment of
/// installs that relied on it; dropping it would turn their proxied
/// announces into direct ones at the upgrade, silently -- the one failure a
/// proxy setting must never have. One process can carry several engines,
/// which is why it cannot be more than a fallback any more.
fn env_proxy() -> Option<&'static str> {
    static ENV: OnceLock<Option<String>> = OnceLock::new();
    ENV.get_or_init(|| {
        std::env::var("TYPHON_ANNOUNCE_PROXY")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    })
    .as_deref()
}

/// `TYPHON_ANNOUNCE_PROXY`, as the environment set it. For the Network tab.
pub fn env_announce_proxy() -> Option<&'static str> {
    env_proxy()
}

/// The proxy one engine's HTTP traffic goes through: what the engine was
/// configured with (`EngineConfig::http_proxy`), else the environment
/// fallback, else none.
pub fn effective_proxy(configured: &str) -> Option<String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(configured.to_string());
    }
    env_proxy().map(str::to_string)
}

/// A proxy URL without its credentials, for a log line or an error.
pub fn redact_proxy(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &url[..scheme], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

/// The reqwest proxy for an engine, or None for a direct engine.
///
/// A URL that does not parse is an ERROR, not "no proxy": the operator asked
/// for one, and announcing directly instead would publish exactly the address
/// they set it up to hide.
pub(crate) fn proxy_for(configured: &str) -> Result<Option<reqwest::Proxy>, String> {
    let Some(url) = effective_proxy(configured) else {
        return Ok(None);
    };
    reqwest::Proxy::all(&url)
        .map(Some)
        .map_err(|e| format!("announce proxy {} is not usable: {}", redact_proxy(&url), e))
}

/// Whether this engine's announces go through a proxy. A UDP announce
/// cannot, and must not go out beside it.
pub(crate) fn announces_proxied(configured: &str) -> bool {
    effective_proxy(configured).is_some()
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
    /// BEP 3 `tracker id`: a token the tracker wants echoed back as
    /// `trackerid=` on every later announce to it. None when it sent none.
    pub tracker_id: Option<String>,
    /// `warning message`: the tracker accepted the announce but has something
    /// to say. Not an error -- the peers and the interval still count.
    pub warning: Option<String>,
}

/// How long a tracker asked us to stay away, when it said so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryHint {
    /// `retry in` minutes (BEP 31) or an HTTP `Retry-After` in seconds.
    After(std::time::Duration),
    /// BEP 31 `retry in: "never"`: do not announce to this tracker again.
    Never,
}

/// Longest wait a tracker can impose through a hint. A day is already far
/// past any interval a tracker uses; a larger number is a unit mistake on the
/// tracker's side and would park the torrent for good.
const MAX_RETRY_HINT: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// The retry hint carried by an announce error, if any.
///
/// Errors cross the crate boundary as strings -- the runner classifies them,
/// redacts them and shows them -- so the hint travels inside the message as a
/// bracketed marker this function is the only reader of.
pub fn retry_hint(err: &str) -> Option<RetryHint> {
    if err.contains("[retry-in never]") {
        return Some(RetryHint::Never);
    }
    for (tag, unit) in [("[retry-in ", 60u64), ("[retry-after ", 1u64)] {
        if let Some(i) = err.find(tag) {
            let rest = &err[i + tag.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<u64>() {
                let d = std::time::Duration::from_secs(n.saturating_mul(unit));
                return Some(RetryHint::After(d.min(MAX_RETRY_HINT)));
            }
        }
    }
    None
}

/// The `key` announce parameter for a peer id on one torrent.
///
/// `key` lets a tracker recognise a peer whose address changed, which only
/// works if nobody else can produce it. Hashing the peer id alone would make
/// it public: every peer we connect to reads our peer id in the handshake. So
/// the hash is salted with a secret drawn once per process -- stable for the
/// whole session, unguessable from anything we put on the wire.
///
/// The info hash is part of the input: a tracker only compares keys within a
/// torrent, so one key per torrent loses nothing, and the same value does not
/// tie every torrent we announce together.
pub fn announce_key(peer_id: &[u8], info_hash: &[u8; 20]) -> String {
    static SALT: OnceLock<u64> = OnceLock::new();
    key_with_salt(*SALT.get_or_init(rand::random::<u64>), peer_id, info_hash)
}

fn key_with_salt(salt: u64, peer_id: &[u8], info_hash: &[u8; 20]) -> String {
    // FNV-1a, 32 bits. Opaque, stable, and cheap; secrecy comes from the salt.
    let mut h: u32 = 0x811c_9dc5;
    for b in salt
        .to_le_bytes()
        .iter()
        .chain(peer_id.iter())
        .chain(info_hash.iter())
    {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
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
    announce_on(
        tracker_url,
        info_hash,
        peer_id,
        port,
        uploaded,
        downloaded,
        left,
        event,
        "",
        "",
    )
    .await
}

/// Pin a client builder to an interface, or refuse where that is impossible.
pub(crate) fn pin_builder(
    builder: reqwest::ClientBuilder,
    device: &str,
) -> Result<reqwest::ClientBuilder, String> {
    let device = device.trim();
    if device.is_empty() {
        return Ok(builder);
    }
    // Linux only, macOS included in the refusal although reqwest could pin
    // an announce there: the engine's TCP peers cannot follow
    // (`netpin::DEVICE_PIN_SUPPORTED`), and an engine whose announces leave
    // by the tunnel while its peers leave by the default route hands the
    // tracker one address and the swarm another.
    #[cfg(target_os = "linux")]
    {
        // The tracker's NAME too: in a managed tunnel it is asked of the
        // tunnel's DNS server through the tunnel, not of the host's resolver
        // (`tunneldns`). Through a proxy it is never resolved here at all.
        Ok(builder
            .interface(device)
            .dns_resolver(crate::tunneldns::Resolver::new(device)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = builder;
        Err(format!(
            "bind_interface {device:?}: {}",
            crate::netpin::UNSUPPORTED
        ))
    }
}

/// `announce` from an interface, through a proxy. Empty device = the default
/// route; empty proxy = the `TYPHON_ANNOUNCE_PROXY` fallback, or direct.
#[allow(clippy::too_many_arguments)]
pub async fn announce_on(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    uploaded: u64,
    downloaded: u64,
    left: u64,
    event: &str,
    device: &str,
    proxy: &str,
) -> Result<AnnounceResponse, String> {
    announce_on_mode(
        tracker_url,
        info_hash,
        peer_id,
        port,
        uploaded,
        downloaded,
        left,
        event,
        device,
        proxy,
        IpMode::Auto,
    )
    .await
}

/// The local address that pins a direct announce to one family, none for
/// `Auto` (one request, the resolver's choice) and none through a proxy (the
/// socket goes to the proxy, whose family says nothing about ours).
pub(crate) fn family_bind(mode: IpMode, proxied: bool) -> Option<std::net::IpAddr> {
    if proxied {
        return None;
    }
    match mode {
        IpMode::V4 => Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
        IpMode::V6 => Some(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
        IpMode::Auto => None,
    }
}

/// `announce_on` held to one address family (`IpMode::Auto` = no hold).
#[allow(clippy::too_many_arguments)]
pub async fn announce_on_mode(
    tracker_url: &str,
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    port: u16,
    uploaded: u64,
    downloaded: u64,
    left: u64,
    event: &str,
    device: &str,
    proxy: &str,
    mode: IpMode,
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
        numwant={}&\
        key={}\
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
        announce_key(peer_id, info_hash),
        if event.is_empty() {
            String::new()
        } else {
            format!("&event={}", event)
        },
    );

    // HTTP GET with timeout, through the engine's proxy when it has one.
    let mut builder = pin_builder(
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            // Same rule as `family_client`: the URL carries the passkey, a
            // forged Location must not replay it to another host.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(crate::config::user_agent()),
        device,
    )?;
    let px = proxy_for(proxy)?;
    if let Some(bind) = family_bind(mode, px.is_some()) {
        builder = builder.local_address(bind);
    }
    if let Some(px) = px {
        builder = builder.proxy(px);
    }
    let client = builder
        .build()
        .map_err(|e| format!("http client: {}", fmt_err_chain(&e)))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("http request: {}", fmt_err_chain(&e.without_url())))?;

    if !resp.status().is_success() {
        return Err(http_error(resp).await);
    }

    let body = read_capped(resp, MAX_TRACKER_BODY)
        .await
        .map_err(|e| format!("http body: {e}"))?;

    // Parse bencoded response
    parse_announce_response(&body)
}

/// The most of a tracker answer (announce or scrape) that is read. A compact
/// answer for 200 peers is 1.2 KB, a scrape of `scrape::BATCH` torrents a few;
/// a megabyte is a tracker sending something else.
pub(crate) const MAX_TRACKER_BODY: usize = 1 << 20;

/// The most peers taken from one answer. We ask for 200; the rest of a longer
/// list would only queue dials nobody asked for.
pub(crate) const MAX_PEERS_PER_ANSWER: usize = 1000;

/// The body of `resp`, refused past `max` bytes. Read chunk by chunk, so a
/// server streaming gigabytes costs `max` bytes of memory and not gigabytes.
pub(crate) async fn read_capped(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, String> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(format!("larger than {max} bytes"));
    }
    let (body, more) = read_prefix(resp, max).await?;
    if more {
        return Err(format!("larger than {max} bytes"));
    }
    Ok(body)
}

/// At most `max` bytes of `resp`'s body, and whether there was more.
pub(crate) async fn read_prefix(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<(Vec<u8>, bool), String> {
    let mut out = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| fmt_err_chain(&e.without_url()))?
    {
        let room = max - out.len();
        if chunk.len() > room {
            out.extend_from_slice(&chunk[..room]);
            return Ok((out, true));
        }
        out.extend_from_slice(&chunk);
    }
    Ok((out, false))
}

fn parse_announce_response(data: &[u8]) -> Result<AnnounceResponse, String> {
    let value = bencode_decode(data)?;
    let dict = value.as_dict().ok_or("response not a dict")?;

    // BEP 3: a `failure reason` means the announce failed, whatever else the
    // dictionary holds -- and whatever type the value has. A tracker that
    // sends it as an integer or raw bytes is still refusing us.
    if let Some(reason) = dict.get("failure reason") {
        let msg = text_of(reason).unwrap_or_else(|| "(unreadable failure reason)".into());
        // BEP 31: a refusal may say when to come back, or never to.
        let hint = match dict.get("retry in") {
            Some(v) if v.as_int().is_some() => {
                format!(" [retry-in {}m]", v.as_int().unwrap_or(0).max(0))
            }
            Some(v) if text_of(v).as_deref() == Some("never") => " [retry-in never]".to_string(),
            _ => String::new(),
        };
        return Err(format!("tracker: {}{}", msg, hint));
    }

    // `interval` is the tracker's; we only refuse values that cannot be one.
    // Zero or negative is no interval at all, and a negative integer cast to
    // u32 would have become a four-billion-second wait.
    let interval = match dict.get("interval").and_then(|v| v.as_int()) {
        Some(n) if n > 0 => n.min(MAX_INTERVAL) as u32,
        _ => DEFAULT_INTERVAL,
    };

    // Bencode spells it with a space. A tracker that omits it leaves us with
    // zero, which means "no floor stated" and not "no floor".
    let min_interval = dict
        .get("min interval")
        .and_then(|v| v.as_int())
        .unwrap_or(0)
        .clamp(0, MAX_INTERVAL) as u32;

    // Counts are displayed, never trusted for anything else; a negative one is
    // a tracker bug and reads as zero rather than wrapping to four billion.
    let count = |k: &str| {
        dict.get(k)
            .and_then(|v| v.as_int())
            .unwrap_or(0)
            .clamp(0, u32::MAX as i64) as u32
    };
    let complete = count("complete");
    let incomplete = count("incomplete");

    let tracker_id = dict
        .get("tracker id")
        .and_then(text_of)
        .filter(|s| !s.is_empty());
    let warning = dict
        .get("warning message")
        .and_then(text_of)
        .filter(|s| !s.is_empty());

    // Parse compact peers (6 bytes each: 4 IP + 2 port)
    let mut peers = Vec::new();
    if let Some(peers_val) = dict.get("peers") {
        if let Some(compact) = peers_val.as_bytes() {
            // Compact format
            for chunk in compact.chunks(6) {
                if chunk.len() == 6 {
                    let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let port = u16::from_be_bytes([chunk[4], chunk[5]]);
                    // Port 0 is not a listening peer; dialling it is a
                    // connection attempt to nothing.
                    if port != 0 && peers.len() < MAX_PEERS_PER_ANSWER {
                        peers.push(SocketAddr::new(IpAddr::V4(ip), port));
                    }
                }
            }
        } else if let Some(peer_list) = peers_val.as_list() {
            // Dict format
            for p in peer_list {
                if let Some(pd) = p.as_dict() {
                    let ip_str = pd.get("ip").and_then(|v| v.as_string()).unwrap_or("");
                    // A port outside 1..=65535 is malformed; `as u16` would
                    // have wrapped it into somebody else's port.
                    let port = match pd.get("port").and_then(|v| v.as_int()) {
                        Some(p) if (1..=65535).contains(&p) => p as u16,
                        _ => continue,
                    };
                    if peers.len() >= MAX_PEERS_PER_ANSWER {
                        break;
                    }
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
                    if port != 0 && peers.len() < MAX_PEERS_PER_ANSWER {
                        peers.push(SocketAddr::new(IpAddr::V6(ip), port));
                    }
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
        tracker_id,
        warning,
    })
}

/// What a tracker says when it does not answer `interval`: libtorrent's and
/// qBittorrent's default.
pub(crate) const DEFAULT_INTERVAL: u32 = 1800;
/// A week. Above that the number is a unit mistake, not an interval.
pub(crate) const MAX_INTERVAL: i64 = 7 * 24 * 3600;

/// A bencoded value read as text, whether a UTF-8 string or raw bytes.
fn text_of(v: &BencodeValue) -> Option<String> {
    if let Some(s) = v.as_string() {
        return Some(s.to_string());
    }
    if let Some(b) = v.as_bytes() {
        return Some(String::from_utf8_lossy(b).into_owned());
    }
    v.as_int().map(|n| n.to_string())
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
/// One client per (interface, family), built once each.
///
/// Keyed by the engine's `bind_interface` as well: 4.3 had one client per
/// family for the whole process, so every engine's announces left by the
/// default route whatever interface the engine was pinned to -- the tracker
/// saw the host's address, not the tunnel's. A pinned client binds its
/// sockets to the device (SO_BINDTODEVICE), so a tunnel that is down makes
/// the announce FAIL rather than leave another way.
///
/// And by proxy: two engines on one interface can still be sent out two
/// different ways, and a client cached for one must never carry the other's
/// announces -- a direct engine's client handed to a proxied one is the
/// home address published by a cache hit.
static ANNOUNCE_CLIENTS: OnceLock<
    std::sync::Mutex<std::collections::HashMap<(String, String, bool), reqwest::Client>>,
> = OnceLock::new();

fn family_client(v6: bool, device: &str, proxy: &str) -> Result<reqwest::Client, String> {
    let device = device.trim();
    let proxy = proxy.trim();
    let key = (device.to_string(), proxy.to_string(), v6);
    let map = ANNOUNCE_CLIENTS.get_or_init(Default::default);
    let mut map = map.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(c) = map.get(&key) {
        return Ok(c.clone());
    }
    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .http1_only()
        // Never follow a redirect: the announce URL carries the passkey, and a
        // tracker (or a MITM on an http hop) that answers 30x with a forged
        // Location would otherwise have reqwest replay the whole URL -- passkey
        // included -- to an attacker-chosen host.
        .redirect(reqwest::redirect::Policy::none())
        .pool_max_idle_per_host(64);
    match proxy_for(proxy)? {
        // Through a proxy, no family bind: the socket we open goes to the
        // PROXY, and the tracker sees the proxy's exit whatever family that
        // socket is. Binding it to v4 would only fail against a v6 proxy.
        Some(px) => builder = builder.proxy(px),
        None => {
            let bind = if v6 {
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
            } else {
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
            };
            builder = builder.local_address(bind);
        }
    }
    // No per-socket interface pin on some platforms: refused there rather
    // than announcing from the default route while the operator believes the
    // engine is pinned. With a proxy, it pins the connection to the proxy.
    builder = pin_builder(builder, device)?;
    let client = builder
        .build()
        .map_err(|e| format!("announce client for {device:?}: {e}"))?;
    map.insert(key, client.clone());
    Ok(client)
}

/// One announce, from one family.
async fn send_announce_family(
    url: &str,
    user_agent: &str,
    v6: bool,
    device: &str,
    proxy: &str,
) -> Result<AnnounceResponse, String> {
    let resp = family_client(v6, device, proxy)?
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
pub(crate) fn merge_announce(
    v4: Result<AnnounceResponse, String>,
    v6: Result<AnnounceResponse, String>,
) -> Result<AnnounceResponse, String> {
    match (v4, v6) {
        (Ok(mut a), Ok(b)) => {
            let mut seen: std::collections::HashSet<SocketAddr> = a.peers.iter().copied().collect();
            for p in b.peers {
                if a.peers.len() >= MAX_PEERS_PER_ANSWER {
                    break;
                }
                if seen.insert(p) {
                    a.peers.push(p);
                }
            }
            if a.tracker_id.is_none() {
                a.tracker_id = b.tracker_id;
            }
            if a.warning.is_none() {
                a.warning = b.warning;
            }
            // The stricter floor of the two: both answers speak for the same
            // tracker, and honouring the shorter one would undercut the other.
            a.min_interval = a.min_interval.max(b.min_interval);
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
    send_announce_on(url, user_agent, mode, "", "").await
}

/// `send_announce` from the engine's interface and through its proxy. Empty
/// device = the default route; empty proxy = the `TYPHON_ANNOUNCE_PROXY`
/// fallback, or direct.
pub async fn send_announce_on(
    url: &str,
    user_agent: &str,
    mode: IpMode,
    device: &str,
    proxy: &str,
) -> Result<AnnounceResponse, String> {
    // Through a proxy there is ONE announce. The tracker sees the proxy's
    // exit, whatever family we reach the proxy by, so a second request from
    // the other family would only report the same address twice -- and fail
    // outright when the proxy listens on one family only.
    if announces_proxied(proxy) {
        return send_announce_family(url, user_agent, false, device, proxy).await;
    }
    match mode {
        IpMode::V4 => return send_announce_family(url, user_agent, false, device, proxy).await,
        IpMode::V6 => return send_announce_family(url, user_agent, true, device, proxy).await,
        IpMode::Auto => {}
    }
    // Same peer id on both, as libtorrent does: one peer, two addresses.
    let (a, b) = tokio::join!(
        send_announce_family(url, user_agent, false, device, proxy),
        send_announce_family(url, user_agent, true, device, proxy),
    );
    merge_announce(a, b)
}

/// The error for a non-2xx answer.
///
/// Up to 200 characters of body: a tracker's own reason for a 403 or a 502 is
/// the only thing that tells an operator whether they are banned or merely
/// behind a broken CDN. A `Retry-After` in seconds (RFC 9110) is carried as a
/// marker, so the runner waits as long as the tracker asked rather than as
/// long as it guesses.
async fn http_error(resp: reqwest::Response) -> String {
    let st = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    // 200 characters are at most 800 bytes of UTF-8: nothing past that is read.
    let (body, _) = read_prefix(resp, 800).await.unwrap_or_default();
    let snip: String = String::from_utf8_lossy(&body).chars().take(200).collect();
    match retry_after {
        Some(secs) => format!("http {}: {} [retry-after {}s]", st, snip.trim(), secs),
        None => format!("http {}: {}", st, snip.trim()),
    }
}

/// Parse one tracker answer. Shared by both families.
async fn finish_announce(resp: reqwest::Response) -> Result<AnnounceResponse, String> {
    if !resp.status().is_success() {
        return Err(http_error(resp).await);
    }

    let body = read_capped(resp, MAX_TRACKER_BODY)
        .await
        .map_err(|e| format!("http body: {e}"))?;
    parse_announce_response(&body)
}

pub(crate) fn url_encode_binary(data: &[u8]) -> String {
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

/// An HTTP server that never stops talking, for the tests of what a client
/// reads from a hostile one.
#[cfg(test)]
pub(crate) mod endless {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Answers every connection with `status` (e.g. `"200 OK"`), no length,
    /// `prefix`, then `fill` bytes until the client hangs up. Returns its base
    /// URL and the count of bytes written, all connections together.
    pub(crate) async fn server(
        status: &'static str,
        prefix: Vec<u8>,
        fill: u8,
    ) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let written = Arc::new(AtomicUsize::new(0));
        let w = written.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let (prefix, w) = (prefix.clone(), w.clone());
                tokio::spawn(async move {
                    let mut req = [0u8; 4096];
                    let _ = s.read(&mut req).await;
                    let head = format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n");
                    if s.write_all(head.as_bytes()).await.is_err()
                        || s.write_all(&prefix).await.is_err()
                    {
                        return;
                    }
                    let block = vec![fill; 64 * 1024];
                    while s.write_all(&block).await.is_ok() {
                        w.fetch_add(block.len(), Ordering::Relaxed);
                    }
                });
            }
        });
        (format!("http://{addr}"), written)
    }
}

#[cfg(test)]
mod announce_wire_tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;

    /// A compact list of `n` distinct peers, as a tracker would send it.
    fn compact_peers(n: usize) -> Vec<u8> {
        let mut out = format!("d8:intervali1800e5:peers{}:", n * 6).into_bytes();
        for i in 0..n {
            out.extend_from_slice(&[10, (i >> 16) as u8, (i >> 8) as u8, i as u8, 0x1a, 0xe1]);
        }
        out.push(b'e');
        out
    }

    /// ⭐ A tracker that keeps streaming is cut off at the cap, not read into
    /// memory to the end of the timeout. The answer it sends is a valid one
    /// that never ends: the peer list's declared length is ten megabytes, and
    /// a client reading it whole would return two million peers.
    #[tokio::test]
    async fn an_announce_answer_past_the_cap_is_refused_unread() {
        for v6_path in [false, true] {
            let (base, written) = endless::server(
                "200 OK",
                b"d8:intervali1800e5:peers12000000:".to_vec(),
                0x01,
            )
            .await;
            let url = format!("{base}/announce");
            let out = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                if v6_path {
                    send_announce(&url, "t", IpMode::V4).await
                } else {
                    announce(&url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, "").await
                }
            })
            .await
            .expect("the client gave up on its own, before the test's timeout");
            let e = out.expect_err("a body this size is refused");
            assert!(e.contains("larger than"), "refused for its size: {e}");
            let w = written.load(std::sync::atomic::Ordering::Relaxed);
            assert!(w < 32 << 20, "{w} bytes went out before the client hung up");
        }
    }

    /// One answer yields at most a thousand peers (`MAX_PEERS_PER_ANSWER`),
    /// whatever the tracker lists.
    #[tokio::test]
    async fn one_answer_yields_at_most_a_thousand_peers() {
        let body: &'static [u8] = Box::leak(compact_peers(5000).into_boxed_slice());
        let t = fake_tracker(body).await;
        let resp = announce(&t.url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 1, "")
            .await
            .expect("answered");
        assert_eq!(resp.peers.len(), 1000);
    }

    /// The v4 and v6 answers, each under the cap, merged stay under it too,
    /// without a quadratic pass over the two lists.
    #[test]
    fn a_merged_answer_stays_under_the_peer_cap() {
        let one = |base: u8| AnnounceResponse {
            interval: 1800,
            min_interval: 0,
            peers: (0..800u32)
                .map(|i| {
                    SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(
                            base,
                            (i >> 16) as u8,
                            (i >> 8) as u8,
                            i as u8,
                        )),
                        6881,
                    )
                })
                .collect(),
            complete: 0,
            incomplete: 0,
            failure: None,
            tracker_id: None,
            warning: None,
        };
        let merged = merge_announce(Ok(one(10)), Ok(one(11))).expect("both answered");
        assert_eq!(merged.peers.len(), 1000);
    }

    /// A refusal's body is quoted, 200 characters of it, and only that much
    /// is read: a tracker streaming an endless error page cannot hold the
    /// announce for the whole timeout.
    #[tokio::test]
    async fn an_endless_error_page_is_quoted_without_being_read_whole() {
        let (base, _) = endless::server("503 Service Unavailable", b"busy ".to_vec(), b'x').await;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            announce(
                &format!("{base}/announce"),
                &[0xABu8; 20],
                &[0xCDu8; 20],
                16371,
                0,
                0,
                0,
                "",
            ),
        )
        .await
        .expect("answered before the 15 s client timeout");
        let e = out.expect_err("a 503 is an error");
        assert!(
            e.contains("503") && e.contains("busy xxx"),
            "the start of the body is quoted: {e}"
        );
    }

    struct FakeTracker {
        url: String,
        _shutdown: tokio::sync::oneshot::Sender<()>,
    }

    /// A tracker on loopback answering a canned bencoded body. Everything the
    /// announce path does is HTTP, so the honest fixture is a real server.
    async fn fake_tracker(body: &'static [u8]) -> FakeTracker {
        let app = Router::new().route("/announce", get(move || async move { body.to_vec() }));
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
        FakeTracker {
            url: format!("http://{addr}/announce"),
            _shutdown: tx,
        }
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
        let resp = announce(
            &t.url,
            &[0xABu8; 20],
            &[0xCDu8; 20],
            16371,
            0,
            0,
            0,
            "started",
        )
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

    /// An engine in a managed tunnel asks the TUNNEL's DNS server for its
    /// tracker's name, from a socket pinned to the tunnel; the host's
    /// resolver never sees it. `lo` stands in for `wg-<engine>`.
    #[tokio::test]
    async fn a_tunnel_engine_resolves_its_tracker_through_the_tunnel_dns() {
        let _one = crate::tunneldns::tests::LO.lock().await;
        let t = fake_tracker(OK_BODY).await;
        let (dns, asked) = crate::tunneldns::tests::fake_server([127, 0, 0, 1]).await;
        crate::tunneldns::tests::register("lo", dns);
        let url = t.url.replace("127.0.0.1", "tracker.tunnel-dns.invalid");
        let resp = announce_on(
            &url,
            &[0xABu8; 20],
            &[0xCDu8; 20],
            16371,
            0,
            0,
            0,
            "",
            "lo",
            "",
        )
        .await;
        crate::tunneldns::forget("lo");
        let resp = resp.expect("resolved by the tunnel server, then announced on lo");
        assert_eq!(resp.complete, 5);
        assert!(asked
            .lock()
            .unwrap()
            .iter()
            .any(|n| n == "tracker.tunnel-dns.invalid"));
    }

    /// A direct announce held to a family binds that family's wildcard; a
    /// proxied one, or `Auto`, binds nothing.
    #[test]
    fn a_held_family_binds_its_wildcard_and_a_proxy_or_auto_binds_none() {
        assert_eq!(
            family_bind(IpMode::V4, false),
            Some("0.0.0.0".parse().unwrap())
        );
        assert_eq!(family_bind(IpMode::V6, false), Some("::".parse().unwrap()));
        assert_eq!(family_bind(IpMode::Auto, false), None);
        assert_eq!(family_bind(IpMode::V4, true), None);
    }

    /// The hold is real: a v6-held announce cannot reach a v4-only tracker.
    #[tokio::test]
    async fn a_v6_held_announce_does_not_reach_a_v4_tracker() {
        let t = fake_tracker(OK_BODY).await;
        let ih = [0xABu8; 20];
        let ask = |m| announce_on_mode(&t.url, &ih, &[0xCDu8; 20], 16371, 0, 0, 0, "", "", "", m);
        ask(IpMode::V4).await.expect("v4 reaches a v4 tracker");
        ask(IpMode::V6)
            .await
            .expect_err("a v6-bound socket cannot dial v4");
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
        assert_eq!(
            REASON.len(),
            25,
            "the fixture length is computed, not counted"
        );
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
        assert!(
            out.contains("%80"),
            "a high byte is escaped, not re-encoded: {out}"
        );
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
        assert!(
            !out.contains('&'),
            "an ampersand would break the query: {out}"
        );
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

    /// BEP 3: a `tracker id` is kept so it can be echoed back as `trackerid=`.
    #[test]
    fn a_tracker_id_is_kept() {
        let body = b"d8:intervali1800e5:peers0:10:tracker id6:abc123e";
        let resp = parse_announce_response(body).expect("valid");
        assert_eq!(resp.tracker_id.as_deref(), Some("abc123"));
        let none = parse_announce_response(b"d8:intervali1800e5:peers0:e").unwrap();
        assert_eq!(none.tracker_id, None, "absent is absent, not empty");
    }

    /// A `warning message` is an accepted announce with something to say:
    /// the peers and the interval still count.
    #[test]
    fn a_warning_is_carried_and_the_answer_still_counts() {
        let msg = "client is outdated";
        let body = format!(
            "d8:intervali900e5:peers0:15:warning message{}:{}e",
            msg.len(),
            msg
        );
        let resp = parse_announce_response(body.as_bytes()).expect("a warning is not a failure");
        assert_eq!(resp.warning.as_deref(), Some(msg));
        assert_eq!(resp.interval, 900);
    }

    /// BEP 31: a refusal can say when to come back, in minutes, or never.
    #[test]
    fn bep31_retry_in_is_read_from_a_refusal() {
        let err = parse_announce_response(b"d14:failure reason4:busy8:retry ini5ee").unwrap_err();
        assert_eq!(
            retry_hint(&err),
            Some(RetryHint::After(std::time::Duration::from_secs(300))),
            "{err}"
        );
        let never =
            parse_announce_response(b"d14:failure reason6:banned8:retry in5:nevere").unwrap_err();
        assert_eq!(retry_hint(&never), Some(RetryHint::Never), "{never}");
        let plain = parse_announce_response(b"d14:failure reason4:nopee").unwrap_err();
        assert_eq!(retry_hint(&plain), None, "no hint, no invented wait");
    }

    /// A hint is capped: a tracker that means minutes and writes seconds must
    /// not park a torrent for months.
    #[test]
    fn a_retry_hint_is_capped_at_a_day() {
        assert_eq!(
            retry_hint("tracker: x [retry-in 999999999m]"),
            Some(RetryHint::After(MAX_RETRY_HINT))
        );
        assert_eq!(
            retry_hint("http 429: slow down [retry-after 120s]"),
            Some(RetryHint::After(std::time::Duration::from_secs(120)))
        );
    }

    /// A `failure reason` that is not a string is still a refusal.
    #[test]
    fn a_failure_reason_of_any_type_is_a_refusal() {
        assert!(parse_announce_response(b"d14:failure reasoni42e8:intervali1800ee").is_err());
    }

    /// Values that cannot be what they claim: a negative interval is no
    /// interval, a negative count is zero -- never a wrapped u32.
    #[test]
    fn impossible_values_are_not_wrapped_into_huge_ones() {
        let body = b"d8:completei-3e10:incompletei-1e8:intervali-60e12:min intervali-5e5:peers0:e";
        let resp = parse_announce_response(body).expect("parses");
        assert_eq!(
            resp.interval, 1800,
            "a negative interval falls back to the default"
        );
        assert_eq!(resp.min_interval, 0);
        assert_eq!((resp.complete, resp.incomplete), (0, 0));
    }

    /// A peer on port 0 is not listening; the dictionary form's out-of-range
    /// port must not wrap onto somebody else's.
    #[test]
    fn a_peer_on_an_impossible_port_is_dropped() {
        let compact =
            b"d8:intervali1800e5:peers12:\x0a\x00\x00\x01\x00\x00\x0a\x00\x00\x02\x1a\xe1e";
        let resp = parse_announce_response(compact).unwrap();
        assert_eq!(resp.peers.len(), 1, "{:?}", resp.peers);
        assert_eq!(resp.peers[0].port(), 6881);
        let dict =
            b"d8:intervali1800e5:peersld2:ip8:10.0.0.14:porti70000eed2:ip8:10.0.0.24:porti6881eeee";
        let resp = parse_announce_response(dict).unwrap();
        assert_eq!(resp.peers, vec!["10.0.0.2:6881".parse().unwrap()]);
    }

    /// `key` is stable for the process and cannot be computed from the peer id
    /// alone -- every peer sees our peer id in the handshake.
    #[test]
    fn the_key_is_stable_and_not_derivable_from_the_peer_id() {
        let pid = b"-HY4240-abcdefghijkl";
        let ih = [7u8; 20];
        assert_eq!(
            announce_key(pid, &ih),
            announce_key(pid, &ih),
            "stable for the session"
        );
        assert_ne!(
            key_with_salt(1, pid, &ih),
            key_with_salt(2, pid, &ih),
            "the salt changes it"
        );
        let k = announce_key(pid, &ih);
        assert_eq!(k.len(), 8);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()), "{k}");
    }

    /// Both family answers speak for one tracker: the stricter floor and the
    /// tracker id survive the merge.
    #[test]
    fn the_merge_keeps_the_stricter_floor_and_the_tracker_id() {
        let mk = |min: u32, id: Option<&str>| AnnounceResponse {
            interval: 1800,
            min_interval: min,
            peers: vec![],
            complete: 0,
            incomplete: 0,
            failure: None,
            tracker_id: id.map(String::from),
            warning: None,
        };
        let m = merge_announce(Ok(mk(60, None)), Ok(mk(300, Some("t")))).unwrap();
        assert_eq!(m.min_interval, 300);
        assert_eq!(m.tracker_id.as_deref(), Some("t"));
    }
}

/// A tracker that answers with a redirect to a second listener, and that
/// second listener, which counts who reaches it. For the tests of what a
/// client does with a `Location` it was handed.
#[cfg(test)]
pub(crate) mod redirect_trap {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) struct Trap {
        /// The first listener, base URL: it always answers with the redirect.
        pub(crate) base: String,
        /// Requests the first listener received.
        pub(crate) asked: Arc<AtomicUsize>,
        /// Connections the target of the redirect received.
        pub(crate) reached: Arc<AtomicUsize>,
    }

    /// `status` is the redirect line (e.g. `"302 Found"`); the target answers
    /// 200 with `target_body`.
    pub(crate) async fn start(status: &'static str, target_body: &'static [u8]) -> Trap {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind target");
        let target_addr = target.local_addr().expect("addr");
        let reached = Arc::new(AtomicUsize::new(0));
        let r = reached.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = target.accept().await {
                r.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut req = [0u8; 4096];
                    let _ = s.read(&mut req).await;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        target_body.len()
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(target_body).await;
                });
            }
        });

        let front = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind front");
        let front_addr = front.local_addr().expect("addr");
        let asked = Arc::new(AtomicUsize::new(0));
        let a = asked.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = front.accept().await {
                a.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut req = [0u8; 4096];
                    let _ = s.read(&mut req).await;
                    let head = format!(
                        "HTTP/1.1 {status}\r\nLocation: http://{target_addr}/announce?passkey=SECRET\r\n\
                         Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(head.as_bytes()).await;
                });
            }
        });
        Trap {
            base: format!("http://{front_addr}"),
            asked,
            reached,
        }
    }
}

#[cfg(test)]
mod redirect_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    const ANSWER: &[u8] = b"d8:intervali1800e5:peers0:e";
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

    /// ⭐ The announce URL carries the passkey. A tracker (or anything on the
    /// path to it) that answers with a redirect must not get the client to
    /// replay that URL somewhere else: the redirect is an error, and the second
    /// listener is never contacted.
    #[tokio::test]
    async fn an_announce_does_not_follow_a_redirect() {
        for status in [
            "301 Moved Permanently",
            "302 Found",
            "307 Temporary Redirect",
            "308 Permanent Redirect",
        ] {
            let trap = redirect_trap::start(status, ANSWER).await;
            let url = format!("{}/announce?passkey=SECRET", trap.base);
            let out = tokio::time::timeout(
                DEADLINE,
                announce(&url, &[0xABu8; 20], &[0xCDu8; 20], 16371, 0, 0, 0, ""),
            )
            .await
            .expect("the client gave up on its own");
            assert_eq!(
                trap.reached.load(Ordering::SeqCst),
                0,
                "{status}: the redirect target was contacted"
            );
            assert!(
                out.is_err(),
                "{status}: a redirect is not an answer: {:?}",
                out.map(|r| r.interval)
            );
            assert_eq!(
                trap.asked.load(Ordering::SeqCst),
                1,
                "{status}: the tracker was asked once"
            );
        }
    }

    /// The same, on the per-family client the announcer uses.
    #[tokio::test]
    async fn a_family_announce_does_not_follow_a_redirect() {
        for status in ["302 Found", "307 Temporary Redirect"] {
            let trap = redirect_trap::start(status, ANSWER).await;
            let url = format!("{}/announce?passkey=SECRET", trap.base);
            let out = tokio::time::timeout(DEADLINE, send_announce(&url, "t", IpMode::V4))
                .await
                .expect("the client gave up on its own");
            assert_eq!(
                trap.reached.load(Ordering::SeqCst),
                0,
                "{status}: the redirect target was contacted"
            );
            assert!(
                out.is_err(),
                "{status}: a redirect is not an answer: {:?}",
                out.map(|r| r.interval)
            );
        }
    }
}
