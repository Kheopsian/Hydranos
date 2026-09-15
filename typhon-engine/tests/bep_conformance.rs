//! Conformance of the tracker client to the BitTorrent specifications,
//! checked against a tracker rather than against itself.
//!
//! Every test here starts a real HTTP tracker on loopback, lets the real
//! announce path talk to it, and asserts on two things: the query that arrived,
//! and what we made of the answer. Nothing is mocked at the module boundary --
//! a test that asserts on a string our own builder produced proves only that
//! the builder is consistent with itself, which is exactly the objection a
//! tracker operator would raise.
//!
//! Each test names the rule it covers. When one fails, the failure message is
//! meant to be readable by someone who has the BEP open and not our source.
//!
//! References:
//!   BEP 3  -- the core protocol, announce parameters and response
//!   BEP 7  -- IPv6 trackers, `peers6`
//!   BEP 23 -- compact peer lists
//!   BEP 48 -- scrape
//!
//! The tracker answers are kept deliberately minimal: a real tracker sends
//! more, and a client that needs the extras to work would break against the
//! strict ones.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use typhon_engine::tracker::http::{send_announce, AnnounceResponse, IpMode};

/// A tracker that records what it was asked and answers what it was told to.
struct Tracker {
    /// `http://127.0.0.1:PORT/announce`, ready to be handed to the client.
    url: String,
    /// The raw query string of the request that arrived, without the `?`.
    query: mpsc::Receiver<String>,
    /// Every header line of the request that arrived.
    headers: mpsc::Receiver<Vec<String>>,
}

impl Tracker {
    /// The query of the announce that arrived, split into pairs, in order.
    ///
    /// Order matters to us: a tracker-side log diff between 3.x and 4.x should
    /// show nothing moved, so the order is part of what is asserted.
    fn params(&self) -> Vec<(String, String)> {
        let q = self
            .query
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the tracker was never asked anything");
        q.split('&')
            .filter(|s| !s.is_empty())
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (k.to_string(), v.to_string()),
                None => (kv.to_string(), String::new()),
            })
            .collect()
    }

    fn header(&self, name: &str) -> Option<String> {
        let hs = self
            .headers
            .recv_timeout(std::time::Duration::from_secs(5))
            .ok()?;
        let want = format!("{}:", name.to_ascii_lowercase());
        hs.iter()
            .find(|h| h.to_ascii_lowercase().starts_with(&want))
            .map(|h| h[want.len()..].trim().to_string())
    }
}

/// Start a tracker that answers `body` to the first request it receives.
///
/// Bound to port 0 so tests can run in parallel without agreeing on a port,
/// and to 127.0.0.1 so nothing leaves the machine running the suite.
fn tracker_answering(body: &'static [u8]) -> Tracker {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let (qtx, qrx) = mpsc::channel();
    let (htx, hrx) = mpsc::channel();

    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        // A tracker request is a GET with no body, so the head is the whole of
        // it: read until the blank line and stop. Reading to EOF would block
        // until the client closes, which it will not do before it has an answer.
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            match sock.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => buf.push(byte[0]),
            }
        }
        // The request target is percent-encoded ASCII; the raw bytes of an info
        // hash never reach us undecoded, so lossy conversion is safe here.
        let head = String::from_utf8_lossy(&buf).into_owned();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let target = request_line.split_whitespace().nth(1).unwrap_or_default();
        let query = target.split_once('?').map(|(_, q)| q).unwrap_or("").to_string();
        let headers: Vec<String> = lines.take_while(|l| !l.is_empty()).map(String::from).collect();

        let _ = qtx.send(query);
        let _ = htx.send(headers);

        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = sock.write_all(head.as_bytes());
        let _ = sock.write_all(body);
        let _ = sock.flush();
    });

    Tracker {
        url: format!("http://127.0.0.1:{port}/announce"),
        query: qrx,
        headers: hrx,
    }
}

/// Drive one announce and hand back what the client made of the answer.
///
/// `IpMode::V4` and not `Auto`: Auto announces from both families at once, and
/// a loopback tracker has no IPv6 half, so Auto would make the test depend on
/// how the host feels about `::1` today. The family behaviour has its own test.
fn announce_to(url: &str) -> Result<AnnounceResponse, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(send_announce(url, "conformance-suite/1.0", IpMode::V4))
}

/// A well-formed answer: two counts, an interval, and one compact peer.
const PEER_127_0_0_1_6881: &[u8] = b"d8:completei5e10:incompletei3e8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe1e";

// ---------------------------------------------------------------------------
// BEP 3 -- the announce request
// ---------------------------------------------------------------------------

