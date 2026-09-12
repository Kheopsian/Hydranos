//! What a tracker is told, and by whom.
//!
//! The transport lives in `typhon_engine::tracker::http`. This decides what
//! goes on the wire: whose passkey, which client we claim to be, which address
//! we ask peers to use. Splitting it that way is what lets the URL be built and
//! asserted in a unit test -- the string the tracker sees is the contract, and
//! it is checkable without a network.

use std::collections::BTreeMap;

use super::overrides::{longest_override_key, override_host};
use super::url::{self, Announce};

/// A client to impersonate for one tracker: the eight-byte peer id prefix and
/// the User-Agent that goes with it.
///
/// Some trackers keep a client whitelist. Claiming a whitelisted client is how
/// 3.x got announces accepted, and the two halves must agree -- a qBittorrent
/// peer id with a Hydra User-Agent is a mismatch a tracker can spot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpoof {
    pub peer_id_prefix: String,
    pub user_agent: String,
}

/// Everything the announcer knows about how to talk to trackers.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    /// host -> passkey, replacing the one in the tracker URL.
    pub passkeys: BTreeMap<String, String>,
    /// host -> client to impersonate.
    pub clients: BTreeMap<String, ClientSpoof>,
    /// host -> "off" to skip the secondary announce for that tracker.
    pub secondary_stats: BTreeMap<String, String>,
    /// Our own peer id, used when no tracker asks for another.
    pub peer_id: String,
    pub user_agent: String,
    /// BEP-7 `ip=`. Empty when the source address is already right.
    pub public_ip: String,
    /// host -> "v4" | "v6" | "auto". Absent means auto: announce from both
    /// families, as libtorrent does. A tracker that overwrites instead of
    /// merging the two addresses should be pinned to one family here.
    pub ip_modes: BTreeMap<String, String>,
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

/// The client to impersonate for this tracker, if any.
pub fn client_for<'a>(policy: &'a Policy, tracker_url: &str) -> Option<&'a ClientSpoof> {
    let host = override_host(tracker_url);
    let key = longest_override_key(&host, policy.clients.keys().map(|s| s.as_str()))?;
    policy.clients.get(key)
}

/// Whether the secondary announce is wanted for this tracker.
///
/// It is skipped for a spoofed tracker whatever the mode says: the second
/// announce is a double-credit trick, and posting a second impersonated peer to
/// a tracker that whitelists clients is how one account gets noticed.
pub fn secondary_wanted(policy: &Policy, tracker_url: &str, spoofed: bool) -> bool {
    if spoofed {
        return false;
    }
    let host = override_host(tracker_url);
    match longest_override_key(&host, policy.secondary_stats.keys().map(|s| s.as_str())) {
        Some(k) => policy.secondary_stats.get(k).map(|m| m != "off").unwrap_or(true),
        None => true,
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
    /// The URL of the secondary announce, when one is wanted. Its peer id has
    /// its last byte flipped so a tracker that dedups by peer id keeps both
    /// entries instead of overwriting the first.
    pub secondary_url: Option<String>,
}

/// Build the announce for one torrent on one tracker.
/// The eight-byte peer-id prefix this torrent must present in the BT
/// handshake, or `None` when the binding's own will do.
///
/// The FIRST tracker decides. A torrent announcing to several private trackers
/// cannot show a different client to each of them -- they share one swarm, and
/// its peers compare notes. An operator who lists several will have overridden
/// all of them anyway; taking the first is the only choice that is stable.
///
/// `None` for a torrent with no tracker override, which is every public one:
/// DHT and PEX hand over peers with nobody vouching for them and nobody
/// checking, so there is nothing to stay consistent with.
///
/// The spoof replaces only the eight-byte prefix, so the random tail -- which
/// differs per binding -- survives. That is what keeps two engines of the same
/// node distinguishable, and the self-connection guard working.
/// The full peer id this policy would send for these trackers.
///
/// The override replaces the 8-byte prefix only; the random tail stays the
/// engine's, which is what keeps one torrent distinguishable from another on
/// the same tracker.
pub fn announced_peer_id(policy: &Policy, trackers: &[Vec<String>]) -> [u8; 20] {
    let mut out = [0u8; 20];
    let base = policy.peer_id.as_bytes();
    let n = base.len().min(20);
    out[..n].copy_from_slice(&base[..n]);
    if let Some(prefix) = handshake_prefix(policy, trackers) {
        out[..8].copy_from_slice(&prefix);
    }
    out
}

pub fn handshake_prefix(policy: &Policy, trackers: &[Vec<String>]) -> Option<[u8; 8]> {
    let first = trackers.iter().flatten().next()?;
    let spoof = client_for(policy, first)?;
    let bytes = spoof.peer_id_prefix.as_bytes();
    if bytes.len() != 8 {
        return None;
    }
    let mut out = [0u8; 8];
    out.copy_from_slice(bytes);
    Some(out)
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
) -> Option<Request> {
    let url_with_key = match passkey_for(policy, tracker_url) {
        Some(k) => apply_passkey(tracker_url, k),
        None => tracker_url.to_string(),
    };

    let spoof = client_for(policy, tracker_url);
    let peer_id = match spoof {
        Some(s) => url::spoofed_peer_id(&policy.peer_id, &s.peer_id_prefix),
        None => policy.peer_id.clone(),
    };
    let user_agent = match spoof {
        Some(s) if !s.user_agent.is_empty() => s.user_agent.clone(),
        _ => policy.user_agent.clone(),
    };

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
    };
    let primary = url::build(&a)?;

    let secondary_url = if secondary_wanted(policy, tracker_url, spoof.is_some()) {
        flip_last_peer_id_byte(&peer_id).and_then(|alt| {
            let b = Announce { peer_id: &alt, ..a };
            url::build(&b)
        })
    } else {
        None
    };

    let ip_mode = ip_mode_for(policy, tracker_url);
    Some(Request { url: primary, user_agent, secondary_url, ip_mode })
}

