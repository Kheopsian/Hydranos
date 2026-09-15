//! Building the announce URL.
//!
//! Assembled by hand rather than through a query-string encoder, and that is
//! not laziness: `info_hash` and `peer_id` are twenty raw bytes, not text. A
//! generic encoder would percent-encode the percent signs of an
//! already-encoded hash, and the tracker would answer about a torrent that
//! does not exist. The order of the parameters is kept as 3.x emitted it so a
//! tracker-side log diff shows nothing changed.

/// What one announce needs to know about itself.
pub struct Announce<'a> {
    pub tracker_url: &'a str,
    /// Hex, 40 characters.
    pub info_hash: &'a str,
    pub peer_id: &'a str,
    pub port: u16,
    pub uploaded: i64,
    pub downloaded: i64,
    pub left: i64,
    /// "started", "completed", "stopped", or empty for a periodic announce.
    pub event: &'a str,
    /// BEP-7 `ip=`: the address we want handed to other peers. Empty when the
    /// source address the tracker sees is already the right one.
    pub public_ip: &'a str,
    /// Ask for peers even on a complete torrent.
    ///
    /// Set only by the periodic announce self-check: a seeding torrent asks for
    /// `numwant=0` because it has nothing to dial, but a list of zero peers
    /// also cannot tell us whether the tracker is handing OUR address out.
    /// Once in a while we ask for a small list and look for ourselves in it.
    pub numwant_override: Option<u32>,
}

/// The twenty bytes of an info hash, percent-encoded.
///
/// Every byte is escaped, including the ones that are printable ASCII. 3.x does
/// the same, and a tracker that logs the raw query would show a different
/// string otherwise -- same torrent, different bytes.
pub fn hex_to_url_encoded(hex: &str) -> Option<String> {
    if hex.len() != 40 {
        return None;
    }
    let mut out = String::with_capacity(60);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push('%');
        out.push_str(&format!("{:02X}", hi * 16 + lo));
    }
    Some(out)
}

/// Percent-encode a value for a query string, the way Go's url.QueryEscape does.
///
/// Go escapes a space as `+`; a generic RFC-3986 encoder writes `%20`. Both are
/// accepted by trackers, but this exists to emit what 3.x emitted.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The `key` parameter: an opaque token letting a tracker recognise us across
/// an address change.
///
/// Not required by BEP 3, but qBittorrent, Transmission, libtorrent, Deluge and
/// uTorrent all send one, and a tracker that loses track of a peer on every
/// address change counts it twice.
///
/// Derived from the random suffix of our peer id rather than stored on the
/// side, and that is the whole point: the suffix is the one part of our
/// identity that a per-tracker client spoof does NOT replace, and that a
/// policy reload does not regenerate. A key that changed whenever the version
/// did would be worse than no key at all -- it would tell the tracker that
/// every announce comes from a new peer.
pub fn key_for(peer_id: &str) -> String {
    let suffix = if peer_id.len() >= 20 { &peer_id[8..] } else { peer_id };
    // FNV-1a, 32 bits. Not a security primitive and does not need to be: this
    // only has to be stable for us and opaque to everyone else.
    let mut h: u32 = 0x811c_9dc5;
    for b in suffix.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    format!("{h:08x}")
}

