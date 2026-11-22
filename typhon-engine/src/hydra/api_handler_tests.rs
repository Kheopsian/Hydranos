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

/// Every read route, against a fresh install: refused without a key, and
/// answering JSON with one.
///
/// The point is not the bodies -- it is that a route cannot answer an
/// unauthenticated caller, and cannot panic on an engine that holds
/// nothing. Both were real: the open API of 10/09, and handlers that
/// assumed at least one torrent.
macro_rules! read_routes {
    ($($test_name:ident => $name:ident),+ $(,)?) => {
        $(
            #[tokio::test]
            async fn $test_name() {
                let s = with_key(concat!("route-", stringify!($name)));

                let refused = super::$name(
                    State(s.state.clone()),
                    RawQuery(None),
                    HeaderMap::new(),
                ).await;
                assert_eq!(
                    refused.status(),
                    StatusCode::UNAUTHORIZED,
                    concat!(stringify!($name), " must refuse a caller with no key")
                );

                let allowed = super::$name(
                    State(s.state.clone()),
                    RawQuery(None),
                    keyed(KEY),
                ).await;
                assert_eq!(
                    allowed.status(),
                    StatusCode::OK,
                    concat!(stringify!($name), " must answer a caller with the key")
                );
                // Body has to parse: a route that answers 200 with a
                // truncated document is a route the UI renders as empty.
                let _ = body_json(allowed).await;
            }
        )+
    };
}

read_routes!(
    route_get_categories => get_categories,
    route_get_tags => get_tags,
    route_get_engines => get_engines,
    route_get_jobs => get_jobs,
    route_get_download_slots => get_download_slots,
    route_get_public_ip => get_public_ip,
    route_get_settings => get_settings,
    route_get_ip_modes => get_ip_modes,
    route_get_passkeys => get_passkeys,
    route_get_add_defaults => get_add_defaults,
    route_get_drain_status => get_drain_status,
    route_get_hoard_stats => get_hoard_stats,
    route_get_race_settings => get_race_settings,
    route_get_baseline => get_baseline,
    route_qbit_categories => qbit_categories,
    route_qbit_tags => qbit_tags,
    route_get_dedup_stats => get_dedup_stats,
    route_get_hoard_pinned => get_hoard_pinned,
);

/// The key may ride the query instead of the header on every route, not
/// only the ones a browser happens to open.
#[tokio::test]
async fn a_read_route_accepts_the_key_in_the_query() {
    let s = with_key_in_query("route-query");
    let resp = super::get_engines(
        State(s.state.clone()),
        RawQuery(Some(format!("apikey={KEY}"))),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// ⚠️ A fresh Docker install has no key in default.toml. Every route must
/// refuse it rather than serve the control plane to the network.
#[tokio::test]
async fn an_install_with_no_key_serves_nothing() {
    let s = state("route-nokey");
    for resp in [
        super::get_engines(State(s.state.clone()), RawQuery(None), HeaderMap::new()).await,
        super::get_settings(State(s.state.clone()), RawQuery(None), keyed("")).await,
        super::get_categories(State(s.state.clone()), RawQuery(None), keyed("guess")).await,
    ] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}

/// The settings screen edits keys this binary does not model, so the route
/// re-reads the file rather than serialising the typed Config back out.
#[tokio::test]
async fn settings_serve_keys_the_struct_does_not_model() {
    let s = state_from(
        "route-settings",
        &format!(
            "[daemon]\napi_key = \"{KEY}\"\n\n[a_section_the_binary_never_heard_of]\nkept = \"yes\"\n"
        ),
    );
    let body = body_json(
        super::get_settings(State(s.state.clone()), RawQuery(None), keyed(KEY)).await,
    )
    .await;
    assert_eq!(
        body["a_section_the_binary_never_heard_of"]["kept"],
        serde_json::json!("yes"),
        "an unmodelled key survives the read: {body}"
    );
}

/// A fresh install lists no jobs -- and says so with an empty list rather
/// than a null the UI has to special-case.
#[tokio::test]
async fn a_fresh_install_reports_empty_collections_not_null() {
    let s = with_key("route-empty");
    let jobs =
        body_json(super::get_jobs(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(!jobs.is_null(), "jobs answered null: {jobs}");
    let tags =
        body_json(super::get_tags(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(!tags.is_null(), "tags answered null: {tags}");
}
