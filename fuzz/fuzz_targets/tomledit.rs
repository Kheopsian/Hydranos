#![no_main]
use libfuzzer_sys::fuzz_target;

// The text-level TOML editor of the `hydranos` binary crate, which edits the
// operator's config file in place. Its only dependency is serde_json (for
// `toml_scalar`), so the source file is compiled straight into the target:
// the fuzzer runs the code the daemon runs, not a copy.
#[allow(dead_code)]
#[path = "../../typhon-engine/src/hydra/tomledit.rs"]
mod tomledit;

use tomledit::*;

// The input is five NUL-separated strings: the document, a section, a key, a
// value and an id. Every editor runs on them; what the editors write into a
// document must not make a later edit of the same document panic either.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let mut parts = text.splitn(5, '\0');
    let (doc, section, key, value, id) = (
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
        parts.next().unwrap_or(""),
    );
    let kv = [(key.to_string(), value.to_string())];
    let session = [(key.to_string(), quote_toml_key(value))];

    let _ = quote_toml_key(key);
    let _ = set_toml_value(doc, section, key, value);
    let _ = delete_toml_key(doc, section, key);
    let _ = prune_empty_table(doc, section);
    let _ = delete_agent_block(doc, id);
    let _ = set_agent_session_key(doc, id, key, value);
    let _ = delete_agent_session_key(doc, id, key);
    if let Ok(edited) = set_toml_table(doc, section, &kv) {
        let _ = delete_toml_key(&edited, section, key);
        let _ = prune_empty_table(&edited, section);
    }
    let added = append_agent_block(doc, id, value, &session);
    let _ = delete_agent_block(&added, id);
    let _ = set_agent_session_key(&added, id, key, value);
    let _ = delete_agent_session_key(&added, id, key);
});