/// The full announce URL, or None when the info hash is not 40 hex characters.
pub fn build(a: &Announce) -> Option<String> {
    let (base, query) = match a.tracker_url.split_once('?') {
        Some((b, q)) => (b, q),
        None => (a.tracker_url, ""),
    };
    let mut url = String::with_capacity(base.len() + 256);
    url.push_str(base);
    url.push('?');
    // A tracker URL that already carries a query keeps it: some trackers put
    // the passkey there rather than in the path.
    if !query.is_empty() {
        url.push_str(query);
        url.push('&');
    }
    url.push_str("info_hash=");
    url.push_str(&hex_to_url_encoded(a.info_hash)?);
    url.push_str("&peer_id=");
    url.push_str(&query_escape(a.peer_id));
    url.push_str("&port=");
    url.push_str(&a.port.to_string());
    url.push_str("&uploaded=");
    url.push_str(&a.uploaded.to_string());
    url.push_str("&downloaded=");
    url.push_str(&a.downloaded.to_string());
    url.push_str("&left=");
    url.push_str(&a.left.to_string());
    // A complete torrent asks for no peers: we are reachable and leechers dial
    // us. Asking for 200 anyway would make the tracker do work for a list we
    // would throw away.
    let numwant = a.numwant_override.unwrap_or(if a.left == 0 { 0 } else { 200 });
    url.push_str("&compact=1&numwant=");
    url.push_str(&numwant.to_string());
    url.push_str("&key=");
    url.push_str(&key_for(a.peer_id));
    if !a.event.is_empty() {
        url.push_str("&event=");
        url.push_str(a.event);
    }
    if !a.public_ip.is_empty() {
        url.push_str("&ip=");
        url.push_str(&query_escape(a.public_ip));
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_info_hash_is_escaped_byte_by_byte() {
        // 'A' is 0x41 and printable, and is still escaped: the tracker log has
        // to read the same as 3.x wrote it.
        assert_eq!(hex_to_url_encoded(&"41".repeat(20)).unwrap(), "%41".repeat(20));
        assert_eq!(hex_to_url_encoded("00").is_none(), true, "40 characters or nothing");
        assert!(hex_to_url_encoded(&"zz".repeat(20)).is_none());
    }

    #[test]
    fn a_space_is_a_plus_not_a_percent_twenty() {
        // Go's url.QueryEscape spells a space `+`. Both are legal; this is the
        // one 3.x sent.
        assert_eq!(query_escape("a b"), "a+b");
        assert_eq!(query_escape("-_.~"), "-_.~");
        assert_eq!(query_escape("/"), "%2F");
    }

    #[test]
    fn a_seeding_torrent_asks_for_no_peers() {
        let a = Announce {
            tracker_url: "https://tr4ker.net/announce/KEY",
            info_hash: &"ab".repeat(20),
            peer_id: "-qB5220-abcdefghijkl",
            port: 16171,
            uploaded: 10,
            downloaded: 20,
            left: 0,
            event: "",
            public_ip: "",
            numwant_override: None,
        };
        let u = build(&a).unwrap();
        assert!(u.contains("&numwant=0"), "a complete torrent wants no peers: {u}");
        assert!(u.starts_with("https://tr4ker.net/announce/KEY?info_hash=%AB"));
        assert!(!u.contains("&event="), "a periodic announce carries no event");
    }

    #[test]
    fn a_tracker_query_is_kept_and_ours_appended() {
        let a = Announce {
            tracker_url: "https://tr4ker.net/announce?passkey=SECRET",
            info_hash: &"ab".repeat(20),
            peer_id: "-qB5220-abcdefghijkl",
            port: 16171,
            uploaded: 0,
            downloaded: 0,
            left: 100,
            event: "started",
            public_ip: "203.0.113.7",
            numwant_override: None,
        };
        let u = build(&a).unwrap();
        assert!(u.starts_with("https://tr4ker.net/announce?passkey=SECRET&info_hash="));
        assert!(u.contains("&numwant=200"), "a leeching torrent asks for peers");
        assert!(u.ends_with("&event=started&ip=203.0.113.7"));
    }

}

/// Conformance of the announce URL to the specifications, one rule per test.
///
/// These sit next to the builder rather than in `tests/` because the announce
/// policy lives in the `hydra` binary, not in the `typhon_engine` library, and
/// an integration test can only reach the library. The wire-level half of the
/// suite is `typhon-engine/tests/bep_conformance.rs`, which drives a real
/// tracker; this half checks what we put in the query before it gets there.
///
/// References:
///   BEP 3  -- announce parameters
///   BEP 7  -- `ip=`
///   BEP 20 -- peer id conventions
///   BEP 23 -- `compact=1`
#[cfg(test)]
mod bep_rules {
    use super::*;

    /// A seeding torrent, the ordinary case: complete, periodic, no event.
    fn seeding() -> Announce<'static> {
        Announce {
            tracker_url: "https://tracker.example.net/announce",
            info_hash: "abcdef0123456789abcdef0123456789abcdef01",
            peer_id: "-TY4R00-abcdefghijkl",
            port: 16171,
            uploaded: 4096,
            downloaded: 0,
            left: 0,
            event: "",
            public_ip: "",
            numwant_override: None,
        }
    }

    /// Split a built URL into its query parameters, in order.
    fn params(url: &str) -> Vec<(String, String)> {
        let q = url.split_once('?').map(|(_, q)| q).unwrap_or("");
        q.split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (kv.to_string(), String::new()),
            })
            .collect()
    }

    fn value_of(url: &str, name: &str) -> Option<String> {
        params(url).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Decode a percent-encoded query value back to bytes.
    fn percent_decode(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'%' if i + 2 < b.len() => {
                    match (
                        (b[i + 1] as char).to_digit(16),
                        (b[i + 2] as char).to_digit(16),
                    ) {
                        (Some(h), Some(l)) => {
                            out.push((h * 16 + l) as u8);
                            i += 3;
                        }
                        _ => {
                            out.push(b[i]);
                            i += 1;
                        }
                    }
                }
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                c => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // BEP 3 -- the parameters a tracker is entitled to expect
    // -----------------------------------------------------------------------

    /// BEP 3 lists info_hash, peer_id, port, uploaded, downloaded and left as
    /// the parameters of an announce. A tracker may reject a request missing
    /// any of them, and several private ones do.
    #[test]
    fn bep3_every_mandatory_parameter_is_emitted() {
        let url = build(&seeding()).expect("a valid announce builds");
        let names: Vec<String> = params(&url).into_iter().map(|(k, _)| k).collect();
        for required in ["info_hash", "peer_id", "port", "uploaded", "downloaded", "left"] {
            assert!(
                names.iter().any(|n| n == required),
                "BEP 3 requires `{required}`; the query carries {names:?}"
            );
        }
    }

    /// BEP 3: info_hash is the twenty raw bytes of the hash, percent-encoded --
    /// not its forty hex characters. Sending hex makes the tracker look up a
    /// torrent that does not exist.
    #[test]
    fn bep3_the_info_hash_is_twenty_bytes_not_forty_characters() {
        let url = build(&seeding()).unwrap();
        let raw = value_of(&url, "info_hash").expect("info_hash");
        assert_eq!(
            percent_decode(&raw).len(),
            20,
            "BEP 3: twenty raw bytes, got `{raw}`"
        );
    }

    /// BEP 20: a peer id is twenty bytes. Nineteen or twenty-one is refused by
    /// trackers outright, and our own handshake reader blocks forever on a
    /// short one.
    #[test]
    fn bep20_the_peer_id_is_twenty_bytes() {
        let url = build(&seeding()).unwrap();
        let raw = value_of(&url, "peer_id").expect("peer_id");
        assert_eq!(
            percent_decode(&raw).len(),
            20,
            "BEP 20: twenty bytes, got `{raw}`"
        );
    }

    /// BEP 20, Azureus convention: `-XX####-` then twelve free bytes. The
    /// leading dash is what tells a parser which convention to read, and the
    /// trailing one terminates the version field.
    #[test]
    fn bep20_the_peer_id_follows_the_azureus_convention() {
        let url = build(&seeding()).unwrap();
        let id = percent_decode(&value_of(&url, "peer_id").unwrap());
        assert_eq!(id[0], b'-', "BEP 20: an Azureus-style id opens with a dash");
        assert_eq!(id[7], b'-', "BEP 20: and closes its version field with one");
        assert!(
            id[1].is_ascii_alphabetic() && id[2].is_ascii_alphabetic(),
            "BEP 20: two letters of client code, got {:?}",
            &id[1..3]
        );
    }

    /// BEP 3: `left` is what remains to be downloaded. Zero means seeding, and
    /// it is the only thing that tells a tracker to count us as a seeder.
    #[test]
    fn bep3_a_complete_torrent_announces_left_zero() {
        let url = build(&seeding()).unwrap();
        assert_eq!(value_of(&url, "left").as_deref(), Some("0"));
    }

    /// BEP 3: the counters are decimal and never negative. A negative `left`
    /// from a bad subtraction is read by some trackers as an enormous unsigned
    /// number.
    #[test]
    fn bep3_the_counters_are_non_negative_decimals() {
        let url = build(&seeding()).unwrap();
        for name in ["uploaded", "downloaded", "left", "port"] {
            let v = value_of(&url, name).unwrap_or_else(|| panic!("{name} present"));
            assert!(
                v.chars().all(|c| c.is_ascii_digit()),
                "BEP 3: `{name}` is a non-negative decimal, got `{v}`"
            );
        }
    }

    // -----------------------------------------------------------------------
    // BEP 3 -- events
    // -----------------------------------------------------------------------

    /// BEP 3: a periodic announce carries no `event` at all. Sending an empty
    /// `event=` is not the same thing, and some trackers treat the unknown
    /// value as an error.
    #[test]
    fn bep3_a_periodic_announce_carries_no_event_key() {
        let url = build(&seeding()).unwrap();
        assert!(
            !url.contains("event="),
            "BEP 3: a periodic announce omits the key entirely: {url}"
        );
    }

    /// BEP 3: the only defined events are started, completed and stopped.
    #[test]
    fn bep3_only_the_three_defined_events_are_emitted() {
        for event in ["started", "completed", "stopped"] {
            let a = Announce { event, ..seeding() };
            let url = build(&a).unwrap();
            assert_eq!(
                value_of(&url, "event").as_deref(),
                Some(event),
                "BEP 3: `{event}` is a defined event and must go out as written"
            );
        }
    }

    // -----------------------------------------------------------------------
    // BEP 23 / BEP 7
    // -----------------------------------------------------------------------

    /// BEP 23: we ask for the compact peer list. A tracker that only speaks
    /// compact -- most private ones -- answers an error otherwise.
    #[test]
    fn bep23_the_compact_peer_list_is_requested() {
        let url = build(&seeding()).unwrap();
        assert_eq!(value_of(&url, "compact").as_deref(), Some("1"));
    }

    /// BEP 7: `ip=` states the address we want handed to other peers, and is
    /// only sent when the source address the tracker sees is not the right one.
    #[test]
    fn bep7_the_ip_parameter_appears_only_when_we_have_one_to_declare() {
        let url = build(&seeding()).unwrap();
        assert!(!url.contains("&ip="), "nothing to declare, nothing sent");

        let a = Announce { public_ip: "203.0.113.7", ..seeding() };
        let url = build(&a).unwrap();
        assert_eq!(value_of(&url, "ip").as_deref(), Some("203.0.113.7"));
    }

    /// A seeding torrent has nothing to dial, so it asks for no peers. Asking
    /// for two hundred per announce across a seeding catalogue is what opens
    /// thousands of idle sockets to the same few seedboxes.
    #[test]
    fn a_seeding_torrent_asks_for_no_peers_and_a_leeching_one_does() {
        assert_eq!(value_of(&build(&seeding()).unwrap(), "numwant").as_deref(), Some("0"));

        let a = Announce { left: 1024, ..seeding() };
        assert_eq!(value_of(&build(&a).unwrap(), "numwant").as_deref(), Some("200"));
    }

    // -----------------------------------------------------------------------
    // Conventions every major client follows
    // -----------------------------------------------------------------------

    /// `key` is not required by BEP 3, but qBittorrent, Transmission,
    /// libtorrent, Deluge and uTorrent all send it, and trackers use it to
    /// recognise a peer whose address changed between two announces. Without
    /// it the only handle a tracker has on us is the peer id -- and ours
    /// changes whenever the version does.
    ///
    /// This is the deviation a strict operator names first.
    #[test]
    fn convention_a_key_is_sent_so_the_tracker_can_re_identify_us() {
        let url = build(&seeding()).unwrap();
        let key = value_of(&url, "key");
        assert!(
            key.is_some(),
            "every major client sends `key`; ours sends {:?}",
            params(&url).iter().map(|(k, _)| k.clone()).collect::<Vec<_>>()
        );
        let key = key.unwrap();
        assert!(
            !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()),
            "`key` is an opaque alphanumeric token, got `{key}`"
        );
    }

    /// `key` identifies the client across address changes, so it must not move
    /// between two announces for the same torrent. One that changes every time
    /// is worse than none: it tells the tracker each announce is a new peer.
    #[test]
    fn convention_the_key_does_not_change_between_two_announces() {
        let first = build(&seeding()).unwrap();
        let second = build(&seeding()).unwrap();
        assert_eq!(
            value_of(&first, "key"),
            value_of(&second, "key"),
            "`key` is stable for the life of the session"
        );
    }

    // -----------------------------------------------------------------------
    // The tracker's own query survives
    // -----------------------------------------------------------------------

    /// A passkey carried in the tracker's query string is kept, and ours is
    /// appended after it. Losing it announces to a private tracker anonymously,
    /// which is an instant ban on most of them.
    #[test]
    fn a_passkey_in_the_tracker_url_is_never_dropped() {
        let a = Announce {
            tracker_url: "https://tracker.example.net/announce?passkey=SECRET",
            ..seeding()
        };
        let url = build(&a).unwrap();
        assert_eq!(value_of(&url, "passkey").as_deref(), Some("SECRET"));
        assert!(url.starts_with("https://tracker.example.net/announce?passkey=SECRET&info_hash="));
    }

    /// An info hash that is not forty hex characters produces no URL at all,
    /// rather than a request the tracker will answer about nothing.
    #[test]
    fn a_malformed_info_hash_builds_no_url() {
        let a = Announce { info_hash: "not-a-hash", ..seeding() };
        assert!(build(&a).is_none());
    }
}
