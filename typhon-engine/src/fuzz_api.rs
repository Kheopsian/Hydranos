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

// ---- MSE (BEP 8 handshake) -------------------------------------------------
//
// The handshake generates a random key pair per connection, so no input could
// ever carry a valid `req1` hash or a decryptable tail. These forwarders take
// the private key as a seed instead; the harness derives the shared secret and
// encrypts the input with it, and the fuzzer reaches the stages behind the
// key exchange.

/// The 96-byte public key of the private key `seed`.
pub fn mse_public(seed: &[u8; 20]) -> Vec<u8> {
    crate::crypto::mse::keypair_from(seed).1
}

/// The shared secret between the private key `seed` and `their_public`.
pub fn mse_secret(their_public: &[u8], seed: &[u8; 20]) -> Vec<u8> {
    let (x, _) = crate::crypto::mse::keypair_from(seed);
    crate::crypto::mse::compute_secret(their_public, &x)
}

/// An RC4 stream keyed as the handshake keys it, 1024 bytes discarded:
/// `prefix` is `keyA` or `keyB`.
pub fn mse_rc4(prefix: &[u8], secret: &[u8], skey: &[u8]) -> crate::crypto::rc4::Rc4 {
    crate::crypto::mse::make_rc4_keys(prefix, secret, skey)
}

/// The receiving side, reading `wire` after the first byte of the peer's public
/// key (`first_byte`, then `remaining_ya` already read). Only `skey` is a known
/// torrent.
pub async fn mse_incoming(
    wire: &[u8],
    first_byte: u8,
    remaining_ya: &[u8],
    seed: &[u8; 20],
    skey: [u8; 20],
    rc4_only: bool,
) -> bool {
    let (x, y) = crate::crypto::mse::keypair_from(seed);
    let mut stream = tokio::io::join(wire, tokio::io::sink());
    let known = crate::crypto::mse::sha1_combine(b"req2", &skey);
    crate::crypto::mse::handshake_incoming_keyed(
        &mut stream,
        first_byte,
        remaining_ya,
        &[0u8; 20],
        |h| (*h == known).then_some(skey),
        rc4_only,
        x,
        y,
    )
    .await
    .is_ok()
}

/// The initiating side, reading `wire` (the peer's whole answer, public key
/// included).
pub async fn mse_outgoing(wire: &[u8], seed: &[u8; 20], skey: [u8; 20], rc4_only: bool) -> bool {
    let (x, y) = crate::crypto::mse::keypair_from(seed);
    let mut stream = tokio::io::join(wire, tokio::io::sink());
    crate::crypto::mse::handshake_outgoing_keyed(&mut stream, &skey, &[0u8; 20], rc4_only, x, y)
        .await
        .is_ok()
}
