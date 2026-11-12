//! What a tracker is told, and by whom.
//!
//! The transport lives in `typhon_engine::tracker::http`. This decides what
//! goes on the wire: whose passkey, which client we claim to be, which address
//! we ask peers to use. Splitting it that way is what lets the URL be built and
//! asserted in a unit test -- the string the tracker sees is the contract, and
//! it is checkable without a network.

use std::collections::BTreeMap;
use std::time::Duration;

use super::overrides::{longest_override_key, override_host};
use super::url::{self, Announce};

/// Everything the announcer knows about how to talk to trackers.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    /// host -> passkey, replacing the one in the tracker URL.
    pub passkeys: BTreeMap<String, String>,
    /// Our own peer id, used when no tracker asks for another.
    pub peer_id: String,
    pub user_agent: String,
    /// BEP-7 `ip=`. Empty when the source address is already right.
    pub public_ip: String,
    /// host -> "v4" | "v6" | "auto". Absent means auto: announce from both
    /// families, as libtorrent does. A tracker that overwrites instead of
    /// merging the two addresses should be pinned to one family here.
    pub ip_modes: BTreeMap<String, String>,
    /// Leave `udp://` trackers alone. Off by default: a tracker the torrent
    /// lists is a tracker it expects to be told about. Named for what it
    /// does when set, so `Policy::default()` cannot turn UDP off by accident.
    pub skip_udp: bool,
    /// The engine's `bind_interface`: announces leave by it or not at all.
    /// Empty = the default route.
    pub device: String,
    /// The engine's announce proxy URL (`EngineConfig::http_proxy`): its
    /// `announce_proxy`, else its SOCKS5 peer proxy. Empty = the process-wide
    /// `TYPHON_ANNOUNCE_PROXY` fallback, or direct. Per engine, like `device`:
    /// one engine can announce through a VPS while the other goes direct.
    pub proxy: String,
    /// The engine's `enable_ipv6 = false`: every tracker is announced over
    /// IPv4 only (4.3 announced `auto` trackers over both regardless). Named
    /// for what it does when set, like `skip_udp`, so a default policy keeps
    /// both families.
    pub no_ipv6: bool,
    /// How long a race retries a tracker that has not registered the torrent
    /// yet. Zero = the default window.
    pub registration_window: Duration,
    /// What to do with a passkey that would travel over plain `http://`.
    /// `Warn` by default: one log line per tracker host, the announce goes
    /// out unchanged.
    pub cleartext_passkey: CleartextPasskey,
}

/// `announce_cleartext_passkey`: the policy for an `http://` tracker whose
/// announce URL carries a passkey.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CleartextPasskey {
    /// Announce, and say so in the log once per tracker host.
    #[default]
    Warn,
    /// Announce, silently.
    Allow,
    /// Do not announce: the URL is not built and nothing is sent.
    Refuse,
}

impl CleartextPasskey {
    /// Unknown or empty values mean `Warn`, the behaviour before the option.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "refuse" => Self::Refuse,
            _ => Self::Warn,
        }
    }
}

/// Whether announcing to `tracker_url` would put a passkey on a plain-text
/// connection: an `http://` URL with a passkey configured for its host, or a
/// path segment that looks like a key (anywhere in the path), or a query
/// parameter whose value looks like a key (the name does not matter).
pub fn sends_passkey_in_clear(policy: &Policy, tracker_url: &str) -> bool {
    let is_http = tracker_url
        .get(..7)
        .map(|p| p.eq_ignore_ascii_case("http://"))
        .unwrap_or(false);
    if !is_http {
        return false;
    }
    if passkey_for(policy, tracker_url).is_some() {
        return true;
    }
    typhon_engine::tracker::carries_key_in_clear(tracker_url)
}

/// Whether the policy is `refuse` and this announce would carry a passkey in
/// clear: the one decision both `prepare` and the runner's error report use.
pub fn refused_for_cleartext(policy: &Policy, tracker_url: &str) -> bool {
    policy.cleartext_passkey == CleartextPasskey::Refuse
        && sends_passkey_in_clear(policy, tracker_url)
}

