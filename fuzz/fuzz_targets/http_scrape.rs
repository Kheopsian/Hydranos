#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = typhon_engine::tracker::scrape::parse_http_scrape(data);
});
