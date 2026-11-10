#![no_main]
use libfuzzer_sys::fuzz_target;

// The body an HTTP tracker answers an announce with.
fuzz_target!(|data: &[u8]| {
    let _ = typhon_engine::fuzz_api::http_announce_response(data);
});
