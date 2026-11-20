#![no_main]
use libfuzzer_sys::fuzz_target;

// The PROXY protocol v2 header a front proxy puts before the BitTorrent
// handshake: 16 fixed bytes, then `len` bytes (at most 536) the parser reads.
// The stream is the input itself, so a short input is an unexpected EOF.
fuzz_target!(|data: &[u8]| {
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let _ = rt.block_on(typhon_engine::peer::proxy_protocol::parse_v2(data));
});
