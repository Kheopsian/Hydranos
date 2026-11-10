#![no_main]
use libfuzzer_sys::fuzz_target;

// BEP 15 replies. The first byte picks the reply kind and the IP family; the
// transaction id is taken from the packet itself (bytes 4..8) half the time, so
// the parser gets past the "another transaction" check and reaches its body.
fuzz_target!(|data: &[u8]| {
    let Some((&sel, buf)) = data.split_first() else {
        return;
    };
    let tx = if sel & 0x80 != 0 && buf.len() >= 8 {
        u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]])
    } else {
        0
    };
    let hashes = [[0u8; 20], [1u8; 20], [2u8; 20]];
    match sel & 3 {
        0 => {
            let _ = typhon_engine::fuzz_api::udp_connect(buf, tx);
        }
        1 => {
            let _ = typhon_engine::fuzz_api::udp_announce(buf, tx, false);
        }
        2 => {
            let _ = typhon_engine::fuzz_api::udp_announce(buf, tx, true);
        }
        _ => {
            let _ = typhon_engine::fuzz_api::udp_scrape(buf, tx, &hashes);
        }
    }
});
