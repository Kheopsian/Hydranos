#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = typhon_engine::torrent::metainfo::piece_hashes_from_bytes(data);
});
