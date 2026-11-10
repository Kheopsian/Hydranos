#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((value, used)) = typhon_engine::peer::bencode::decode_prefix(data) {
        // The consumed length is an index into the input: it must be in range,
        // and what was decoded must survive being written back out.
        assert!(used <= data.len());
        let _ = value.to_vec();
    }
});
