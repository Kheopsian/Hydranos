use super::*;

#[test]
fn percent_decoding_handles_plus_escapes_and_literals() {
    assert_eq!(percent_decode("plain"), "plain");
    assert_eq!(
        percent_decode("a+b"),
        "a b",
        "+ is a space in a query string"
    );
    assert_eq!(percent_decode("%41%42"), "AB");
    assert_eq!(percent_decode("a%2Fb"), "a/b");
    assert_eq!(percent_decode("100%25"), "100%");
}

/// The list routes page on their own: a caller's offset/limit must not
/// pin every page to the same rows.
#[test]
fn a_list_query_loses_the_callers_paging_only() {
    assert_eq!(
        without_paging("category=tv&offset=5000&limit=10&tag=a"),
        "category=tv&tag=a"
    );
    assert_eq!(without_paging("offset=1"), "");
    assert_eq!(
        without_paging("category=offset&limiter=1"),
        "category=offset&limiter=1"
    );
    let q = format!(
        "{}&offset=5000&limit=5000",
        without_paging("category=tv&offset=0")
    );
    assert_eq!(
        query_param(&q, "offset").as_deref(),
        Some("5000"),
        "the appended offset is the one read"
    );
}

/// A stray `%` is not an escape and must come back as itself rather than
/// eating the characters after it.
#[test]
fn a_truncated_escape_is_left_alone() {
    assert_eq!(percent_decode("%"), "%");
    assert_eq!(percent_decode("%4"), "%4");
    assert_eq!(percent_decode("%zz"), "%zz", "not hex, so not an escape");
}

#[test]
fn utf8_survives_percent_decoding() {
    assert_eq!(percent_decode("caf%C3%A9"), "café");
}

#[test]
fn a_query_parameter_is_found_by_its_whole_name() {
    assert_eq!(query_param("a=1&b=2", "b").as_deref(), Some("2"));
    assert_eq!(query_param("a=1", "missing"), None);
    assert_eq!(query_param("", "a"), None);
    assert_eq!(
        query_param("flag", "flag"),
        None,
        "a bare flag has no value"
    );
    assert_eq!(query_param("name=a%20b", "name").as_deref(), Some("a b"));
}

/// A parameter that merely CONTAINS the name is a different parameter.
#[test]
fn a_lookalike_parameter_name_is_not_a_match() {
    assert_eq!(query_param("xapikey=k", "apikey"), None);
    assert_eq!(query_param("apikeyx=k", "apikey"), None);
}

#[test]
fn the_host_of_a_url_drops_the_scheme_the_path_and_the_port() {
    assert_eq!(url_host("http://example.com/path"), "example.com");
    assert_eq!(url_host("https://example.com:8199/x"), "example.com");
    assert_eq!(url_host("http://192.168.99.200:8199"), "192.168.99.200");
    assert_eq!(
        url_host("example.com"),
        "example.com",
        "a bare host is a host"
    );
}

/// ⚠️ A bracketed IPv6 literal keeps its colons. Splitting on the last
/// colon returned "2001" here, and the empty string for `[::1]` -- so
/// `is_loopback_host` answered FALSE for the one address the enrolment
/// guard exists to refuse, and the dial address was never parseable.
#[test]
fn an_ipv6_literal_keeps_its_colons() {
    assert_eq!(url_host("http://[2001:db8::1]:8199/x"), "2001:db8::1");
    assert_eq!(url_host("http://[2001:db8::1]"), "2001:db8::1");
    assert_eq!(url_host("http://[::1]:8199"), "::1");
}

/// The consequence, stated where it bites: a node offering to register
/// itself on this machine's own loopback must still be refused when it
/// spells the address in IPv6.
#[test]
fn the_loopback_guard_sees_through_an_ipv6_literal() {
    assert!(is_loopback_host(&url_host("http://[::1]:8199")));
    assert!(is_loopback_host(&url_host("http://127.0.0.1:8199")));
    assert!(!is_loopback_host(&url_host("http://[2001:db8::1]:8199")));
}