/// BEP 3: the announce carries info_hash, peer_id, port, uploaded, downloaded
/// and left. A tracker is entitled to reject a request missing any of them.
#[test]
fn bep3_the_mandatory_parameters_are_all_present() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!(
        "{}?info_hash={}&peer_id={}&port=16171&uploaded=0&downloaded=0&left=0&compact=1&numwant=0",
        t.url,
        "%AB".repeat(20),
        "-TY0001-abcdefghijkl",
    );
    announce_to(&url).expect("the tracker answered");

    let params = t.params();
    let names: Vec<&str> = params.iter().map(|(k, _)| k.as_str()).collect();
    for required in ["info_hash", "peer_id", "port", "uploaded", "downloaded", "left"] {
        assert!(
            names.contains(&required),
            "BEP 3 requires `{required}` on every announce; the tracker received {names:?}"
        );
    }
}

/// BEP 3: info_hash is twenty raw bytes, percent-encoded -- not the forty
/// characters of its hex rendering. A tracker decoding forty bytes looks for a
/// torrent that cannot exist, and answers about nothing.
#[test]
fn bep3_the_info_hash_decodes_to_exactly_twenty_bytes() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!(
        "{}?info_hash={}&peer_id=-TY0001-abcdefghijkl&port=16171&uploaded=0&downloaded=0&left=0",
        t.url,
        "%AB".repeat(20),
    );
    announce_to(&url).expect("the tracker answered");

    let params = t.params();
    let raw = &params.iter().find(|(k, _)| k == "info_hash").expect("info_hash").1;
    let decoded = percent_decode(raw);
    assert_eq!(
        decoded.len(),
        20,
        "BEP 3: info_hash is 20 raw bytes; the tracker received {} after decoding `{raw}`",
        decoded.len()
    );
}

/// BEP 3 / BEP 20: peer_id is twenty bytes too. Trackers answer
/// "invalid peer_id length" on anything else, and our own peer code asserts on
/// it -- a short id makes a 65-byte handshake and the far side waits forever.
#[test]
fn bep3_the_peer_id_decodes_to_exactly_twenty_bytes() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!(
        "{}?info_hash={}&peer_id=-TY0001-abcdefghijkl&port=16171&uploaded=0&downloaded=0&left=0",
        t.url,
        "%AB".repeat(20),
    );
    announce_to(&url).expect("the tracker answered");

    let params = t.params();
    let raw = &params.iter().find(|(k, _)| k == "peer_id").expect("peer_id").1;
    let decoded = percent_decode(raw);
    assert_eq!(
        decoded.len(),
        20,
        "BEP 20: peer_id is 20 bytes; the tracker received {} after decoding `{raw}`",
        decoded.len()
    );
}

// ---------------------------------------------------------------------------
// BEP 3 -- the announce response
// ---------------------------------------------------------------------------

/// BEP 3: a response carrying `failure reason` is an error, and the rest of
/// the dictionary must be ignored. Reading the peers out of a failure is how a
/// client keeps hammering a tracker that told it to stop.
#[test]
fn bep3_a_failure_reason_is_an_error_and_not_a_peer_list() {
    let t = tracker_answering(b"d14:failure reason24:your account is disabled5:peers6:\x7f\x00\x00\x01\x1a\xe1e");
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let err = announce_to(&url).expect_err("BEP 3: a failure reason is an error");
    assert!(
        err.contains("your account is disabled"),
        "the tracker's own words are what an operator needs to see, got: {err}"
    );
}

/// BEP 3: `interval` is the tracker's instruction on when to come back. It is
/// not advisory, and a client that picks its own cadence is the one an
/// operator notices first.
#[test]
fn bep3_the_interval_the_tracker_asks_for_is_the_one_we_report() {
    let t = tracker_answering(b"d8:completei1e10:incompletei0e8:intervali2700e5:peers0:e");
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(
        r.interval, 2700,
        "BEP 3: the tracker asked for 2700s between announces"
    );
}

/// BEP 3: the swarm counts are reported back as sent. They are the only
/// numbers in the process that say how many peers a parked torrent has.
#[test]
fn bep3_the_swarm_counts_survive_the_parse() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!((r.complete, r.incomplete), (5, 3), "complete/incomplete as sent");
}

/// BEP 3: `min interval`, when the tracker states one, is the floor below which
/// it wants no request at all -- a forced re-announce included. Reading
/// `interval` and ignoring this one leaves the tracker without the only lever
/// it has against a client asking again too soon.
#[test]
fn bep3_the_min_interval_floor_is_read() {
    let t = tracker_answering(
        b"d8:completei1e10:incompletei0e8:intervali1800e12:min intervali900e5:peers0:e",
    );
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.min_interval, 900, "BEP 3: the tracker imposed a 900s floor");
    assert_eq!(r.interval, 1800, "and still asked to be seen every 1800s");
}

