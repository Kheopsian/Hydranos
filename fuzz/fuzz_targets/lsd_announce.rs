#![no_main]
use libfuzzer_sys::fuzz_target;
use std::net::SocketAddr;
use typhon_engine::lsd;

// One BEP 14 `BT-SEARCH` datagram, as read off the LAN multicast group. What
// parses must survive being written back: `build` then `parse` keeps the port
// and the info hashes (the cookie is free text and is not compared).
fuzz_target!(|data: &[u8]| {
    let Some(a) = lsd::parse(data) else {
        return;
    };
    assert!(!a.info_hashes.is_empty() && a.port != 0);
    let group = SocketAddr::from((lsd::LSD_V4, lsd::LSD_PORT));
    let again = lsd::parse(lsd::build(group, a.port, &a.info_hashes, "").as_bytes())
        .expect("a built message parses");
    assert_eq!(again.port, a.port);
    assert_eq!(again.info_hashes, a.info_hashes);
});