/// The same peer id with its last byte flipped.
fn flip_last_peer_id_byte(peer_id: &str) -> Option<String> {
    let mut bytes = peer_id.as_bytes().to_vec();
    if bytes.len() < 20 {
        return None;
    }
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod handshake_identity_tests {
    use super::*;

    fn policy_with(host: &str, prefix: &str) -> Policy {
        let mut p = Policy::default();
        p.clients.insert(
            host.to_string(),
            ClientSpoof { peer_id_prefix: prefix.into(), user_agent: "qBittorrent/5.2.2".into() },
        );
        p
    }
    fn tiers(urls: &[&str]) -> Vec<Vec<String>> {
        vec![urls.iter().map(|u| u.to_string()).collect()]
    }

    /// The case this exists for: the tracker was told -qB5220- while its swarm
    /// saw -HY....-, and a strict tracker compares the two.
    #[test]
    fn an_overridden_tracker_sets_the_handshake_too() {
        let p = policy_with("tracker.example.org", "-qB5220-");
        let got = handshake_prefix(&p, &tiers(&["https://tracker.example.org/announce"]));
        assert_eq!(got, Some(*b"-qB5220-"));
    }

    /// Public torrents keep the binding's own id: nobody vouches for a DHT or
    /// PEX peer and nobody cross-checks, so there is nothing to match.
    #[test]
    fn a_tracker_with_no_override_changes_nothing() {
        let p = policy_with("tracker.example.org", "-qB5220-");
        let got = handshake_prefix(&p, &tiers(&["https://other.example.net/announce"]));
        assert_eq!(got, None);
    }

    /// The FIRST tracker decides. Several private trackers share one swarm, so
    /// a torrent cannot show each of them a different client.
    #[test]
    fn the_first_tracker_decides() {
        let mut p = policy_with("first.example.org", "-qB5220-");
        p.clients.insert(
            "second.example.org".into(),
            ClientSpoof { peer_id_prefix: "-DE13F0-".into(), user_agent: "Deluge".into() },
        );
        let got = handshake_prefix(&p, &tiers(&[
            "https://first.example.org/announce",
            "https://second.example.org/announce",
        ]));
        assert_eq!(got, Some(*b"-qB5220-"));
    }

    /// A prefix that is not eight bytes would shift the random tail into the
    /// client field of whoever reads it. Refused rather than truncated.
    #[test]
    fn a_malformed_prefix_is_refused() {
        let p = policy_with("tracker.example.org", "-qB-");
        assert_eq!(handshake_prefix(&p, &tiers(&["https://tracker.example.org/announce"])), None);
    }

    /// No tracker at all: a magnet before metadata, or a torrent stripped of
    /// its trackers.
    #[test]
    fn no_tracker_means_no_override() {
        let p = policy_with("tracker.example.org", "-qB5220-");
        assert_eq!(handshake_prefix(&p, &[]), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        let mut p = Policy {
            peer_id: "-TY0001-abcdefghijkl".into(),
            user_agent: "Hydra/4.0.0".into(),
            ..Default::default()
        };
        p.passkeys.insert("tr4ker.net".into(), "NEWKEY".into());
        p.clients.insert(
            "mam.example".into(),
            ClientSpoof { peer_id_prefix: "-qB5220-".into(), user_agent: "qBittorrent/5.2.2".into() },
        );
        p
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
    fn a_spoofed_tracker_gets_the_claimed_client_and_no_second_announce() {
        let p = policy();
        let r = prepare(&p, "https://mam.example/announce/K", &"ab".repeat(20), 16171, 0, 0, 0, "", None)
            .unwrap();
        assert!(r.url.contains("peer_id=-qB5220-abcdefghijkl"), "{}", r.url);
        assert_eq!(r.user_agent, "qBittorrent/5.2.2");
        assert!(
            r.secondary_url.is_none(),
            "a second impersonated peer is how one account gets noticed"
        );
    }

    #[test]
    fn an_ordinary_tracker_keeps_our_identity_and_gets_a_second_announce() {
        let p = policy();
        let r = prepare(&p, "https://tr4ker.net/announce/OLD", &"ab".repeat(20), 16171, 1, 2, 3, "started", None)
            .unwrap();
        assert!(r.url.starts_with("https://tr4ker.net/announce/NEWKEY?"), "{}", r.url);
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"));
        assert_eq!(r.user_agent, "Hydra/4.0.0");
        let sec = r.secondary_url.unwrap();
        assert!(sec.contains("peer_id=-TY0001-abcdefghijkm"), "last byte flipped: {sec}");
    }

    #[test]
    fn a_tracker_marked_off_gets_no_second_announce() {
        let mut p = policy();
        p.secondary_stats.insert("tr4ker.net".into(), "off".into());
        let r = prepare(&p, "https://tr4ker.net/announce/OLD", &"ab".repeat(20), 16171, 0, 0, 5, "", None)
            .unwrap();
        assert!(r.secondary_url.is_none());
    }
}
