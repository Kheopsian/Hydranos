//! The conformance document names a test for every claim. This keeps it honest.
//!
//! `docs/BITTORRENT-CONFORMANCE.md` is read by tracker operators deciding
//! whether to allow the client, and every row of its tables ends with the name
//! of the test that asserts the rule. A test that is renamed or deleted leaves
//! the row pointing at nothing, and the document keeps claiming what nobody
//! checks any more. So: every name in the last column of every table must be
//! a test that exists in this crate.

use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut String) {
    for entry in std::fs::read_dir(dir).expect("readable source tree").flatten() {
        let p = entry.path();
        if p.is_dir() {
            if p.file_name().map_or(false, |n| n == "target") {
                continue;
            }
            sources(&p, out);
        } else if p.extension().map_or(false, |e| e == "rs") {
            out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
        }
    }
}

/// Backticked identifiers in the last cell of each row of every table whose
/// last column is headed "Test". Other tables quote protocol words in
/// backticks too, and those are not test names.
fn named_tests(doc: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_test_table = false;
    let mut header_next = true;
    for line in doc.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            header_next = true;
            in_test_table = false;
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        let Some(last) = cells.last() else { continue };
        if header_next {
            in_test_table = last.trim() == "Test";
            header_next = false;
            continue;
        }
        if line.starts_with("|---") || !in_test_table {
            continue;
        }
        for piece in last.split('`').skip(1).step_by(2) {
            if !piece.is_empty() && piece.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                out.push(piece.to_string());
            }
        }
    }
    out
}

#[test]
fn every_test_the_conformance_document_names_exists() {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let doc = std::fs::read_to_string(crate_dir.join("../docs/BITTORRENT-CONFORMANCE.md"))
        .expect("docs/BITTORRENT-CONFORMANCE.md is there");
    let mut code = String::new();
    sources(&crate_dir.join("src"), &mut code);
    sources(&crate_dir.join("tests"), &mut code);

    let names = named_tests(&doc);
    assert!(names.len() > 100, "the parser found only {} names -- it is the parser that broke", names.len());
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !code.contains(&format!("fn {n}(")))
        .collect();
    assert!(missing.is_empty(), "the document names tests that do not exist: {missing:?}");
}