/// True the first time this host is seen, false after: the warning is once
/// per tracker host, not once per announce.
fn first_cleartext_note(host: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.to_string())
}

/// The passkey this tracker should be given, if it is not the one already in
/// the URL.
pub fn passkey_for<'a>(policy: &'a Policy, tracker_url: &str) -> Option<&'a str> {
    let host = override_host(tracker_url);
    let key = longest_override_key(&host, policy.passkeys.keys().map(|s| s.as_str()))?;
    policy.passkeys.get(key).map(|s| s.as_str())
}

/// Which address families to announce from for this tracker.
///
/// Auto by default, which is two announces with the SAME peer id -- one peer
/// with two addresses, per BEP 7. Pin a host to one family when the tracker
/// keys peers by id and overwrites rather than merging: the symptom is our
/// address appearing in one of `peers`/`peers6` and never the other.
pub fn ip_mode_for(policy: &Policy, tracker_url: &str) -> typhon_engine::tracker::http::IpMode {
    let host = override_host(tracker_url);
    match longest_override_key(&host, policy.ip_modes.keys().map(|s| s.as_str())) {
        Some(k) => typhon_engine::tracker::http::IpMode::parse(&policy.ip_modes[k]),
        None => typhon_engine::tracker::http::IpMode::Auto,
    }
}

/// Where the passkey sits in a tracker URL: the byte range of its value, or
/// `None` when no position can be identified (the URL then cannot take an
/// override, and `apply_passkey` leaves it as it is).
///
/// The rule, first match wins:
/// 1. a query parameter named `passkey` (any case): its value, whatever it
///    looks like. Only that name: another name is a guess at which parameter
///    is the credential, and a wrong guess overwrites something else;
/// 2. a path segment next to a segment named `announce` that looks like a key
///    (`tracker::looks_like_key`: 16+ characters of letters, digits, `-`, `_`,
///    with a digit and a letter): the one after `announce` first, else the one
///    before it (`/<key>/announce`);
/// 3. the last path segment, as 3.x did (`/announce/<key>`, `/<key>`), unless
///    it is empty or starts with `announce` (`/announce`, `/announce.php`).
///
/// The scheme does not matter: `udp://host:port/announce` has no key and
/// answers `None`.
pub fn passkey_slot(tracker_url: &str) -> Option<std::ops::Range<usize>> {
    let url = tracker_url;
    let auth = url.find("://").map(|i| i + 3).unwrap_or(0);
    let path_start = auth
        + url[auth..]
            .find(['/', '?', '#'])
            .unwrap_or(url.len() - auth);
    let end = url[path_start..]
        .find('#')
        .map(|i| path_start + i)
        .unwrap_or(url.len());
    let (path_end, query) = match url[path_start..end].find('?') {
        Some(i) => (path_start + i, Some(path_start + i + 1)),
        None => (end, None),
    };

    if let Some(q) = query {
        let mut off = q;
        for piece in url[q..end].split('&') {
            if let Some(eq) = piece.find('=') {
                if piece[..eq].eq_ignore_ascii_case("passkey") {
                    return Some(off + eq + 1..off + piece.len());
                }
            }
            off += piece.len() + 1;
        }
    }

    let path = &url[path_start..path_end];
    if !path.starts_with('/') {
        return None;
    }
    let mut segs: Vec<(std::ops::Range<usize>, &str)> = Vec::new();
    let mut off = path_start + 1;
    for seg in path[1..].split('/') {
        segs.push((off..off + seg.len(), seg));
        off += seg.len() + 1;
    }
    if let Some(i) = segs
        .iter()
        .position(|(_, s)| s.eq_ignore_ascii_case("announce"))
    {
        let after = segs
            .get(i + 1)
            .filter(|(_, s)| typhon_engine::tracker::looks_like_key(s));
        let before = i
            .checked_sub(1)
            .and_then(|b| segs.get(b))
            .filter(|(_, s)| typhon_engine::tracker::looks_like_key(s));
        if let Some((r, _)) = after.or(before) {
            return Some(r.clone());
        }
    }
    let (r, last) = segs.last()?;
    let is_announce = last
        .get(..8)
        .is_some_and(|p| p.eq_ignore_ascii_case("announce"));
    (!last.is_empty() && !is_announce).then(|| r.clone())
}

