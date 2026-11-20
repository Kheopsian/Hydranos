#![no_main]
use libfuzzer_sys::fuzz_target;

// A `magnet:?` link, as a person pastes it or an RSS item carries it.
fuzz_target!(|data: &[u8]| {
    let Ok(uri) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(m) = typhon_engine::magnet_uri::parse(uri) {
        // Trackers are kept once each and never empty.
        for (i, t) in m.trackers.iter().enumerate() {
            assert!(!t.is_empty() && !m.trackers[..i].contains(t));
        }
    }
});
