#![no_main]
use libfuzzer_sys::fuzz_target;

// First byte picks the leaf count (a power of two up to 4096, as a v2 piece
// layer is); the rest is the file content. Unbounded `leaves` would only
// measure the allocator, not the hashing.
fuzz_target!(|data: &[u8]| {
    let Some((&n, body)) = data.split_first() else {
        return;
    };
    let _ = typhon_engine::torrent::merkle::root(body, 1usize << (n % 13));
});
