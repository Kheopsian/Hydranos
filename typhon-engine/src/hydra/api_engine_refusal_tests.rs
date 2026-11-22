use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// ⭐⭐ Every engine-addressed route must name the engine it could not find.
///
/// Caught by the :8399 bench, not by a unit test: `get_engine_torrents` had
/// kept a bare `not_found()` and answered `{"error":"torrent not found"}` for
/// an engine that does not exist. The status was right and the sentence was
/// about the wrong noun -- an operator who mistyped a tunnel name would go
/// looking for a missing torrent.
///
/// Pinned as a table so a route added later without the shared lookup fails
/// here instead of shipping its own wording.
#[tokio::test]
async fn an_unknown_engine_is_refused_by_name_on_every_route() {
    let s = state_from(
        "unknown-engine",
        &format!("[daemon]\napi_key = \"{KEY}\"\n"),
    );
    let sel = "no-such-tunnel".to_string();

    let responses = vec![
        (
            "torrents",
            super::get_engine_torrents(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ),
        (
            "page",
            super::get_engine_page_by_id(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ),
        (
            "pinned",
            super::get_engine_pinned_by_id(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ),
        (
            "pause-all",
            super::engine_pause_all_by_id(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ),
        (
            "resume-all",
            super::engine_resume_all_by_id(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
            )
            .await,
        ),
        (
            "bulk",
            super::engine_bulk_by_id(
                State(s.state.clone()),
                axum::extract::Path(sel.clone()),
                RawQuery(None),
                keyed(KEY),
                "{}".to_string(),
            )
            .await,
        ),
    ];

    for (name, resp) in responses {
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{name}: an unknown engine must be a 404"
        );
        let body = body_json(resp).await;
        let err = body["error"].as_str().unwrap_or_default().to_string();
        assert!(
            err.contains(&sel),
            "{name}: the refusal must name the engine asked for, said {err:?}"
        );
    }
}
