//! RSS end to end: a fake HTTP server on 127.0.0.1, the real add path.

use super::*;
use crate::api::testing::*;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// An HTTP server that answers what it is told per path and records every
/// request (path with its query, and the User-Agent).
struct Fake {
    port: u16,
    routes: Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>,
    hits: Arc<Mutex<Vec<(String, String)>>>,
}

impl Fake {
    async fn start() -> Fake {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let routes: Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>> = Default::default();
        let hits: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let (r, h) = (routes.clone(), hits.clone());
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let (r, h) = (r.clone(), h.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match s.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).to_string();
                    let target = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let ua = head
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("user-agent:"))
                        .map(|l| l[11..].trim().to_string())
                        .unwrap_or_default();
                    h.lock().unwrap().push((target.clone(), ua));
                    let path = target.split('?').next().unwrap_or("/").to_string();
                    let (code, body) = r.lock().unwrap().get(&path).cloned().unwrap_or((404, b"nope".to_vec()));
                    let head = format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        Fake { port, routes, hits }
    }

    fn serve(&self, path: &str, code: u16, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(path.to_string(), (code, body.into()));
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().iter().filter(|(t, _)| t.split('?').next() == Some(path)).count()
    }
}

fn torrent_bytes(name: &str) -> Vec<u8> {
    let info = format!("d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e", name.len(), "N".repeat(20));
    let announce = "https://tracker.example/announce";
    format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
}

/// An RSS 2.0 document: (guid, title, link) per item, the link as a torrent
/// enclosure.
fn feed_xml(items: &[(&str, &str, &str)]) -> String {
    let mut s = String::from("<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><title>Fake tracker</title>");
    for (guid, title, link) in items {
        let link = link.replace('&', "&amp;");
        s.push_str(&format!(
            "<item><title>{title}</title><guid>{guid}</guid>\
             <enclosure url=\"{link}\" type=\"application/x-bittorrent\" length=\"16384\"/></item>"
        ));
    }
    s.push_str("</channel></rss>");
    s
}

fn st(tag: &str, extra: &str) -> TestState {
    let s = state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n{extra}"));
    let lib = s.dir.join("lib").display().to_string();
    put_category(&s.state, "books", "hoard", &lib);
    s
}

fn count(s: &TestState, engine: &str) -> usize {
    s.engines.get(engine).map(|e| e.manager.count()).unwrap_or(0)
}