/// Rewrite the passkey of a tracker URL, where `passkey_slot` finds it.
/// A URL with no identifiable slot comes back unchanged.
pub fn apply_passkey(tracker_url: &str, passkey: &str) -> String {
    let mut out = tracker_url.to_string();
    if let Some(r) = passkey_slot(tracker_url) {
        out.replace_range(r, passkey);
    }
    out
}

/// Hosts that have an override which could not be placed in one of their URLs.
fn unplaced_hosts() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

/// Records that `host` has an override its URL cannot take; true the first
/// time, so the warning is once per host and not once per announce.
fn note_unplaced(host: &str) -> bool {
    unplaced_hosts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(host.to_string())
}

/// Whether an announce or scrape to `host` since start found an override it
/// could not place (the `passkey_unplaced` of `/api/trackers`).
pub fn passkey_unplaced(host: &str) -> bool {
    unplaced_hosts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(host)
}

/// The tracker URL with the host's passkey override applied. When an override
/// exists but the URL has no slot for it, the URL is returned as it was, the
/// host is remembered for the Trackers tab and a warning is logged once.
pub fn keyed_url(policy: &Policy, tracker_url: &str) -> String {
    let Some(k) = passkey_for(policy, tracker_url) else {
        return tracker_url.to_string();
    };
    if passkey_slot(tracker_url).is_none() {
        let host = override_host(tracker_url);
        if note_unplaced(&host) {
            // The host only: the URL may carry the passkey.
            tracing::warn!(tracker = %host, "a passkey is set for this host but its announce URL has no place for it (no passkey= parameter, no key beside /announce, no last path segment): torrents keep announcing with the key in their own URL");
        }
    }
    apply_passkey(tracker_url, k)
}

/// The URL for one announce, and the User-Agent to send it with.
pub struct Request {
    pub url: String,
    pub user_agent: String,
    /// Which families to announce from for this tracker.
    pub ip_mode: typhon_engine::tracker::http::IpMode,
    /// Set for a `udp://` tracker: the same announce as `url`, as BEP 15
    /// carries it. Built from the same inputs in the same call, so the two
    /// transports cannot disagree about what the tracker is told.
    pub udp: Option<typhon_engine::tracker::udp::UdpAnnounce>,
    /// The interface to announce from (the engine's `bind_interface`).
    pub device: String,
    /// The proxy to announce through (`Policy::proxy`).
    pub proxy: String,
}

/// The peer id this policy sends. One identity, the same to every tracker and
/// to every peer.
///
/// It used to depend on which tracker was being addressed. It does not any
/// more, and the argument is kept only so the call sites read the same: what a
/// tracker is told cannot vary by tracker.
pub fn announced_peer_id(policy: &Policy, _trackers: &[Vec<String>]) -> [u8; 20] {
    let mut out = [0u8; 20];
    let base = policy.peer_id.as_bytes();
    let n = base.len().min(20);
    out[..n].copy_from_slice(&base[..n]);
    out
}

