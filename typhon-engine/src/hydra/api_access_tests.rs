//! Who may call what, through the real router: the log line a login writes,
//! the refusal by default, the session after a password change.

use super::testing::*;
use super::*;
use tower::ServiceExt;

const KEY: &str = "0123456789abcdef0123456789abcdef";

#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

/// One request through the real router, as a caller at `ip`.
async fn call(st: &AppState, ip: &str, req: axum::http::Request<axum::body::Body>) -> Response {
    let peer = std::net::SocketAddr::new(ip.parse().unwrap(), 40000);
    router(st.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(peer))
        .oneshot(req)
        .await
        .unwrap()
}

fn qbit_login_form(body: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/api/v2/auth/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

/// A user name with a line break in it is one log line, not two: whoever
/// types it chooses what the log says otherwise. Run alone in a child
/// process: `tracing` caches per call site whether anyone listens.
#[test]
fn a_login_name_with_a_line_break_is_one_log_line() {
    const NAME: &str = "api::api_access_tests::a_login_name_with_a_line_break_is_one_log_line";
    if std::env::var_os("TYPHON_LOG_CHILD").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
            .env("TYPHON_LOG_CHILD", "1")
            .output()
            .unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && text.contains("1 passed"), "{text}");
        return;
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let hash = bcrypt::hash("right-password", 4).unwrap();
        let s = state_from(
            "log-name",
            &format!("[daemon]\napi_key = \"{KEY}\"\n\n[auth]\nusername = \"admin\"\npassword_hash = \"{hash}\"\n"),
        );
        let out = Captured::default();
        let sub = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(out.clone())
            .finish();
        let _g = tracing::subscriber::set_default(sub);

        // Refused: the name never matched anything.
        let r = call(&s, "10.0.0.7", qbit_login_form("username=a%0AINFO%20forged&password=nope")).await;
        assert_eq!(r.status(), StatusCode::OK);
        // Accepted through the key in the password box: the name is anything.
        let r = call(
            &s,
            "10.0.0.8",
            qbit_login_form(&format!("username=b%0AWARN%20forged&password={KEY}")),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        // A very long one stays one bounded line.
        let long = "c".repeat(5000);
        call(&s, "10.0.0.9", qbit_login_form(&format!("username={long}&password=nope"))).await;

        let text = String::from_utf8_lossy(&out.0.lock().unwrap()).into_owned();
        assert_eq!(text.lines().count(), 3, "one line per login, none forged: {text}");
        assert!(!text.lines().any(|l| l.starts_with("INFO forged") || l.starts_with("WARN forged")), "{text}");
        assert!(text.contains(r"a\nINFO forged") && text.contains(r"b\nWARN forged"), "{text}");
        assert!(text.len() < 1500, "a 5000-character name is cut: {} bytes", text.len());
    });
}

/// What answers without a key or a session. This list is written out here, not
/// read from the code, so that opening one more route is a change to a test
/// somebody has to read.
const OPEN: &[&str] = &[
    "/",
    "/health",
    "/metrics",
    "/api/setup",
    "/api/login",
    "/api/startup",
    "/changelog.md",
    "/install.sh",
    "/api/nodes/register",
    "/api/auth/api-key/confirm",
    "/api/v2/auth/login",
    "/api/v2/auth/logout",
    "/mcp",
];

fn is_open(path: &str) -> bool {
    OPEN.contains(&path) || path.starts_with("/static/")
}