/// A rule's article is added by the API's add: the category picks the engine
/// (hoard here, where an add without one goes to race), and the activity
/// trail says so. Taken once, it is never taken again -- not by a second
/// pass, not when the feed republishes the same link under a new guid.
#[tokio::test]
async fn a_rule_adds_through_the_api_path_and_never_twice() {
    let s = st("rss-add", "");
    let fake = Fake::start().await;
    let t1 = fake.url("/dl/1.torrent?passkey=SECRET");
    fake.serve("/dl/1.torrent", 200, torrent_bytes("book1.epub"));
    fake.serve("/dl/2.torrent", 200, torrent_bytes("other.mkv"));
    fake.serve("/rss", 200, feed_xml(&[("g1", "Some.Book.EPUB", &t1), ("g2", "Some.Film.1080p", &fake.url("/dl/2.torrent"))]));
    let feed_url = fake.url("/rss?passkey=SECRET");
    let f = add_feed(&s, &feed_url, "Trackers\\Books", None).unwrap();
    assert_eq!(folders(&s), vec!["Trackers"], "the folder of the path is created");
    put_rule(&s, "books", &json!({"mustContain": "book epub", "affectedFeeds": [feed_url], "assignedCategory": "books"})).unwrap();

    let row = refresh_feed(&s, f.id).await.unwrap();
    assert_eq!(row.last_error, "");
    assert_eq!(row.title, "Fake tracker");
    assert_eq!(count(&s, "hoard"), 1, "added, and to the category's engine");
    assert_eq!(count(&s, "race"), 0);
    assert_eq!(fake.hits("/dl/2.torrent"), 0, "what no rule wants is not fetched");
    let ua = fake.hits.lock().unwrap()[0].1.clone();
    assert!(ua.starts_with("Hydranos/"), "the client's User-Agent: {ua}");

    let (arts, taken, act) = {
        let store = s.store.lock().unwrap();
        (store.rss_articles(f.id).unwrap(), store.rss_taken(&["g1"]).unwrap(), store.workflow_activity(10).unwrap())
    };
    assert!(arts.iter().find(|a| a.guid == "g1").unwrap().is_read, "a taken article is read");
    assert!(!arts.iter().find(|a| a.guid == "g2").unwrap().is_read);
    let taken = taken.expect("recorded as taken");
    assert_eq!((taken.rule.as_str(), taken.outcome.as_str()), ("books", "added"));
    assert_eq!(act.len(), 1);
    assert_eq!((act[0].workflow_name.as_str(), act[0].outcome.as_str()), ("RSS: books", "applied"));
    assert_eq!(act[0].info_hash, taken.info_hash);
    let rule = load_rules(&s).into_iter().find(|r| r.name == "books").unwrap();
    assert!(rule.last_match > 0, "lastMatch is kept");

    // The same link under another guid: recognised by the link.
    fake.serve("/rss", 200, feed_xml(&[("g1-again", "Some.Book.EPUB (repost)", &t1)]));
    refresh_feed(&s, f.id).await.unwrap();
    assert_eq!(run_rules(&s, None).await, 0);
    assert_eq!(fake.hits("/dl/1.torrent"), 1, "fetched once, ever");
    assert_eq!(count(&s, "hoard"), 1);
}

/// `ignoreDays`: of two matching articles of the same day, one is taken.
#[tokio::test]
async fn ignore_days_lets_one_article_of_the_period_through() {
    let s = st("rss-ignore", "");
    let fake = Fake::start().await;
    fake.serve("/a.torrent", 200, torrent_bytes("a.epub"));
    fake.serve("/b.torrent", 200, torrent_bytes("b.epub"));
    fake.serve("/rss", 200, feed_xml(&[("a", "Book A EPUB", &fake.url("/a.torrent")), ("b", "Book B EPUB", &fake.url("/b.torrent"))]));
    let f = add_feed(&s, &fake.url("/rss"), "", None).unwrap();
    assert_eq!(f.path, "127.0.0.1:".to_string() + &fake.port.to_string(), "named after the host, not the URL");
    put_rule(&s, "once", &json!({"mustContain": "epub", "affectedFeeds": [fake.url("/rss")], "assignedCategory": "books", "ignoreDays": 1}))
        .unwrap();
    refresh_feed(&s, f.id).await.unwrap();
    assert_eq!(count(&s, "hoard"), 1);
    assert_eq!(fake.hits("/a.torrent") + fake.hits("/b.torrent"), 1);
    // Still one on the next pass: the other is dated inside the period.
    assert_eq!(run_rules(&s, None).await, 0);
}

/// A magnet goes through the magnet queue, to the category's engine.
#[tokio::test]
async fn a_magnet_article_goes_through_the_magnet_queue() {
    let s = st("rss-magnet", "");
    let fake = Fake::start().await;
    let hash = "c9e15763f722f23e98a29decdfae341b98d53056";
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=Book&tr=https://t.example/announce?passkey=SECRET");
    fake.serve("/rss", 200, feed_xml(&[("m", "Book EPUB", &magnet)]));
    let f = add_feed(&s, &fake.url("/rss"), "M", None).unwrap();
    put_rule(&s, "m", &json!({"mustContain": "book", "affectedFeeds": [fake.url("/rss")], "assignedCategory": "books"})).unwrap();
    refresh_feed(&s, f.id).await.unwrap();
    let rows = s.store.lock().unwrap().magnets().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].info_hash.as_str(), rows[0].engine.as_str(), rows[0].category.as_str()), (hash, "hoard", "books"));
}

