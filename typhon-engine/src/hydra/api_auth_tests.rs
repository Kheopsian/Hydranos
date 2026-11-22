use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn with_key(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

fn with_key_in_query(tag: &str) -> TestState {
    state_from(
        tag,
        &format!("[daemon]\napi_key = \"{KEY}\"\napi_key_in_query = true\n"),
    )
}

/// ⚠️ THE regression to keep out. A Docker install that has never been
/// through setup has no key in default.toml, and an empty expected key
/// once meant "compare nothing, match everything" -- every `/api` route
/// answered unauthenticated on a fresh container.
#[test]
fn an_unset_api_key_refuses_everyone_rather_than_admitting_everyone() {
    let s = state("nokey");
    assert!(!authorised(&s, &HeaderMap::new(), ""), "no key, no header");
    assert!(!authorised(&s, &keyed(""), ""), "no key, empty header");
    assert!(
        !authorised(&s, &keyed("anything"), ""),
        "no key, any header"
    );
    assert!(
        !authorised(&s, &HeaderMap::new(), "apikey="),
        "no key, empty query param"
    );
    assert!(
        !authorised(&s, &HeaderMap::new(), "apikey=guess"),
        "no key, any query param"
    );
}

#[test]
fn the_right_key_in_the_header_is_admitted() {
    let s = with_key("hdr");
    assert!(authorised(&s, &keyed(KEY), ""));
}

/// The header name is compared case-insensitively by http, and callers do
/// spell it every way.
#[test]
fn the_header_name_is_not_case_sensitive() {
    let s = with_key("case");
    for name in ["X-API-Key", "x-api-key", "X-Api-Key"] {
        let mut h = HeaderMap::new();
        h.insert(name, KEY.parse().unwrap());
        assert!(authorised(&s, &h, ""), "{name} must be accepted");
    }
}

#[test]
fn a_wrong_key_is_refused() {
    let s = with_key("wrong");
    assert!(!authorised(&s, &keyed("not-the-key"), ""));
    // Same length, one byte out: the case constant_time_eq exists for.
    let mut near = KEY.to_string();
    near.pop();
    near.push('0');
    if near != KEY {
        assert!(!authorised(&s, &keyed(&near), ""));
    }
}

/// The query parameter is the fallback for callers that cannot set a
/// header -- a browser opening an SSE stream, mainly.
#[test]
fn the_key_may_arrive_in_the_query_instead() {
    let s = with_key_in_query("query");
    assert!(authorised(&s, &HeaderMap::new(), &format!("apikey={KEY}")));
    assert!(authorised(
        &s,
        &HeaderMap::new(),
        &format!("foo=1&apikey={KEY}&bar=2")
    ));
    assert!(!authorised(&s, &HeaderMap::new(), "apikey=wrong"));
}

/// An empty header must fall through to the query rather than count as an
/// answer: a client that sets the header to "" and the parameter properly
/// is still a client with the key.
#[test]
fn an_empty_header_falls_through_to_the_query() {
    let s = with_key_in_query("fallthrough");
    assert!(authorised(&s, &keyed(""), &format!("apikey={KEY}")));
}

/// A parameter that merely ends in "apikey" is a different parameter.
#[test]
fn a_lookalike_query_parameter_is_not_the_key() {
    let s = with_key("lookalike");
    assert!(!authorised(
        &s,
        &HeaderMap::new(),
        &format!("notapikey={KEY}")
    ));
    assert!(!authorised(
        &s,
        &HeaderMap::new(),
        &format!("apikeyx={KEY}")
    ));
}

#[test]
fn a_cookie_that_names_no_live_session_is_refused() {
    let s = with_key("cookie");
    let mut h = HeaderMap::new();
    h.insert(
        axum::http::header::COOKIE,
        format!("{}=never-issued", crate::session::COOKIE_NAME)
            .parse()
            .unwrap(),
    );
    assert!(
        !authorised(&s, &h, ""),
        "an unissued session id is not a session"
    );
}

/// The *arr stack's only path: log in, get a cookie, ride it. A session
/// the daemon issued must be accepted with no key at all.
#[test]
fn a_live_session_cookie_is_admitted_without_a_key() {
    let s = with_key("session");
    let sid = s.sessions.create();
    let mut h = HeaderMap::new();
    h.insert(
        axum::http::header::COOKIE,
        format!("{}={sid}", crate::session::COOKIE_NAME)
            .parse()
            .unwrap(),
    );
    assert!(authorised(&s, &h, ""));
}

#[test]
fn constant_time_eq_is_still_an_equality() {
    assert!(constant_time_eq(b"", b""));
    assert!(constant_time_eq(b"abc", b"abc"));
    assert!(!constant_time_eq(b"abc", b"abd"));
    assert!(!constant_time_eq(b"abc", b"ab"), "a prefix is not a match");
    assert!(!constant_time_eq(b"ab", b"abc"));
    // Differing in the LAST byte is the case an early-return comparison
    // would leak the length of the shared prefix for.
    assert!(!constant_time_eq(b"aaaaaaaa", b"aaaaaaab"));
}
