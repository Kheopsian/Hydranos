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
    let numwant = if a.left == 0 { 0 } else { 200 };
    url.push_str("&compact=1&numwant=");
    url.push_str(&numwant.to_string());
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

/// Apply a tracker's client spoof to our peer id.
///
/// Only the eight-byte prefix changes; the suffix is ours and stays, so the
/// tracker sees one consistent peer across announces instead of a new one
/// every time.
pub fn spoofed_peer_id(peer_id: &str, prefix: &str) -> String {
    if peer_id.len() >= 20 && prefix.len() == 8 {
        format!("{}{}", prefix, &peer_id[8..])
    } else {
        peer_id.to_string()
    }
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
        };
        let u = build(&a).unwrap();
        assert!(u.starts_with("https://tr4ker.net/announce?passkey=SECRET&info_hash="));
        assert!(u.contains("&numwant=200"), "a leeching torrent asks for peers");
        assert!(u.ends_with("&event=started&ip=203.0.113.7"));
    }

    #[test]
    fn a_spoof_changes_the_prefix_and_keeps_our_suffix() {
        let ours = "-TY0001-abcdefghijkl";
        assert_eq!(spoofed_peer_id(ours, "-qB5220-"), "-qB5220-abcdefghijkl");
        // A prefix of the wrong length is ignored rather than truncating the id
        // into something no tracker would accept.
        assert_eq!(spoofed_peer_id(ours, "-qB-"), ours);
    }
}