fn anonymous(method: &str, uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// A route added without its own check is refused all the same.
///
/// Every handler asks for the key itself, which is a rule kept by whoever
/// writes the next one. The route here asks for nothing; a caller with no key
/// must still be turned away, and one with the key let in.
#[tokio::test]
async fn a_route_that_forgets_its_check_is_refused_anyway() {
    let s = state_from("forgot", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let extra = Router::new().route("/api/forgotten", get(|| async { "leaked" }));
    let call = |req: axum::http::Request<axum::body::Body>| {
        let peer = std::net::SocketAddr::new("10.0.0.7".parse().unwrap(), 40000);
        let app = router_with(s.state.clone(), extra.clone())
            .layer(axum::extract::connect_info::MockConnectInfo(peer));
        async move { app.oneshot(req).await.unwrap() }
    };
    let r = call(anonymous("GET", "/api/forgotten")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "no key, no answer");
    let r = call(anonymous("GET", "/api/forgotten?apikey=wrong")).await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "a wrong key is no key");
    let r = call(anonymous("GET", &format!("/api/forgotten?apikey={KEY}"))).await;
    assert_eq!(r.status(), StatusCode::OK, "the right key still opens it");
    let r = call(anonymous("GET", "/api/nowhere-at-all")).await;
    assert_ne!(r.status(), StatusCode::OK);
}

/// The paths of every `.route("...` in a piece of source.
fn routes_in(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(".route(") {
        rest = &rest[i + ".route(".len()..];
        if let Some(t) = rest.trim_start().strip_prefix('"') {
            if let Some(end) = t.find('"') {
                out.push(t[..end].to_string());
            }
        }
    }
    out
}

/// Every route of the real router, asked with no key and no session, on every
/// verb: all of them are refused but the ones OPEN lists. The routes come from
/// the router's own source, so a route added to it, or to one of the files it
/// merges, is in the table the day it is written.
#[tokio::test]
async fn every_route_but_the_open_ones_refuses_a_caller_with_no_key() {
    fn fn_body<'a>(src: &'a str, head: &str) -> &'a str {
        let from = src.find(head).unwrap_or_else(|| panic!("{head} not found"));
        &src[from..from + src[from..].find("\n}\n").unwrap()]
    }
    let router_src = fn_body(include_str!("api.rs"), "fn router_with(");
    let merged: &[(&str, &str)] = &[
        ("rulesapi", include_str!("rulesapi.rs")),
        ("magnets", include_str!("magnets.rs")),
        ("rss", include_str!("rss/routes.rs")),
        ("ipfilter", include_str!("ipfilter.rs")),
        ("watch", include_str!("watch.rs")),
        ("selection", include_str!("selection.rs")),
        ("mcp", include_str!("mcp.rs")),
    ];
    // A module merged into the router and missing from the table above would
    // be a hole in this test, not in the router.
    let mut named: Vec<&str> = router_src
        .split(".merge(crate::")
        .skip(1)
        .filter_map(|t| t.split("::routes()").next())
        .collect();
    named.sort_unstable();
    let mut known: Vec<&str> = merged.iter().map(|(n, _)| *n).collect();
    known.sort_unstable();
    assert_eq!(named, known, "a module merged into the router is not in this test");

    let mut paths = routes_in(router_src);
    for (_, src) in merged {
        paths.extend(routes_in(fn_body(src, "pub fn routes()")));
    }
    assert!(paths.len() > 250, "only {} routes found", paths.len());
    for open in OPEN {
        assert!(paths.iter().any(|p| p == open), "{open} is listed as open and is not a route");
    }

    let s = state_from("every-route", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    let mut answered = Vec::new();
    for path in &paths {
        if is_open(path) {
            continue;
        }
        let uri: String = path
            .split('/')
            .map(|seg| match seg {
                ":info_hash" => "0000000000000000000000000000000000000000",
                s if s.starts_with(':') || s.starts_with('*') => "x",
                s => s,
            })
            .collect::<Vec<_>>()
            .join("/");
        for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            let r = call(&s.state, "10.0.0.7", anonymous(method, &uri)).await;
            if r.status() != StatusCode::UNAUTHORIZED && r.status() != StatusCode::FORBIDDEN {
                answered.push(format!("{method} {path} -> {}", r.status()));
            }
        }
    }
    assert!(answered.is_empty(), "answered a caller with no key:\n{}", answered.join("\n"));
}

/// The list the code refuses by is the list written above.
#[test]
fn the_code_opens_exactly_the_listed_paths() {
    assert_eq!(OPEN_PATHS, OPEN);
}

/// A session cookie, as the *arr stack carries it.
fn cookie_of(s: &AppState) -> String {
    format!("SID={}", s.sessions.create())
}

/// Changing the password ends every session. A cookie that was valid a moment
/// ago is how whoever held it keeps their access past the change, and the
/// session lives an hour from its last use however often it is used.
#[tokio::test]
async fn changing_the_password_ends_every_session() {
    let old = bcrypt::hash("old-password", 4).unwrap();
    let s = state_from(
        "pw-sessions",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[auth]\nusername = \"admin\"\npassword_hash = \"{old}\"\n"),
    );
    let cookie = cookie_of(&s.state);
    let mut h = axum::http::HeaderMap::new();
    h.insert(axum::http::header::COOKIE, cookie.parse().unwrap());
    assert!(authorised(&s.state, &h, ""), "the session is live before the change");

    let r = post_password(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"password":"new-password"}"#.into(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    assert!(!authorised(&s.state, &h, ""), "the session did not survive the change");
    assert!(authorised(&s.state, &keyed(KEY), ""), "the key is not a session and still works");
}