/// An add the add path refuses (no save path: a category nobody defined)
/// is recorded with the reason, and not tried again.
#[tokio::test]
async fn a_refused_add_says_why_and_is_not_retried() {
    let s = st("rss-refused", "");
    let fake = Fake::start().await;
    fake.serve("/x.torrent", 200, torrent_bytes("x.epub"));
    fake.serve("/rss", 200, feed_xml(&[("x", "X EPUB", &fake.url("/x.torrent"))]));
    let f = add_feed(&s, &fake.url("/rss"), "R", None).unwrap();
    put_rule(&s, "nowhere", &json!({"mustContain": "epub", "affectedFeeds": [fake.url("/rss")], "assignedCategory": "unknown"})).unwrap();
    refresh_feed(&s, f.id).await.unwrap();
    assert_eq!(count(&s, "race") + count(&s, "hoard"), 0);
    let act = s.store.lock().unwrap().workflow_activity(10).unwrap();
    assert_eq!(act[0].outcome, "failed");
    assert!(act[0].detail.contains("no save path"), "{}", act[0].detail);
    let t = s.store.lock().unwrap().rss_taken(&["x"]).unwrap().unwrap();
    assert!(t.outcome.starts_with("refused: "), "{}", t.outcome);
    assert_eq!(run_rules(&s, None).await, 0);
    assert_eq!(fake.hits("/x.torrent"), 1);
}

/// With the kill switch keeping the engine off the network (WireGuard mode,
/// no engine assigned a tunnel), nothing is fetched and the feed says why.
#[tokio::test]
async fn a_closed_way_out_fetches_nothing_and_says_why() {
    let s = st("rss-killswitch", "\n[network]\nmode = \"wireguard\"\n");
    let fake = Fake::start().await;
    fake.serve("/rss", 200, feed_xml(&[]));
    let f = add_feed(&s, &fake.url("/rss"), "K", None).unwrap();
    let row = refresh_feed(&s, f.id).await.unwrap();
    assert!(row.last_error.starts_with("not fetched"), "{}", row.last_error);
    assert!(row.last_error.contains("race"), "names the engine: {}", row.last_error);
    assert_eq!(fake.hits("/rss"), 0, "not a single request");
}

/// The feed leaves by the engine's announce proxy, the route its announces
/// take: with that proxy dead, the tracker is not contacted directly.
#[tokio::test]
async fn the_feed_leaves_by_the_engines_announce_proxy() {
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = dead.local_addr().unwrap().port();
    drop(dead);
    let s = st("rss-proxy", &format!("\n[race]\nannounce_proxy = \"socks5h://127.0.0.1:{port}\"\n"));
    let (route, via) = Exits::new(&s).of(&s, "race").unwrap();
    assert_eq!(route.proxy, format!("socks5h://127.0.0.1:{port}"));
    assert!(via.contains("announce proxy of engine race"), "{via}");
    let fake = Fake::start().await;
    fake.serve("/rss", 200, feed_xml(&[]));
    let f = add_feed(&s, &fake.url("/rss"), "P", None).unwrap();
    let row = refresh_feed(&s, f.id).await.unwrap();
    assert!(!row.last_error.is_empty(), "the dead proxy fails the fetch");
    assert_eq!(fake.hits("/rss"), 0, "never sent around the proxy");
}

