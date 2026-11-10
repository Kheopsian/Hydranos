//! The RSS tab's own routes where the qBittorrent shim's tests do not go:
//! folders, rules (save, list, rename, remove), the overview, "refresh
//! all", and what each says to a feed that is not there.

use super::*;

fn parsed(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

const RULE: &str = r#"{"enabled":true,"mustContain":"epub","assignedCategory":"books"}"#;

/// ⭐ A rule is saved under its name, listed, renamed (with either key the
/// UI and the API have used) and removed, and each refusal says which rule
/// it was about.
#[tokio::test]
async fn a_rule_is_saved_listed_renamed_and_removed_by_name() {
    let s = st("rss-rules", "");

    let (code, body) = call(&s, "PUT", "/api/rss/rules/epub", RULE).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v = parsed(&body);
    assert_eq!(v["name"], "epub");
    assert_eq!(v["mustContain"], "epub");
    assert_eq!(v["assignedCategory"], "books");
    let (code, body) = call(&s, "PUT", "/api/rss/rules/other", RULE).await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (code, body) = call(&s, "GET", "/api/rss/rules", "").await;
    assert_eq!(code, StatusCode::OK);
    let names: Vec<String> = parsed(&body)
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["epub", "other"]);

    let (code, body) = call(
        &s,
        "POST",
        "/api/rss/rules/epub/rename",
        r#"{"name":" books "}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(parsed(&body), json!({"name": "books"}), "trimmed");
    let (code, body) = call(
        &s,
        "POST",
        "/api/rss/rules/books/rename",
        r#"{"new_name":"ebooks"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(parsed(&body), json!({"name": "ebooks"}));

    let (code, body) = call(
        &s,
        "POST",
        "/api/rss/rules/ebooks/rename",
        r#"{"name":"other"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        parsed(&body)["error"],
        "a rule named \"other\" already exists"
    );
    let (code, body) = call(&s, "POST", "/api/rss/rules/ghost/rename", r#"{"name":"x"}"#).await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(parsed(&body)["error"], "no rule \"ghost\"");
    let (code, body) = call(&s, "POST", "/api/rss/rules/ebooks/rename", "{}").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(parsed(&body)["error"], "a rule needs a name");

    let (code, body) = call(&s, "DELETE", "/api/rss/rules/ebooks", "").await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(parsed(&body), json!({"removed": "ebooks"}));
    let (code, body) = call(&s, "DELETE", "/api/rss/rules/ebooks", "").await;
    assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(parsed(&body)["error"], "no rule \"ebooks\"");
    let (_, body) = call(&s, "GET", "/api/rss/rules", "").await;
    let left = parsed(&body);
    assert_eq!(left.as_array().unwrap().len(), 1);
    assert_eq!(left[0]["name"], "other");
}

/// A rule that is not a rule, with no name or a body that is not JSON, is
/// refused as a bad request and nothing is saved.
#[tokio::test]
async fn a_rule_that_cannot_be_saved_says_so_and_saves_nothing() {
    let s = st("rss-rules-bad", "");
    let (code, body) = call(&s, "PUT", "/api/rss/rules/epub", "this is not json").await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    let (code, body) = call(&s, "PUT", "/api/rss/rules/%20", RULE).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(parsed(&body)["error"], "a rule needs a name");
    let (_, body) = call(&s, "GET", "/api/rss/rules", "").await;
    assert_eq!(parsed(&body), json!([]));
    let (code, _) = call(&s, "GET", "/api/rss/rules/ghost/matching", "").await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

/// Folders are made, and a second one at the same place is a conflict that
/// names the place.
#[tokio::test]
async fn a_folder_is_created_once() {
    let s = st("rss-folders", "");
    let (code, body) = call(&s, "POST", "/api/rss/folders", r#"{"path":"Books"}"#).await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    assert_eq!(parsed(&body), json!({"folders": ["Books"]}));
    let (code, body) = call(
        &s,
        "POST",
        "/api/rss/folders",
        r#"{"path":"Books\\Fiction"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    assert_eq!(
        parsed(&body),
        json!({"folders": ["Books", "Books\\Fiction"]})
    );

    let (code, body) = call(&s, "POST", "/api/rss/folders", r#"{"path":"Books"}"#).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        parsed(&body)["error"],
        "an item already exists at \"Books\""
    );
    let (code, _) = call(&s, "POST", "/api/rss/folders", r#"{"path":""}"#).await;
    assert_eq!(code, StatusCode::BAD_REQUEST, "no path is no folder");
    assert_eq!(folders(&s), ["Books", "Books\\Fiction"]);
}

/// The overview carries what the tab draws: the switch, the limits, and every
/// feed, folder and rule; "refresh all" says how many feeds it queued.
#[tokio::test]
async fn the_overview_and_refresh_all_say_what_there_is() {
    let s = st("rss-overview", "");
    let fake = Fake::start().await;
    fake.serve("/a", 200, feed_xml(&[]));
    fake.serve("/b", 200, feed_xml(&[]));
    for (name, path) in [("Alpha", "/a"), ("Beta", "/b")] {
        let (code, body) = call(
            &s,
            "POST",
            "/api/rss/feeds",
            &json!({"url": fake.url(path), "name": name}).to_string(),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{body}");
    }
    call(&s, "POST", "/api/rss/folders", r#"{"path":"Empty"}"#).await;
    call(&s, "PUT", "/api/rss/rules/epub", RULE).await;

    let (code, body) = call(&s, "GET", "/api/rss", "").await;
    assert_eq!(code, StatusCode::OK);
    let v = parsed(&body);
    assert_eq!(v["enabled"], true);
    assert_eq!(v["min_refresh_secs"], MIN_REFRESH_SECS);
    assert_eq!(v["max_articles"], settings(&s.cfg()).max_articles);
    assert_eq!(v["default_refresh_secs"], settings(&s.cfg()).refresh_secs);
    assert_eq!(v["feeds"].as_array().unwrap().len(), 2);
    assert_eq!(v["folders"], json!(["Empty"]));
    assert_eq!(v["rules"][0]["name"], "epub");
    let (_, body) = call(&s, "GET", "/api/rss/feeds", "").await;
    assert_eq!(parsed(&body), v["feeds"], "the same feeds as the list");

    let (code, body) = call(&s, "POST", "/api/rss/refresh", "").await;
    assert_eq!(code, StatusCode::ACCEPTED, "{body}");
    assert_eq!(parsed(&body), json!({"refreshing": 2}));
}

/// A feed that is not there is a 404 that names its number, on every route
/// that takes one.
#[tokio::test]
async fn a_feed_that_is_not_there_is_named_in_every_refusal() {
    let s = st("rss-nofeed", "");
    for (method, uri) in [
        ("GET", "/api/rss/feeds/99/articles"),
        ("POST", "/api/rss/feeds/99/read"),
    ] {
        let (code, body) = call(&s, method, uri, "{}").await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{uri}: {body}");
        assert_eq!(parsed(&body)["error"], "no feed 99", "{uri}");
    }
}

/// Reading: one article by its guid, or the whole feed; the answer counts
/// what was marked, and the unread filter lists what is left.
#[tokio::test]
async fn articles_are_marked_read_one_by_one_or_all_at_once() {
    let s = st("rss-read", "");
    let fake = Fake::start().await;
    fake.serve(
        "/rss",
        200,
        feed_xml(&[
            ("g1", "One", &fake.url("/1.torrent")),
            ("g2", "Two", &fake.url("/2.torrent")),
            ("g3", "Three", &fake.url("/3.torrent")),
        ]),
    );
    let (_, body) = call(
        &s,
        "POST",
        "/api/rss/feeds",
        &json!({"url": fake.url("/rss"), "name": "Feed"}).to_string(),
    )
    .await;
    let id = parsed(&body)["id"].as_i64().unwrap();
    let (code, _) = call(&s, "POST", &format!("/api/rss/feeds/{id}/refresh"), "").await;
    assert_eq!(code, StatusCode::OK);
    let unread = |q: &str| {
        let uri = format!("/api/rss/feeds/{id}/articles{q}");
        let s = &s;
        async move {
            let (_, body) = call(s, "GET", &uri, "").await;
            let mut g: Vec<String> = parsed(&body)
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["guid"].as_str().unwrap().to_string())
                .collect();
            g.sort();
            g
        }
    };
    assert_eq!(unread("").await, ["g1", "g2", "g3"]);

    let (code, body) = call(
        &s,
        "POST",
        &format!("/api/rss/feeds/{id}/read"),
        r#"{"guid":"g2"}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(parsed(&body), json!({"marked": 1}));
    assert_eq!(unread("?unread=1").await, ["g1", "g3"]);
    assert_eq!(unread("?unread=true").await, ["g1", "g3"]);
    assert_eq!(unread("?unread=0").await, ["g1", "g2", "g3"]);

    let (_, body) = call(&s, "POST", &format!("/api/rss/feeds/{id}/read"), "{}").await;
    assert_eq!(parsed(&body), json!({"marked": 2}), "the two still unread");
    assert!(unread("?unread=1").await.is_empty());
}
