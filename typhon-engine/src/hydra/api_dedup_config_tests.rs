use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

async fn post(s: &TestState, body: &str) -> axum::response::Response {
    post_dedup_config(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await
}

/// The method is saved and read back; a save that does not name it (the
/// checkbox of an older page) leaves it alone; an unknown word writes nothing.
#[tokio::test]
async fn the_link_method_is_saved_kept_and_validated() {
    let s = state_from("dedup-method", &format!("[daemon]\napi_key = \"{KEY}\"\n"));
    assert_eq!(
        s.state.cfg().dedup.method,
        crate::dedup::Method::Hardlink,
        "default"
    );

    let resp = post(&s, r#"{"enabled":true,"method":"reflink"}"#).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["method"], "reflink");
    let text = std::fs::read_to_string(&s.state.config_path).unwrap();
    let on_disk: Config = toml::from_str(&text).unwrap();
    assert_eq!(
        on_disk.dedup.method,
        crate::dedup::Method::Reflink,
        "{text}"
    );

    let resp = post(&s, r#"{"enabled":false}"#).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        s.state.cfg().dedup.method,
        crate::dedup::Method::Reflink,
        "kept"
    );
    assert!(!s.state.cfg().dedup.enabled);

    let resp = get_dedup_stats(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(body_json(resp).await["method"], "reflink");

    let before = std::fs::read_to_string(&s.state.config_path).unwrap();
    let resp = post(&s, r#"{"enabled":true,"method":"copy"}"#).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        std::fs::read_to_string(&s.state.config_path).unwrap(),
        before,
        "nothing written"
    );
}
