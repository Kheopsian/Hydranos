#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = typhon_engine::torrent::metainfo::parse_torrent_bytes(data);
});
