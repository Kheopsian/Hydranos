use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

/// The new password is the one that logs in afterwards, in memory and in
/// the file the next boot reads; the old one no longer does.
#[tokio::test]
async fn a_changed_password_is_the_one_that_works() {
    let old = bcrypt::hash("old-password", 4).unwrap();
    let s = state_from(
        "pw-change",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[auth]\nusername = \"admin\"\npassword_hash = \"{old}\"\n"),
    );
    let resp = post_password(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"password":"new-password"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let live = s.state.cfg().auth.password_hash.clone();
    assert!(bcrypt::verify("new-password", &live).unwrap());
    assert!(!bcrypt::verify("old-password", &live).unwrap());

    let on_disk: Config =
        toml::from_str(&std::fs::read_to_string(&s.state.config_path).unwrap()).unwrap();
    assert!(bcrypt::verify("new-password", &on_disk.auth.password_hash).unwrap());
    assert_eq!(on_disk.auth.username, "admin", "the rest of [auth] is kept");
}

#[tokio::test]
async fn a_short_password_changes_nothing() {
    let old = bcrypt::hash("old-password", 4).unwrap();
    let s = state_from(
        "pw-short",
        &format!("[daemon]\napi_key = \"{KEY}\"\n\n[auth]\npassword_hash = \"{old}\"\n"),
    );
    let resp = post_password(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        r#"{"password":"short"}"#.into(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(bcrypt::verify("old-password", &s.state.cfg().auth.password_hash).unwrap());
}
