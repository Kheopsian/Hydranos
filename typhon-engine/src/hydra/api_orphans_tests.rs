//! The labels no category defines, as the Categories tab lists them: what
//! they say, and what reading them costs the store.

use super::testing::*;
use super::*;

const CONFIGURED_ROWS: i64 = 20_000;
const KEY: &str = "0123456789abcdef0123456789abcdef";

/// A library where nearly every torrent wears a configured category, each in
/// a folder of its own (the shape of a real one), and three wear a label that
/// no category defines any more.
fn library() -> TestState {
    let s = state_from("orphans", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    put_category(&s, "tv", "hoard", "/data/tv");
    let store = s.store.lock().unwrap();
    store.conn_for_test(&format!(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {CONFIGURED_ROWS})
         INSERT INTO torrents (info_hash, session, torrent, save_path, category)
         SELECT printf('%040x', i), 'hoard', x'00', '/data/tv/' || i, 'tv' FROM n;
         INSERT INTO torrents (info_hash, session, torrent, save_path, category) VALUES
           ('{a}', 'hoard', x'00', '/old/a', 'old'),
           ('{b}', 'hoard', x'00', '/old/a', 'old'),
           ('{c}', 'race',  x'00', '/old/b', 'old'),
           ('{d}', 'hoard', x'00', '/none', '');",
        a = "a".repeat(40),
        b = "b".repeat(40),
        c = "c".repeat(40),
        d = "d".repeat(40),
    ));
    drop(store);
    s
}

async fn orphans(s: &TestState) -> serde_json::Value {
    let resp = get_categories_orphans(
        axum::extract::State(s.state.clone()),
        axum::extract::RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    body_json(resp).await
}

/// The orphaned label is listed with its count and the folder most of its
/// torrents use; the configured category and the unlabelled row are not.
#[tokio::test]
async fn an_orphaned_label_is_listed_and_a_configured_one_is_not() {
    let s = library();
    let out = orphans(&s).await;
    let list = out.as_array().expect("a list");
    assert_eq!(list.len(), 1, "{out}");
    assert_eq!(list[0]["name"], "old");
    assert_eq!(list[0]["torrents"], 3);
    assert_eq!(list[0]["save_path"], "/old/a");
}

/// ⭐ Listing the orphans does not walk the torrents of the configured
/// categories: the store's work stays far below one VM instruction per such
/// torrent. The old GROUP BY over the whole table did ~10 per row, 16-23 s
/// under the store's lock on a million-torrent library, on every refresh of
/// the Categories tab.
#[tokio::test]
async fn listing_the_orphans_does_not_walk_the_configured_categories() {
    let s = library();
    s.store.lock().unwrap().count_vm_steps_for_test(true);
    let out = orphans(&s).await;
    let steps = s.store.lock().unwrap().count_vm_steps_for_test(false);
    assert_eq!(out.as_array().map(Vec::len), Some(1), "{out}");
    // The counter sits on the connection `lock()` gives. If the route ever
    // reads through another one (a test state with its own read connection),
    // nothing is counted and the bound below would hold for any query: a
    // zero is a broken measure, not a fast route.
    assert!(
        steps > 0,
        "no VM step counted: the route read through another connection"
    );
    assert!(
        steps < CONFIGURED_ROWS as u64 / 10,
        "{steps} VM steps to list one orphaned label beside {CONFIGURED_ROWS} configured torrents"
    );
}
