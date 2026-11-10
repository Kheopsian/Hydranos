//! What the fuzz crate (`fuzz/`) needs from parsers that are not `pub`.
//!
//! Compiled only under `--cfg fuzzing` (see lib.rs). Each function is a thin
//! forwarder with no logic of its own, so the fuzzer exercises exactly the code
//! that runs in production.

/// An HTTP tracker's announce reply body (bencoded).
pub fn http_announce_response(data: &[u8]) -> bool {
    crate::tracker::http::parse_announce_response(data).is_ok()
}

/// A UDP tracker's connect reply.
pub fn udp_connect(buf: &[u8], tx: u32) -> bool {
    crate::tracker::udp::parse_connect(buf, tx).is_ok()
}

/// A UDP tracker's announce reply, IPv4 or IPv6 peer format.
pub fn udp_announce(buf: &[u8], tx: u32, v6: bool) -> bool {
    crate::tracker::udp::parse_announce(buf, tx, v6).is_ok()
}

/// A UDP tracker's scrape reply for `hashes`.
pub fn udp_scrape(buf: &[u8], tx: u32, hashes: &[[u8; 20]]) -> bool {
    crate::tracker::udp::parse_scrape(buf, tx, hashes).is_ok()
}
