#![no_main]
use libfuzzer_sys::fuzz_target;

// The RSS / Atom reader of the `hydranos` binary crate. That crate cannot be
// depended on from here, but this file has no dependency of its own (std only),
// so it is compiled straight into the target from its source path: the fuzzer
// runs the very code the daemon runs, not a copy.
#[allow(dead_code)]
#[path = "../../typhon-engine/src/hydra/rss/parse.rs"]
mod parse;

// A feed body, as a tracker or indexer serves it (the daemon caps it in size
// before it gets here). The first byte picks what is read: a feed, or a date.
fuzz_target!(|data: &[u8]| {
    let Some((&sel, body)) = data.split_first() else {
        return;
    };
    if sel & 1 == 0 {
        let _ = parse::parse(body);
    } else if let Ok(s) = std::str::from_utf8(body) {
        // A date that reads can be written back out.
        if let Some(ts) = parse::parse_date(s) {
            let _ = parse::rfc2822(ts);
        }
    }
});