// Every argument is a distinct input the caller already holds; a parameter struct would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    policy: &Policy,
    tracker_url: &str,
    info_hash: &str,
    port: u16,
    uploaded: i64,
    downloaded: i64,
    left: i64,
    event: &str,
    numwant_override: Option<u32>,
    tracker_id: Option<&str>,
) -> Option<Request> {
    if refused_for_cleartext(policy, tracker_url) {
        return None;
    }
    if policy.cleartext_passkey == CleartextPasskey::Warn
        && sends_passkey_in_clear(policy, tracker_url)
    {
        let host = override_host(tracker_url);
        if first_cleartext_note(&host) {
            // The host only: the URL carries the passkey.
            tracing::warn!(tracker = %host, "announce URL carries a passkey over plain http://; set announce_cleartext_passkey to \"refuse\" to stop announcing to it");
        }
    }
    let url_with_key = keyed_url(policy, tracker_url);

    let peer_id = policy.peer_id.clone();
    let user_agent = policy.user_agent.clone();

    let a = Announce {
        tracker_url: &url_with_key,
        info_hash,
        peer_id: &peer_id,
        port,
        uploaded,
        downloaded,
        left,
        event,
        public_ip: &policy.public_ip,
        numwant_override,
        tracker_id,
    };
    let primary = url::build(&a)?;
    let udp = if typhon_engine::tracker::udp::is_udp(&url_with_key) {
        Some(udp_request(&a, &url_with_key)?)
    } else {
        None
    };

    let mut ip_mode = ip_mode_for(policy, tracker_url);
    if policy.no_ipv6 {
        ip_mode = typhon_engine::tracker::http::IpMode::V4;
    }
    Some(Request {
        url: primary,
        user_agent,
        ip_mode,
        udp,
        device: policy.device.clone(),
        proxy: policy.proxy.clone(),
    })
}

