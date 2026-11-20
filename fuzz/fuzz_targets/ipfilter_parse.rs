#![no_main]
use libfuzzer_sys::fuzz_target;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use typhon_engine::ipfilter::IpFilter;

// An IP block list as an operator loads it (PeerGuardian, eMule dat, CIDR,
// plain ranges, mixed). The first 16 bytes are an address to look up, the rest
// is the list. A list unioned with itself is the same list.
fuzz_target!(|data: &[u8]| {
    if data.len() < 16 {
        return;
    }
    let (probe, rest) = data.split_at(16);
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let mut b = [0u8; 16];
    b.copy_from_slice(probe);
    let v6 = IpAddr::V6(Ipv6Addr::from(b));
    let v4 = IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]));

    let (f, stats) = IpFilter::parse(text);
    assert!(f.len() <= stats.ranges);
    let both = IpFilter::union(&[f.clone(), f.clone()]);
    assert_eq!(both, f);
    for ip in [v4, v6] {
        assert_eq!(f.contains(ip), both.contains(ip));
    }
});