/// The passkey is in no error, no feed state, no activity line.
#[tokio::test]
async fn the_passkey_never_shows_in_errors() {
    assert_eq!(
        mask_url("https://user:pw@t.example/rss/0123456789abcdef0123456789abcdef/feed.xml?passkey=SECRET&cat=1"),
        "https://t.example/rss/***/feed.xml?***"
    );
    assert_eq!(mask_url("https://t.example/rss"), "https://t.example/rss");
    assert_eq!(
        mask_url("magnet:?xt=urn:btih:abc&tr=https://t.example/a?passkey=SECRET"),
        "magnet:?xt=urn:btih:abc&***"
    );

    let s = st("rss-mask", "");
    let fake = Fake::start().await;
    let gone = fake.url("/gone?passkey=SECRET");
    let f = add_feed(&s, &gone, "Gone", None).unwrap();
    let row = refresh_feed(&s, f.id).await.unwrap();
    assert!(row.last_error.contains("404"), "{}", row.last_error);
    assert!(!row.last_error.contains("SECRET") && row.last_error.contains("/gone?***"), "{}", row.last_error);

    fake.serve("/rss", 200, feed_xml(&[("t", "T EPUB", &fake.url("/missing.torrent?passkey=SECRET"))]));
    let f = add_feed(&s, &fake.url("/rss?passkey=SECRET"), "Ok", None).unwrap();
    put_rule(&s, "t", &json!({"mustContain": "epub", "affectedFeeds": [fake.url("/rss?passkey=SECRET")], "assignedCategory": "books"})).unwrap();
    refresh_feed(&s, f.id).await.unwrap();
    let act = s.store.lock().unwrap().workflow_activity(10).unwrap();
    assert_eq!(act[0].outcome, "failed");
    assert!(!act[0].detail.contains("SECRET"), "{}", act[0].detail);
    assert!(act[0].detail.contains("tried again"), "a network failure is transient: {}", act[0].detail);
    assert!(s.store.lock().unwrap().rss_taken(&["t"]).unwrap().is_none(), "not taken");
    // The next refresh tries it again, nothing new in the feed.
    fake.serve("/missing.torrent", 200, torrent_bytes("t.epub"));
    refresh_feed(&s, f.id).await.unwrap();
    assert_eq!(count(&s, "hoard"), 1, "added at the next pass");
}

/// A response bigger than the cap is refused while it streams.
#[tokio::test]
async fn a_feed_over_the_size_cap_is_refused() {
    let fake = Fake::start().await;
    fake.serve("/big", 200, vec![b'x'; 2048]);
    let e = fetch(&Route::default(), &fake.url("/big"), 1024).await.unwrap_err();
    assert!(e.contains("larger than 1024"), "{e}");
    assert_eq!(fetch(&Route::default(), &fake.url("/big"), 4096).await.unwrap().len(), 2048);
}

// ---------------------------------------------------------------------------
// The qBittorrent shim, through the router
// ---------------------------------------------------------------------------

async fn call(s: &TestState, method: &str, uri: &str, form: &str) -> (StatusCode, String) {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Api-Key", KEY)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(form.to_string()))
        .unwrap();
    let resp = crate::api::router(s.state.clone()).oneshot(req).await.unwrap();
    let code = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (code, String::from_utf8_lossy(&body).to_string())
}

fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[tokio::test]
async fn the_shim_routes_answer_like_qbittorrent() {
    let s = st("rss-shim", "");
    let fake = Fake::start().await;
    fake.serve("/rss", 200, feed_xml(&[("e1", "Show.S01E01.1080p", &fake.url("/1.torrent")), ("e2", "Show.S01E02.720p", &fake.url("/2.torrent"))]));
    let url = fake.url("/rss");

    assert_eq!(call(&s, "POST", "/api/v2/rss/addFolder", "path=TV").await.0, StatusCode::OK);
    assert_eq!(call(&s, "POST", "/api/v2/rss/addFolder", "path=TV").await.0, StatusCode::CONFLICT);
    let (code, _) = call(&s, "POST", "/api/v2/rss/addFeed", &format!("url={}&path={}", enc(&url), enc("TV\\Fake"))).await;
    assert_eq!(code, StatusCode::OK);
    let (code, msg) = call(&s, "POST", "/api/v2/rss/addFeed", &format!("url={}&path=Again", enc(&url))).await;
    assert_eq!(code, StatusCode::CONFLICT, "{msg}");
    assert_eq!(call(&s, "POST", "/api/v2/rss/addFeed", "path=x").await.0, StatusCode::BAD_REQUEST, "url missing");
    let id = feeds(&s)[0].id;
    refresh_feed(&s, id).await.unwrap();

    // items: nested by folder; withData adds the articles.
    let (code, body) = call(&s, "GET", "/api/v2/rss/items", "").await;
    assert_eq!(code, StatusCode::OK);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["TV"]["Fake"]["url"], url.as_str());
    assert!(v["TV"]["Fake"]["uid"].as_str().unwrap().starts_with('{'));
    assert!(v["TV"]["Fake"].get("articles").is_none());
    let (_, body) = call(&s, "POST", "/api/v2/rss/items", "withData=true").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    let feed = &v["TV"]["Fake"];
    assert_eq!(feed["title"], "Fake tracker");
    assert_eq!(feed["hasError"], false);
    assert_eq!(feed["isLoading"], false);
    let arts = feed["articles"].as_array().unwrap();
    assert_eq!(arts.len(), 2);
    assert_eq!(arts[0]["torrentURL"].as_str().unwrap(), fake.url(if arts[0]["id"] == "e1" { "/1.torrent" } else { "/2.torrent" }));
    assert_eq!(arts[0]["isRead"], false);
    assert!(arts[0]["date"].as_str().unwrap().ends_with("+0000"), "RFC 2822");

    // setRule / rules / matchingArticles, with qBittorrent's document.
    let def = json!({"enabled": false, "mustContain": "show 1080p", "affectedFeeds": [url], "assignedCategory": "books", "episodeFilter": "1x1-5;"});
    let (code, msg) = call(&s, "POST", "/api/v2/rss/setRule", &format!("ruleName=hd&ruleDef={}", enc(&def.to_string()))).await;
    assert_eq!(code, StatusCode::OK, "{msg}");
    assert_eq!(call(&s, "POST", "/api/v2/rss/setRule", "ruleName=bad&ruleDef=%7Bnope").await.0, StatusCode::BAD_REQUEST);
    let (_, body) = call(&s, "GET", "/api/v2/rss/rules", "").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["hd"]["mustContain"], "show 1080p");
    assert_eq!(v["hd"]["assignedCategory"], "books");
    assert_eq!(v["hd"]["torrentParams"]["category"], "books");
    assert_eq!(v["hd"]["affectedFeeds"][0], url.as_str());
    let (_, body) = call(&s, "GET", "/api/v2/rss/matchingArticles?ruleName=hd", "").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), json!({"Fake": ["Show.S01E01.1080p"]}));
    assert_eq!(count(&s, "hoard") + count(&s, "race"), 0, "matching adds nothing (and the rule is disabled)");
    assert_eq!(fake.hits("/1.torrent"), 0);

    assert_eq!(call(&s, "POST", "/api/v2/rss/renameRule", "ruleName=hd&newRuleName=HD").await.0, StatusCode::OK);
    assert_eq!(call(&s, "POST", "/api/v2/rss/renameRule", "ruleName=hd&newRuleName=x").await.0, StatusCode::CONFLICT);
    assert!(load_rules(&s).iter().any(|r| r.name == "HD"));

    // markAsRead, one article then the whole folder.
    assert_eq!(call(&s, "POST", "/api/v2/rss/markAsRead", &format!("itemPath={}&articleId=e1", enc("TV\\Fake"))).await.0, StatusCode::OK);
    let unread = |s: &TestState| s.store.lock().unwrap().rss_articles(id).unwrap().iter().filter(|a| !a.is_read).count();
    assert_eq!(unread(&s), 1);
    assert_eq!(call(&s, "POST", "/api/v2/rss/markAsRead", "itemPath=TV").await.0, StatusCode::OK);
    assert_eq!(unread(&s), 0);

    // moveItem renames; a folder carries its feeds; refreshItem takes a folder.
    assert_eq!(call(&s, "POST", "/api/v2/rss/moveItem", &format!("itemPath={}&destPath=Fake", enc("TV\\Fake"))).await.0, StatusCode::OK);
    assert_eq!(feeds(&s)[0].path, "Fake");
    assert_eq!(call(&s, "POST", "/api/v2/rss/moveItem", "itemPath=Fake&destPath=TV%5CShows").await.0, StatusCode::OK);
    assert_eq!(call(&s, "POST", "/api/v2/rss/moveItem", "itemPath=TV&destPath=Series").await.0, StatusCode::OK);
    assert_eq!(feeds(&s)[0].path, "Series\\Shows");
    assert_eq!(call(&s, "POST", "/api/v2/rss/moveItem", "itemPath=nope&destPath=x").await.0, StatusCode::CONFLICT);
    assert_eq!(call(&s, "POST", "/api/v2/rss/refreshItem", "itemPath=Series").await.0, StatusCode::OK);

    // The write routes are POST, as in qBittorrent.
    assert_eq!(call(&s, "GET", "/api/v2/rss/removeRule?ruleName=HD", "").await.0, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(call(&s, "POST", "/api/v2/rss/removeRule", "ruleName=HD").await.0, StatusCode::OK);
    assert_eq!(call(&s, "POST", "/api/v2/rss/removeRule", "ruleName=HD").await.0, StatusCode::CONFLICT);
    assert_eq!(call(&s, "POST", "/api/v2/rss/removeItem", "path=Series").await.0, StatusCode::OK);
    assert!(feeds(&s).is_empty() && folders(&s).is_empty());

    // No key, no answer: the shim's own refusal, qBittorrent's 403.
    let req = axum::http::Request::builder().method("POST").uri("/api/v2/rss/items").body(axum::body::Body::empty()).unwrap();
    use tower::ServiceExt;
    assert_eq!(crate::api::router(s.state.clone()).oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
}

/// The native routes the RSS tab uses: a feed, its articles, a preview of
/// a draft rule (nothing added), and a download by hand.
#[tokio::test]
async fn the_native_routes_preview_and_download() {
    let s = st("rss-native", "");
    let fake = Fake::start().await;
    fake.serve("/1.torrent", 200, torrent_bytes("one.epub"));
    fake.serve("/rss", 200, feed_xml(&[("1", "One EPUB", &fake.url("/1.torrent")), ("2", "Two PDF", &fake.url("/2.torrent"))]));
    let url = fake.url("/rss");
    let (code, body) = call(&s, "POST", "/api/rss/feeds", &json!({"url": url, "name": "Books", "refresh_secs": 10}).to_string()).await;
    assert_eq!(code, StatusCode::CREATED, "{body}");
    let feed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(feed["refresh_secs"], MIN_REFRESH_SECS, "five minutes at least");
    let id = feed["id"].as_i64().unwrap();
    let (code, body) = call(&s, "POST", &format!("/api/rss/feeds/{id}/refresh"), "").await;
    assert_eq!(code, StatusCode::OK);
    let feed: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((feed["articles"].as_i64(), feed["unread"].as_i64()), (Some(2), Some(2)));
    assert_eq!(feed["engine"], "race", "no rule: fetched by the way out of a plain add");

    let draft = json!({"mustContain": "epub", "affectedFeeds": [url], "assignedCategory": "books"});
    let (code, body) = call(&s, "POST", "/api/rss/preview", &draft.to_string()).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["count"].as_i64(), v["would_add"].as_i64()), (Some(1), Some(1)));
    assert_eq!(v["feeds"][0]["articles"][0]["title"], "One EPUB");
    assert_eq!(count(&s, "hoard"), 0, "a preview adds nothing");

    let (code, body) = call(&s, "POST", &format!("/api/rss/feeds/{id}/download"), &json!({"guid": "1", "category": "books"}).to_string()).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(count(&s, "hoard"), 1);
    let (_, body) = call(&s, "GET", &format!("/api/rss/feeds/{id}/articles"), "").await;
    let arts: Value = serde_json::from_str(&body).unwrap();
    let one = arts.as_array().unwrap().iter().find(|a| a["guid"] == "1").unwrap();
    assert_eq!(one["taken"]["rule"], "(manual)");
    assert_eq!(one["read"], true);
    let (code, _) = call(&s, "POST", &format!("/api/rss/feeds/{id}/download"), &json!({"guid": "2", "category": "books"}).to_string()).await;
    assert_eq!(code, StatusCode::BAD_GATEWAY, "a .torrent that is not there is a network failure");

    let (code, _) = call(&s, "PUT", "/api/rss/rules/epub", &draft.to_string()).await;
    assert_eq!(code, StatusCode::OK);
    let (_, body) = call(&s, "GET", "/api/rss/rules/epub/matching", "").await;
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((v["count"].as_i64(), v["would_add"].as_i64()), (Some(1), Some(0)), "taken by hand already");
    assert_eq!(call(&s, "PATCH", &format!("/api/rss/feeds/{id}"), &json!({"name": "Lib\\Books"}).to_string()).await.0, StatusCode::OK);
    assert_eq!(feeds(&s)[0].path, "Lib\\Books");
    assert_eq!(call(&s, "DELETE", &format!("/api/rss/feeds/{id}"), "").await.0, StatusCode::OK);
    assert!(feeds(&s).is_empty());
}

