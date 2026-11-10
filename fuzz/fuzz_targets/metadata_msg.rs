#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(m) = typhon_engine::peer::extension::parse_metadata_message(data) {
        // The block of a `data` message starts where the dict ended.
        assert!(m.data_offset <= data.len());
    }
});
