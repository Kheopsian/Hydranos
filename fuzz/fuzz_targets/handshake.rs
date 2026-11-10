#![no_main]
use libfuzzer_sys::fuzz_target;

// The 68-byte BitTorrent handshake. Shorter input is zero-padded, longer is cut,
// so every input reaches the parser the way a read of exactly 68 bytes does.
fuzz_target!(|data: &[u8]| {
    let mut buf = [0u8; 68];
    let n = data.len().min(68);
    buf[..n].copy_from_slice(&data[..n]);
    let _ = typhon_engine::peer::handshake::parse_handshake(&buf);
});