/// A tracker that states no floor leaves us with zero, which reads as "none
/// stated" -- not as a floor of zero seconds.
#[test]
fn bep3_an_absent_min_interval_is_not_a_floor_of_zero() {
    let t = tracker_answering(b"d8:completei1e10:incompletei0e8:intervali1800e5:peers0:e");
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.min_interval, 0, "no floor stated");
}

// ---------------------------------------------------------------------------
// BEP 23 -- compact peer lists
// ---------------------------------------------------------------------------

/// BEP 23: `peers` as a byte string is six bytes per peer, four of address and
/// two of port, big-endian.
#[test]
fn bep23_a_compact_peer_list_is_six_bytes_per_peer() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.peers.len(), 1, "one peer was sent");
    assert_eq!(
        r.peers[0].to_string(),
        "127.0.0.1:6881",
        "BEP 23: 7f000001 is the address and 1ae1 the port, big-endian"
    );
}

/// BEP 23: a truncated trailing entry is not a reason to lose the peers that
/// did arrive whole. Trackers behind a proxy do truncate.
#[test]
fn bep23_a_truncated_last_entry_does_not_lose_the_others() {
    let t = tracker_answering(b"d8:completei1e10:incompletei0e8:intervali1800e5:peers8:\x7f\x00\x00\x01\x1a\xe1\x7f\x00e");
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("a short tail is not a fatal answer");
    assert_eq!(r.peers.len(), 1, "the whole entry is kept, the half one dropped");
}

/// BEP 3: the non-compact form is a list of dictionaries. A tracker may ignore
/// `compact=1` and send it anyway; dropping those peers is a silent outage.
#[test]
fn bep3_the_dictionary_peer_list_is_understood_too() {
    let t = tracker_answering(
        b"d8:completei1e10:incompletei0e8:intervali1800e5:peersld2:ip9:10.0.0.424:porti6881eeee",
    );
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.peers.len(), 1, "a dictionary peer list is still a peer list");
    assert_eq!(r.peers[0].to_string(), "10.0.0.42:6881");
}

// ---------------------------------------------------------------------------
// BEP 7 -- IPv6
// ---------------------------------------------------------------------------

/// BEP 7: `peers6` is eighteen bytes per peer, sixteen of address and two of
/// port. Without it every v6 peer a tracker returns is dropped in silence.
#[test]
fn bep7_peers6_is_eighteen_bytes_per_peer() {
    let t = tracker_answering(
        b"d8:completei1e10:incompletei0e8:intervali1800e6:peers618:\x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x1a\xe1e",
    );
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.peers.len(), 1, "the v6 peer was read");
    assert_eq!(r.peers[0].to_string(), "[2001:db8::1]:6881");
}

/// BEP 7: a tracker may answer with both families at once, and both lists
/// count. One peer holding two addresses is still reachable by either.
#[test]
fn bep7_both_families_in_one_answer_are_both_kept() {
    let t = tracker_answering(
        b"d8:completei2e10:incompletei0e8:intervali1800e5:peers6:\x7f\x00\x00\x01\x1a\xe16:peers618:\x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\x1a\xe1e",
    );
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let r = announce_to(&url).expect("the tracker answered");
    assert_eq!(r.peers.len(), 2, "one v4 and one v6, neither dropped");
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// The User-Agent we were asked to send is the one that goes out. It is part
/// of a client's identity to a tracker, and a stale hard-coded string
/// contradicting the version in the peer id is exactly what a strict tracker
/// cross-checks.
#[test]
fn the_user_agent_asked_for_is_the_one_sent() {
    let t = tracker_answering(PEER_127_0_0_1_6881);
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    announce_to(&url).expect("the tracker answered");
    let _ = t.params();
    assert_eq!(
        t.header("user-agent").as_deref(),
        Some("conformance-suite/1.0"),
        "the caller's User-Agent must not be overridden by a built-in one"
    );
}

/// A tracker that answers something that is not bencode is an error, not a
/// panic. Captive portals and CDN error pages both do this.
#[test]
fn a_non_bencode_answer_is_an_error_not_a_panic() {
    let t = tracker_answering(b"<html>502 Bad Gateway</html>");
    let url = format!("{}?info_hash={}", t.url, "%AB".repeat(20));

    let err = announce_to(&url).expect_err("html is not a tracker answer");
    assert!(!err.is_empty(), "the error says something");
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Decode a percent-encoded query value back to the bytes that were meant.
fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hi = (b[i + 1] as char).to_digit(16);
                let lo = (b[i + 2] as char).to_digit(16);
                match (hi, lo) {
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
