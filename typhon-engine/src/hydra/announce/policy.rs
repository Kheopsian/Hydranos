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
    /// The engine's `enable_ipv6 = false`: every tracker is announced over
    /// IPv4 only (4.3 announced `auto` trackers over both regardless). Named
    /// for what it does when set, like `skip_udp`, so a default policy keeps
    /// both families.
    pub no_ipv6: bool,
    /// How long a race retries a tracker that has not registered the torrent
    /// yet. Zero = the default window.
    pub registration_window: Duration,
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

/// Rewrite the passkey segment of a tracker URL.
///
/// The passkey is the last path segment on every tracker that puts it in the
/// path (`/announce/<key>`), which is the shape 3.x rewrites. A tracker that
/// carries it in the query is left alone -- guessing which query parameter is
/// the credential would be worse than not rewriting.
pub fn apply_passkey(tracker_url: &str, passkey: &str) -> String {
    let (base, query) = match tracker_url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (tracker_url, None),
    };
    let rewritten = match base.rsplit_once('/') {
        Some((head, last)) if !last.is_empty() && last != "announce" => {
            format!("{head}/{passkey}")
        }
        _ => base.to_string(),
    };
    match query {
        Some(q) => format!("{rewritten}?{q}"),
        None => rewritten,
    }
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
    let url_with_key = match passkey_for(policy, tracker_url) {
        Some(k) => apply_passkey(tracker_url, k),
        None => tracker_url.to_string(),
    };

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
    Some(Request { url: primary, user_agent, ip_mode, udp, device: policy.device.clone() })
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
        ip: a.public_ip.parse::<std::net::Ipv4Addr>().map(u32::from).unwrap_or(0),
        // The HTTP `key`, read back as the 32 bits it is: one key per peer,
        // whichever transport carries it.
        key: u32::from_str_radix(&url::key_for(a.peer_id), 16).unwrap_or(0),
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
        let mut p = Policy::default();
        p.peer_id = "-HY4R00-abcdefghijkl".into();

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
        let r = prepare(&p, "https://tracker.example/announce", &ih, 6881, 0, 0, 0, "", None, None).expect("request");
        assert_eq!(r.ip_mode, typhon_engine::tracker::http::IpMode::V4);
        assert_eq!(r.device, "wg1");
        let r = prepare(&policy(), "https://other.example/announce", &ih, 6881, 0, 0, 0, "", None, None).expect("request");
        assert_eq!(r.ip_mode, typhon_engine::tracker::http::IpMode::Auto, "a default policy keeps both families");
        assert_eq!(r.device, "");
    }

    fn policy() -> Policy {
        let mut p = Policy {
            peer_id: "-TY0001-abcdefghijkl".into(),
            user_agent: "Hydra/4.0.0".into(),
            ..Default::default()
        };
        p.passkeys.insert("tr4ker.net".into(), "NEWKEY".into());
        p
    }

    /// ⭐ One announce, two transports, one set of values: the UDP packet
    /// says what the HTTP URL says -- counters, event, port, numwant, key,
    /// `ip=` -- and the passkey rewrite reaches the BEP 41 path too.
    #[test]
    fn a_udp_tracker_is_told_what_the_http_url_says() {
        let mut p = policy();
        p.public_ip = "203.0.113.9".into();
        let r = prepare(&p, "udp://tr4ker.net:6969/announce/OLDKEY", &"ab".repeat(20), 16172, 30, 20, 10, "completed", None, None)
            .expect("prepared");
        let u = r.udp.expect("a UDP request for a udp:// tracker");
        assert_eq!(u.tracker, "udp://tr4ker.net:6969/announce/NEWKEY", "the passkey is rewritten here too");
        assert_eq!(u.info_hash, [0xAB; 20]);
        assert_eq!(&u.peer_id, b"-TY0001-abcdefghijkl");
        assert_eq!((u.uploaded, u.downloaded, u.left), (30, 20, 10));
        assert_eq!(u.event, 1, "completed");
        assert_eq!(u.port, 16172);
        assert_eq!(u.num_want, 200, "still leeching: asks for peers, as the URL does");
        assert!(r.url.contains("&numwant=200"));
        assert_eq!(u.ip, u32::from(std::net::Ipv4Addr::new(203, 0, 113, 9)));
        assert_eq!(format!("{:08x}", u.key), url::key_for(&p.peer_id), "the same key as &key=");

        let http = prepare(&p, "https://tr4ker.net/announce/OLDKEY", &"ab".repeat(20), 16172, 0, 0, 0, "", None, None).unwrap();
        assert!(http.udp.is_none(), "an HTTP tracker gets no UDP request");
    }

    #[test]
    fn a_passkey_replaces_the_last_path_segment() {
        assert_eq!(
            apply_passkey("https://tr4ker.net/announce/OLDKEY", "NEWKEY"),
            "https://tr4ker.net/announce/NEWKEY"
        );
        // A URL that ends at /announce has no key segment to replace.
        assert_eq!(
            apply_passkey("https://tr4ker.net/announce", "NEWKEY"),
            "https://tr4ker.net/announce"
        );
        // A query is preserved, not swallowed.
        assert_eq!(
            apply_passkey("https://tr4ker.net/announce/OLD?x=1", "NEW"),
            "https://tr4ker.net/announce/NEW?x=1"
        );
    }

    /// ⭐ The credential test. A passkey configured for one tracker must never
    /// reach another, whatever the two names look like.
    #[test]
    fn a_passkey_never_reaches_a_tracker_it_was_not_meant_for() {
        let p = policy();
        assert_eq!(passkey_for(&p, "https://tr4ker.net/announce/X"), Some("NEWKEY"));
        assert_eq!(passkey_for(&p, "https://tk.tr4ker.net/announce/X"), Some("NEWKEY"));
        assert_eq!(passkey_for(&p, "https://nottr4ker.net/announce/X"), None);
        assert_eq!(passkey_for(&p, "https://mam.example/announce/X"), None);
    }

    #[test]
    fn every_tracker_gets_the_same_identity() {
        let p = policy();
        let r = prepare(&p, "https://mam.example/announce/K", &"ab".repeat(20), 16171, 0, 0, 0, "", None, None)
            .unwrap();
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"), "{}", r.url);
        assert_eq!(r.user_agent, "Hydra/4.0.0", "no tracker gets told anything else");
    }

    #[test]
    fn an_ordinary_tracker_keeps_our_identity() {
        let p = policy();
        let r = prepare(&p, "https://tr4ker.net/announce/OLD", &"ab".repeat(20), 16171, 1, 2, 3, "started", None, None)
            .unwrap();
        assert!(r.url.starts_with("https://tr4ker.net/announce/NEWKEY?"), "{}", r.url);
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"));
        assert_eq!(r.user_agent, "Hydra/4.0.0");
    }
}