/// The BEP 15 form of one announce.
///
/// Counters below zero cannot be sent as the unsigned fields BEP 15 has and
/// are sent as zero, as a negative is already a bug upstream. `ip` is IPv4
/// only; an IPv6 `ip=` has no field and the tracker uses the source address.
fn udp_request(a: &Announce, tracker: &str) -> Option<typhon_engine::tracker::udp::UdpAnnounce> {
    let hex = a.info_hash.as_bytes();
    if hex.len() != 40 {
        return None;
    }
    let mut info_hash = [0u8; 20];
    for (i, pair) in hex.chunks(2).enumerate() {
        info_hash[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    let mut peer_id = [0u8; 20];
    let pid = a.peer_id.as_bytes();
    let n = pid.len().min(20);
    peer_id[..n].copy_from_slice(&pid[..n]);
    Some(typhon_engine::tracker::udp::UdpAnnounce {
        tracker: tracker.to_string(),
        info_hash,
        peer_id,
        downloaded: a.downloaded.max(0) as u64,
        left: a.left.max(0) as u64,
        uploaded: a.uploaded.max(0) as u64,
        event: typhon_engine::tracker::udp::event_code(a.event),
        ip: a
            .public_ip
            .parse::<std::net::Ipv4Addr>()
            .map(u32::from)
            .unwrap_or(0),
        // The HTTP `key`, read back as the 32 bits it is: one key per peer and
        // torrent, whichever transport carries it.
        key: u32::from_str_radix(&url::key_for(a.peer_id, a.info_hash), 16).unwrap_or(0),
        num_want: url::numwant(a.left, a.numwant_override) as i32,
        port: a.port,
    })
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn tiers(urls: &[&str]) -> Vec<Vec<String>> {
        vec![urls.iter().map(|u| u.to_string()).collect()]
    }

    /// One identity, whatever the tracker. A per-tracker override used to
    /// replace the first eight bytes here; a client that presents itself
    /// differently depending on who is asking cannot then ask to be trusted on
    /// anything else it reports.
    #[test]
    fn the_peer_id_does_not_depend_on_the_tracker() {
        let p = Policy {
            peer_id: "-HY4R00-abcdefghijkl".into(),
            ..Default::default()
        };

        let a = announced_peer_id(&p, &tiers(&["https://tracker.example.org/announce"]));
        let b = announced_peer_id(&p, &tiers(&["https://other.example.net/announce"]));
        let c = announced_peer_id(&p, &[]);

        assert_eq!(&a, b"-HY4R00-abcdefghijkl");
        assert_eq!(a, b, "two trackers, one identity");
        assert_eq!(a, c, "and the same with no tracker at all");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An engine with IPv6 off announces over IPv4 only, whatever the
    /// tracker's override says, and every request carries the engine's
    /// interface so the transport can pin it.
    #[test]
    fn an_engine_without_ipv6_announces_over_v4_from_its_interface() {
        let mut p = policy();
        p.no_ipv6 = true;
        p.device = "wg1".into();
        p.ip_modes.insert("tracker.example".into(), "v6".into());
        let ih = "ab".repeat(20);
        let r = prepare(
            &p,
            "https://tracker.example/announce",
            &ih,
            6881,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .expect("request");
        assert_eq!(r.ip_mode, typhon_engine::tracker::http::IpMode::V4);
        assert_eq!(r.device, "wg1");
        let r = prepare(
            &policy(),
            "https://other.example/announce",
            &ih,
            6881,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .expect("request");
        assert_eq!(
            r.ip_mode,
            typhon_engine::tracker::http::IpMode::Auto,
            "a default policy keeps both families"
        );
        assert_eq!(r.device, "");
    }

    /// Two torrents announced with the same peer id do not share a `key`:
    /// the value would otherwise tie every torrent we announce together.
    #[test]
    fn two_torrents_do_not_share_a_key() {
        let p = policy();
        let key_of = |url: &str| {
            url.split("&key=")
                .nth(1)
                .map(|k| k[..8].to_string())
                .expect("a key")
        };
        let a = prepare(
            &p,
            "udp://tr-a.test:6969/announce",
            &"ab".repeat(20),
            16172,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .unwrap();
        let b = prepare(
            &p,
            "udp://tr-a.test:6969/announce",
            &"cd".repeat(20),
            16172,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .unwrap();
        assert_ne!(key_of(&a.url), key_of(&b.url), "HTTP key");
        assert_ne!(a.udp.unwrap().key, b.udp.unwrap().key, "UDP key");
        let again = prepare(
            &p,
            "udp://tr-a.test:6969/announce",
            &"ab".repeat(20),
            16172,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .unwrap();
        assert_eq!(key_of(&a.url), key_of(&again.url), "stable for one torrent");
    }

    fn policy() -> Policy {
        let mut p = Policy {
            peer_id: "-TY0001-abcdefghijkl".into(),
            user_agent: "Hydra/4.0.0".into(),
            ..Default::default()
        };
        p.passkeys.insert("tr-a.test".into(), "NEWKEY".into());
        p
    }

    /// ⭐ One announce, two transports, one set of values: the UDP packet
    /// says what the HTTP URL says -- counters, event, port, numwant, key,
    /// `ip=` -- and the passkey rewrite reaches the BEP 41 path too.
    #[test]
    fn a_udp_tracker_is_told_what_the_http_url_says() {
        let mut p = policy();
        p.public_ip = "203.0.113.9".into();
        let r = prepare(
            &p,
            "udp://tr-a.test:6969/announce/OLDKEY",
            &"ab".repeat(20),
            16172,
            30,
            20,
            10,
            "completed",
            None,
            None,
        )
        .expect("prepared");
        let u = r.udp.expect("a UDP request for a udp:// tracker");
        assert_eq!(
            u.tracker, "udp://tr-a.test:6969/announce/NEWKEY",
            "the passkey is rewritten here too"
        );
        assert_eq!(u.info_hash, [0xAB; 20]);
        assert_eq!(&u.peer_id, b"-TY0001-abcdefghijkl");
        assert_eq!((u.uploaded, u.downloaded, u.left), (30, 20, 10));
        assert_eq!(u.event, 1, "completed");
        assert_eq!(u.port, 16172);
        assert_eq!(
            u.num_want, 200,
            "still leeching: asks for peers, as the URL does"
        );
        assert!(r.url.contains("&numwant=200"));
        assert_eq!(u.ip, u32::from(std::net::Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(
            format!("{:08x}", u.key),
            url::key_for(&p.peer_id, &"ab".repeat(20)),
            "the same key as &key="
        );

        let http = prepare(
            &p,
            "https://tr-a.test/announce/OLDKEY",
            &"ab".repeat(20),
            16172,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .unwrap();
        assert!(http.udp.is_none(), "an HTTP tracker gets no UDP request");
    }

    const IH40: &str = "abababababababababababababababababababab";

    /// The default policy announces a passkey over http:// exactly as it always
    /// did (no new symbol: this compiles on the parent commit too).
    #[test]
    fn a_passkey_over_plain_http_is_announced_unchanged_by_default() {
        let mut p = policy();
        p.passkeys
            .insert("clear-default.example".into(), "NEWKEY".into());
        let ih = "ab".repeat(20);
        let r = prepare(
            &p,
            "http://clear-default.example/announce/OLDKEY",
            &ih,
            6881,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .expect("announces");
        let expected = format!(
            "http://clear-default.example/announce/NEWKEY?info_hash={}&peer_id=-TY0001-abcdefghijkl&port=6881&uploaded=0&downloaded=0&left=0&compact=1&numwant=0&key={}",
            "%AB".repeat(20),
            url::key_for(&p.peer_id, &"ab".repeat(20)),
        );
        assert_eq!(r.url, expected);
    }

    /// `allow` is the same URL as the default; warn and allow never refuse.
    #[test]
    fn only_refuse_refuses_a_cleartext_passkey() {
        let u = "http://clear-default.example/announce/0123456789abcdef0123456789abcdef";
        let mut p = policy();
        let default =
            prepare(&p, u, IH40, 6881, 0, 0, 0, "", None, None).expect("default announces");
        for mode in [CleartextPasskey::Warn, CleartextPasskey::Allow] {
            p.cleartext_passkey = mode;
            assert!(!refused_for_cleartext(&p, u));
            assert_eq!(
                prepare(&p, u, IH40, 6881, 0, 0, 0, "", None, None)
                    .expect("announces")
                    .url,
                default.url
            );
        }
        p.cleartext_passkey = CleartextPasskey::Refuse;
        assert!(refused_for_cleartext(&p, u));
        assert_eq!(CleartextPasskey::parse(""), CleartextPasskey::Warn);
        assert_eq!(CleartextPasskey::parse("Refuse"), CleartextPasskey::Refuse);
    }

    /// With `refuse` no request is built, so none can leave; https, udp and
    /// http URLs without anything key-shaped are untouched.
    #[test]
    fn refuse_builds_no_request_for_a_cleartext_passkey() {
        let mut p = policy();
        p.passkeys
            .insert("configured.example".into(), "NEWKEY".into());
        p.cleartext_passkey = CleartextPasskey::Refuse;
        let refused = |u: &str| prepare(&p, u, IH40, 1, 0, 0, 0, "", None, None).is_none();
        assert!(
            refused("http://t.example/announce/0123456789abcdef0123456789abcdef"),
            "key after announce"
        );
        assert!(
            refused("http://t.example/0123456789abcdef0123456789abcdef/announce"),
            "key before announce"
        );
        assert!(
            refused("http://t.example/announce?pk=0123456789abcdef0123"),
            "key in a query value"
        );
        assert!(
            refused("http://configured.example/announce"),
            "configured override"
        );
        assert!(!refused(
            "https://t.example/announce/0123456789abcdef0123456789abcdef"
        ));
        assert!(!refused("udp://t.example:6969/announce"));
        assert!(!refused("http://t.example/announce"));
        assert!(!refused("http://t.example/announce.php"));
        assert!(!refused("http://t.example/announce?foo=1"));
        assert!(!refused("http://t.example/tracker/ann"));
        assert!(!refused("http://t.example/opentracker_announce"));
        assert!(!refused("http://t.example/public-tracker-announce"));
    }

    #[test]
    fn the_cleartext_note_comes_once_per_host() {
        assert!(first_cleartext_note("once.example"));
        assert!(!first_cleartext_note("once.example"));
        assert!(first_cleartext_note("twice.example"));
    }

    #[test]
    fn a_passkey_replaces_the_last_path_segment() {
        assert_eq!(
            apply_passkey("https://tr-a.test/announce/OLDKEY", "NEWKEY"),
            "https://tr-a.test/announce/NEWKEY"
        );
        // A URL that ends at /announce has no key segment to replace.
        assert_eq!(
            apply_passkey("https://tr-a.test/announce", "NEWKEY"),
            "https://tr-a.test/announce"
        );
        // A query is preserved, not swallowed.
        assert_eq!(
            apply_passkey("https://tr-a.test/announce/OLD?x=1", "NEW"),
            "https://tr-a.test/announce/NEW?x=1"
        );
    }

    /// ⭐ The credential test. A passkey configured for one tracker must never
    /// reach another, whatever the two names look like.
    #[test]
    fn a_passkey_never_reaches_a_tracker_it_was_not_meant_for() {
        let p = policy();
        assert_eq!(
            passkey_for(&p, "https://tr-a.test/announce/X"),
            Some("NEWKEY")
        );
        assert_eq!(
            passkey_for(&p, "https://tk.tr-a.test/announce/X"),
            Some("NEWKEY")
        );
        assert_eq!(passkey_for(&p, "https://nottr-a.test/announce/X"), None);
        assert_eq!(passkey_for(&p, "https://tr-c.example/announce/X"), None);
    }

    #[test]
    fn every_tracker_gets_the_same_identity() {
        let p = policy();
        let r = prepare(
            &p,
            "https://tr-c.example/announce/K",
            &"ab".repeat(20),
            16171,
            0,
            0,
            0,
            "",
            None,
            None,
        )
        .unwrap();
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"), "{}", r.url);
        assert_eq!(
            r.user_agent, "Hydra/4.0.0",
            "no tracker gets told anything else"
        );
    }

    #[test]
    fn an_ordinary_tracker_keeps_our_identity() {
        let p = policy();
        let r = prepare(
            &p,
            "https://tr-a.test/announce/OLD",
            &"ab".repeat(20),
            16171,
            1,
            2,
            3,
            "started",
            None,
            None,
        )
        .unwrap();
        assert!(
            r.url.starts_with("https://tr-a.test/announce/NEWKEY?"),
            "{}",
            r.url
        );
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"));
        assert_eq!(r.user_agent, "Hydra/4.0.0");
    }

    const K1: &str = "0123abcd0123abcd0123abcd";
    const K2: &str = "fedc9876fedc9876fedc9876";

    #[test]
    fn the_override_replaces_a_passkey_query_parameter() {
        for scheme in ["http", "https"] {
            assert_eq!(
                apply_passkey(
                    &format!("{scheme}://tracker.example.invalid/tracker/announce?passkey={K1}"),
                    K2
                ),
                format!("{scheme}://tracker.example.invalid/tracker/announce?passkey={K2}")
            );
        }
        // Other parameters stay, in their order, on both sides of the key.
        assert_eq!(
            apply_passkey(
                &format!("https://tracker.example.invalid/announce?passkey={K1}&other=1"),
                K2
            ),
            format!("https://tracker.example.invalid/announce?passkey={K2}&other=1")
        );
        assert_eq!(
            apply_passkey(
                &format!("https://tracker.example.invalid/announce?a=1&passkey={K1}&b=2"),
                K2
            ),
            format!("https://tracker.example.invalid/announce?a=1&passkey={K2}&b=2")
        );
        // A short key still goes: the name says what it is.
        assert_eq!(
            apply_passkey("https://tracker.example.invalid/announce?passkey=old", K2),
            format!("https://tracker.example.invalid/announce?passkey={K2}")
        );
    }

    #[test]
    fn the_override_replaces_a_key_before_announce() {
        for scheme in ["http", "https"] {
            assert_eq!(
                apply_passkey(
                    &format!("{scheme}://tracker.example.invalid/{K1}/announce"),
                    K2
                ),
                format!("{scheme}://tracker.example.invalid/{K2}/announce")
            );
        }
        assert_eq!(
            apply_passkey(
                &format!("https://tracker.example.invalid/{K1}/announce?x=1"),
                K2
            ),
            format!("https://tracker.example.invalid/{K2}/announce?x=1")
        );
    }

    #[test]
    fn the_last_segment_forms_still_work() {
        assert_eq!(
            apply_passkey("https://tracker.example.invalid/announce/old", K2),
            format!("https://tracker.example.invalid/announce/{K2}")
        );
        assert_eq!(
            apply_passkey("http://tracker.example.invalid/old", K2),
            format!("http://tracker.example.invalid/{K2}")
        );
        assert_eq!(
            apply_passkey(
                &format!("udp://tracker.example.invalid:6969/announce/{K1}"),
                K2
            ),
            format!("udp://tracker.example.invalid:6969/announce/{K2}")
        );
    }

    #[test]
    fn a_url_with_no_identifiable_key_is_not_rewritten_and_says_so() {
        for url in [
            "https://tracker.example.invalid/announce",
            "https://tracker.example.invalid/tracker/announce",
            "https://tracker.example.invalid/announce.php",
            "https://tracker.example.invalid/announce?foo=1",
            "https://tracker.example.invalid/announce/",
            "https://tracker.example.invalid",
            "https://tracker.example.invalid/",
            "udp://tracker.example.invalid:6969/announce",
            "udp://tracker.example.invalid:6969",
        ] {
            assert_eq!(passkey_slot(url), None, "{url}");
            assert_eq!(apply_passkey(url, K2), url, "{url}");
        }
        // And the ones that do have a place say where it is.
        let url = format!("https://tracker.example.invalid/{K1}/announce");
        assert_eq!(
            passkey_slot(&url).map(|r| url[r].to_string()),
            Some(K1.to_string())
        );
        let url = "https://tracker.example.invalid/announce?x=1&passkey=old";
        assert_eq!(
            passkey_slot(url).map(|r| url[r].to_string()),
            Some("old".to_string())
        );
    }

    #[test]
    fn an_override_that_cannot_be_placed_is_noted_once_per_host() {
        let mut p = Policy::default();
        p.passkeys
            .insert("unplaced-a.example.invalid".into(), K2.into());
        p.passkeys
            .insert("placed-a.example.invalid".into(), K2.into());
        let bad = "https://unplaced-a.example.invalid/tracker/announce";
        assert!(!passkey_unplaced("unplaced-a.example.invalid"));
        assert_eq!(keyed_url(&p, bad), bad, "left as it was");
        assert!(passkey_unplaced("unplaced-a.example.invalid"));
        assert!(
            !note_unplaced("unplaced-a.example.invalid"),
            "the second sighting is not news"
        );
        // A host whose URL takes the key is not flagged, nor one with no override.
        let good = format!("https://placed-a.example.invalid/{K1}/announce");
        assert_eq!(
            keyed_url(&p, &good),
            format!("https://placed-a.example.invalid/{K2}/announce")
        );
        assert!(!passkey_unplaced("placed-a.example.invalid"));
        keyed_url(&p, "https://other-a.example.invalid/announce");
        assert!(!passkey_unplaced("other-a.example.invalid"));
    }

    #[test]
    fn prepare_applies_the_override_to_a_query_key() {
        let mut p = policy();
        p.passkeys.insert("tr-q.test".into(), K2.into());
        let r = prepare(
            &p,
            &format!("https://tr-q.test/tracker/announce?passkey={K1}"),
            &"ab".repeat(20),
            16171,
            0,
            0,
            0,
            "started",
            None,
            None,
        )
        .expect("prepared");
        assert!(r.url.contains(&format!("passkey={K2}")), "{}", r.url);
        assert!(!r.url.contains(K1), "{}", r.url);
    }
}