#[test]
fn loopback_is_recognised_by_every_spelling_the_ui_offers() {
    for h in ["127.0.0.1", "localhost", "::1", "0.0.0.0", "127.1.2.3"] {
        assert!(is_loopback_host(h), "{h} is loopback");
    }
    for h in ["example.com", "192.168.99.200", "", "127x"] {
        assert!(!is_loopback_host(h), "{h} is not loopback");
    }
}

#[test]
fn a_release_tag_is_three_numbers() {
    assert!(is_semver_tag("v4.27.0"));
    assert!(is_semver_tag("4.27.0"));
    assert!(!is_semver_tag("v4.27"));
    assert!(!is_semver_tag("v4.27.0.1"));
    assert!(!is_semver_tag("nightly"));
    assert!(!is_semver_tag("v4.27.x"));
    assert!(
        !is_semver_tag("v4..0"),
        "an empty component is not a number"
    );
}

/// ⭐ String comparison is what makes this subtly wrong: "3.9.0" sorts
/// AFTER "3.180.0" lexically, so a naive check announces a downgrade as an
/// update.
#[test]
fn versions_compare_numerically_not_lexically() {
    assert!(
        version_less("3.9.0", "3.180.0"),
        "9 < 180, whatever the strings do"
    );
    assert!(!version_less("3.180.0", "3.9.0"));
    assert!(version_less("4.26.0", "4.27.0"));
    assert!(!version_less("4.27.0", "4.27.0"), "equal is not less");
    assert!(version_less("v4.26.0", "4.27.0"), "a leading v is ignored");
}

/// Hydranos' own version carries a "-typhon" suffix; comparing it as part
/// of the number would make every check report an update.
#[test]
fn a_build_suffix_is_dropped_before_comparing() {
    assert!(!version_less("4.27.0-typhon", "4.27.0"));
    assert!(!version_less("4.27.0", "4.27.0-typhon"));
    assert!(version_less("4.27.0-typhon", "4.28.0"));
}

#[test]
fn a_missing_component_counts_as_zero() {
    assert!(version_less("4.27", "4.27.1"));
    assert!(!version_less("4.27.0", "4.27"));
}

/// ⭐ Seed mode means "the data is already here, take my word for it".
/// Rechecking then is exactly the work the operator asked to skip.
#[test]
fn seed_mode_is_taken_at_its_word_and_never_rechecks() {
    assert!(!add_recheck_wanted(true, true), "seed mode skips the check");
    assert!(!add_recheck_wanted(true, false));
    assert!(
        add_recheck_wanted(false, true),
        "data on disk is worth checking"
    );
    assert!(
        !add_recheck_wanted(false, false),
        "a fresh download has nothing on disk: an all-miss check is wasted"
    );
}

#[test]
fn search_separators_are_the_ones_a_release_name_uses() {
    for c in [' ', '.', '_', '-', '\t'] {
        assert!(is_search_sep(c), "{c:?} separates words");
    }
    for c in ['a', '0', '\'', '&'] {
        assert!(!is_search_sep(c), "{c:?} does not");
    }
}

/// Equivalent to a `contains` over the fully normalised name -- that
/// equivalence is the whole reason the per-row String is avoided.
#[test]
fn a_token_is_found_inside_any_one_word() {
    assert!(name_has_token("Some.Show.S01E01.1080p", "s01e01"));
    assert!(name_has_token("Some.Show.S01E01.1080p", "show"));
    assert!(name_has_token("Some.Show.S01E01.1080p", "1080"));
    assert!(
        !name_has_token("Some.Show.S01E01", "showS01"),
        "a token cannot span words"
    );
    assert!(!name_has_token("Some.Show", "absent"));
}

#[test]
fn token_matching_ignores_case_including_outside_ascii() {
    assert!(name_has_token("SOME.SHOW", "show"));
    assert!(
        name_has_token("CAFÉ.2024", "café"),
        "real folding for non-ascii"
    );
}

