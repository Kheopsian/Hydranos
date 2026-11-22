use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn torrent_bytes(name: &str) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    info.extend_from_slice(&[0u8; 20]);
    info.extend_from_slice(b"e");
    let mut out = Vec::new();
    out.extend_from_slice(b"d4:infod");
    out.extend_from_slice(&info[1..]);
    out.extend_from_slice(b"e");
    out
}

/// A node holding one torrent in the `race` engine.
fn populated(tag: &str) -> (TestState, String) {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let (hash, _) = add_torrent_bytes(
        &s.state,
        &torrent_bytes("alpha"),
        "",
        "/tmp",
        "fr",
        true,
        true,
        "race",
    )
    .expect("added");
    (s, hash)
}

fn form(hashes: &str) -> Fields {
    let mut f = Fields::new();
    f.insert("hashes".into(), hashes.into());
    f
}

/// ⭐⭐ Both routes used to be `qbit_empty_ok`, which was covered by the
/// auth-gate macro. Replacing them must not drop that coverage: an open
/// mutation route is exactly the hole found on 10/09.
#[tokio::test]
async fn the_bulk_routes_refuse_a_caller_with_no_key() {
    let (s, hash) = populated("qbit-bulk-auth");
    for resp in [
        super::qbit_reannounce(
            State(s.state.clone()),
            RawQuery(None),
            HeaderMap::new(),
            axum::extract::Form(form(&hash)),
        )
        .await,
        super::qbit_recheck(
            State(s.state.clone()),
            RawQuery(None),
            HeaderMap::new(),
            axum::extract::Form(form(&hash)),
        )
        .await,
    ] {
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "a bulk mutation must refuse an unauthenticated caller"
        );
    }
}

/// The qBittorrent contract is an empty 200, and clients break on anything
/// else -- so the answer stays 200 even when nothing could be done. What
/// changed is that the work is attempted; the counts go to the log.
///
/// ⚠ This asserts the CONTRACT, not the effect. The effect is not visible in
/// the response by design, which is why it is checked against the log line
/// on the :8399 bench rather than pretended here.
#[tokio::test]
async fn the_bulk_routes_keep_the_empty_200_contract() {
    let (s, hash) = populated("qbit-bulk-contract");
    for (name, resp) in [
        (
            "reannounce",
            super::qbit_reannounce(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                axum::extract::Form(form(&hash)),
            )
            .await,
        ),
        (
            "recheck",
            super::qbit_recheck(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                axum::extract::Form(form(&hash)),
            )
            .await,
        ),
    ] {
        assert_eq!(resp.status(), StatusCode::OK, "{name} answers 200");
    }
}

/// A hash the node does not hold, and a form with no `hashes` at all: both
/// are ordinary, and neither may panic or turn into a 500. The old stub got
/// this right by doing nothing at all -- the new one has to get it right
/// while actually looking things up.
#[tokio::test]
async fn an_unknown_hash_or_an_empty_form_is_not_an_error() {
    let (s, _hash) = populated("qbit-bulk-unknown");
    let cases = [form(&"f".repeat(40)), Fields::new()];
    for f in cases {
        for resp in [
            super::qbit_reannounce(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                axum::extract::Form(f.clone()),
            )
            .await,
            super::qbit_recheck(
                State(s.state.clone()),
                RawQuery(None),
                keyed(KEY),
                axum::extract::Form(f.clone()),
            )
            .await,
        ] {
            assert_eq!(resp.status(), StatusCode::OK, "got {:?}", resp.status());
        }
    }
}

/// ⭐ The fixture is under test too: if `populated` silently failed to load
/// the torrent, every assertion above would pass against an empty node and
/// prove nothing about a node that holds something.
#[tokio::test]
async fn the_fixture_really_holds_the_torrent() {
    let (s, hash) = populated("qbit-bulk-fixture");
    let copies = copies_of(&s.state, &hash);
    assert!(
        !copies.is_empty(),
        "the fixture must hold a copy of {hash}, found none"
    );
    assert!(
        copies.iter().any(|(id, _)| id == "race"),
        "the copy must live in the race engine, got {:?}",
        copies.iter().map(|(id, _)| id).collect::<Vec<_>>()
    );
}
