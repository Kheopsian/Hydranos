#![no_main]
use libfuzzer_sys::fuzz_target;
use typhon_engine::peer::extension::{parse_pex, PeerPolicy};

// First byte selects the policy (IPv6 peers accepted or not); the rest is the
// BEP 11 payload.
fuzz_target!(|data: &[u8]| {
    let Some((&flags, payload)) = data.split_first() else {
        return;
    };
    let policy = PeerPolicy::default();
    policy.set_ipv6(flags & 1 != 0);
    let _ = parse_pex(payload, &policy);
});