#[test]
fn a_token_longer_than_the_word_cannot_match() {
    assert!(!name_has_token("ab", "abc"));
    // The ASCII fast path agrees with the word-wise walk it replaces.
    assert!(name_has_token("x.ABC.y", "abc"));
    assert!(
        name_has_token("Aaab", "aab"),
        "a first-byte candidate that fails is not the last one"
    );
    assert!(!name_has_token("a.b", "ab"), "not across a separator");
    assert!(name_has_token("anything", ""));
}

#[test]
fn the_borrowed_host_is_the_host() {
    for url in [
        "https://t.example.org/announce",
        "http://t.example.org:8080/abc/announce",
        "udp://tracker.opentrackr.org:1337/announce",
        "t.example.org/announce",
        "http://[::1]:80/a",
        "",
    ] {
        assert_eq!(
            tracker_host_in(url),
            typhon_engine::rpc::dispatch::tracker_host_of(url),
            "{url}"
        );
    }
}

#[test]
fn hex_on_the_stack_is_hex_encode() {
    let h: [u8; 20] = core::array::from_fn(|i| (i * 37 + 11) as u8);
    assert_eq!(
        std::str::from_utf8(&hex40(&h)).unwrap(),
        typhon_engine::torrent::hex_encode(&h)
    );
}

/// The window is REPLACED, never appended: a query that already carried an
/// offset would otherwise arrive with two, and which one wins is the
/// parser's business rather than ours.
#[test]
fn paging_replaces_any_window_the_query_already_had() {
    let out = with_window("sort=name&offset=500&limit=10", 0, 100);
    assert!(out.contains("sort=name"));
    assert_eq!(
        out.matches("offset=").count(),
        1,
        "exactly one offset: {out}"
    );
    assert_eq!(out.matches("limit=").count(), 1, "exactly one limit: {out}");
    assert!(out.contains("offset=0") && out.contains("limit=100"));
}

#[test]
fn paging_an_empty_query_produces_just_the_window() {
    assert_eq!(with_window("", 20, 50), "offset=20&limit=50");
}

#[test]
fn a_timestamp_in_the_past_reads_as_a_date_not_as_an_offset() {
    let s = iso8601_ago(std::time::Duration::from_secs(3600));
    assert!(s.len() >= 10, "an RFC3339 stamp, got {s:?}");
    assert!(s.contains('-'), "got {s:?}");
}

#[test]
fn zero_is_the_value_that_gets_omitted() {
    assert!(is_zero_i64(&0));
    assert!(!is_zero_i64(&1));
    assert!(!is_zero_i64(&-1));
}

/// The settings screen shows and edits keys this binary does not model, so
/// the file is re-read and parsed generically rather than serialised back
/// out of the typed Config -- a struct round trip drops them.
#[test]
fn toml_becomes_json_without_losing_a_key_the_struct_does_not_model() {
    let src = r#"
            answer = 42
            ratio = 1.5
            on = true
            name = "hydranos"
            list = [1, 2]
            [section]
            unknown_to_the_struct = "kept"
        "#;
    let v: toml::Value = toml::from_str(src).expect("valid toml");
    let j = toml_to_json(&v);
    assert_eq!(j["answer"], serde_json::json!(42));
    assert_eq!(j["ratio"], serde_json::json!(1.5));
    assert_eq!(j["on"], serde_json::json!(true));
    assert_eq!(j["name"], serde_json::json!("hydranos"));
    assert_eq!(j["list"], serde_json::json!([1, 2]));
    assert_eq!(
        j["section"]["unknown_to_the_struct"],
        serde_json::json!("kept"),
        "a key the binary does not model survives the round trip"
    );
}

#[test]
fn the_local_agent_name_is_derived_from_the_engine_id() {
    let a = local_agent("race");
    let b = local_agent("hoard");
    assert_ne!(a, b, "two engines are two agents");
    assert!(a.contains("race"), "got {a}");
}
