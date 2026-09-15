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
    };
    let primary = url::build(&a)?;

    let ip_mode = ip_mode_for(policy, tracker_url);
    Some(Request { url: primary, user_agent, ip_mode })
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

    fn policy() -> Policy {
        let mut p = Policy {
            peer_id: "-TY0001-abcdefghijkl".into(),
            user_agent: "Hydra/4.0.0".into(),
            ..Default::default()
        };
        p.passkeys.insert("tr4ker.net".into(), "NEWKEY".into());
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
    fn every_tracker_gets_the_same_identity() {
        let p = policy();
        let r = prepare(&p, "https://mam.example/announce/K", &"ab".repeat(20), 16171, 0, 0, 0, "", None)
            .unwrap();
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"), "{}", r.url);
        assert_eq!(r.user_agent, "Hydra/4.0.0", "no tracker gets told anything else");
    }

    #[test]
    fn an_ordinary_tracker_keeps_our_identity() {
        let p = policy();
        let r = prepare(&p, "https://tr4ker.net/announce/OLD", &"ab".repeat(20), 16171, 1, 2, 3, "started", None)
            .unwrap();
        assert!(r.url.starts_with("https://tr4ker.net/announce/NEWKEY?"), "{}", r.url);
        assert!(r.url.contains("peer_id=-TY0001-abcdefghijkl"));
        assert_eq!(r.user_agent, "Hydra/4.0.0");
    }
}
