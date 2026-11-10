#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = typhon_engine::peer::extension::parse_extension_handshake(data);
});