/// `[rss] enabled = false`: the worker does nothing at all.
#[tokio::test]
async fn the_worker_does_nothing_when_disabled() {
    let s = st("rss-off", "\n[rss]\nenabled = false\n");
    assert!(!settings(&s.cfg()).enabled);
    let fake = Fake::start().await;
    fake.serve("/rss", 200, feed_xml(&[]));
    add_feed(&s, &fake.url("/rss"), "Off", None).unwrap();
    tick(&s).await;
    assert_eq!(fake.hits("/rss"), 0);
    // Nor by hand: a refresh and a Download are refused, and say why.
    let id = feeds(&s)[0].id;
    assert!(matches!(refresh_feed(&s, id).await, Err(Refusal::Conflict(_))));
    assert_eq!(fake.hits("/rss"), 0, "a manual refresh fetched while disabled");
    let (code, body) = call(&s, "POST", &format!("/api/rss/feeds/{id}/download"), &json!({"guid": "g1"}).to_string()).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert!(body.contains("disabled"), "{body}");
    let on = st("rss-on", "\n[rss]\nrefresh_minutes = 1\n");
    assert_eq!(settings(&on.cfg()).refresh_secs, MIN_REFRESH_SECS, "never under five minutes");
    let f2 = Fake::start().await;
    f2.serve("/rss", 200, feed_xml(&[]));
    add_feed(&on, &f2.url("/rss"), "On", None).unwrap();
    tick(&on).await;
    assert_eq!(f2.hits("/rss"), 1, "a new feed is due at once");
    tick(&on).await;
    assert_eq!(f2.hits("/rss"), 1, "and not again before its interval");
}

/// The RSS tab's form sends no `torrentParams`: a save from it kept the rule
/// but emptied its tags.
#[tokio::test]
async fn a_rule_saved_without_tags_keeps_them() {
    let s = st("rss-tags-kept", "");
    let feed = "http://127.0.0.1:9/rss";
    put_rule(&s, "t", &json!({"affectedFeeds": [feed], "torrentParams": {"tags": ["a", "b"]}})).unwrap();
    put_rule(&s, "t", &json!({"affectedFeeds": [feed], "mustContain": "x"})).unwrap();
    let r = load_rules(&s).into_iter().find(|r| r.name == "t").unwrap();
    assert_eq!((r.tags.clone(), r.must_contain.as_str()), (vec!["a".to_string(), "b".to_string()], "x"));
    put_rule(&s, "t", &json!({"affectedFeeds": [feed], "torrentParams": {"tags": []}})).unwrap();
    assert!(load_rules(&s).into_iter().find(|r| r.name == "t").unwrap().tags.is_empty(), "an explicit empty list clears them");
}
