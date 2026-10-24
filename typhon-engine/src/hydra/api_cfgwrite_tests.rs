//! The configuration file is written whole, by one writer at a time, and only
//! with text the daemon could load again.

use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(
        tag,
        &format!("[daemon]\napi_key = \"{KEY}\"\napi_port = 8199\n"),
    )
}

fn add_table(doc: &str, name: &str) -> Result<String, String> {
    crate::tomledit::set_toml_table(doc, name, &[("k".to_string(), "1".to_string())])
}

/// A value of the wrong type is valid TOML and a daemon that cannot start:
/// refused with a 400, the file as it was, the running config untouched.
#[tokio::test]
async fn a_setting_of_the_wrong_type_is_refused_and_the_file_is_unchanged() {
    let s = st("cfgw-badtype");
    let before = std::fs::read_to_string(&s.config_path).unwrap();
    for value in [r#""abc""#, "70000"] {
        let body =
            format!(r#"{{"changes":[{{"section":"daemon","key":"api_port","value":{value}}}]}}"#);
        let resp = post_settings(State(s.state.clone()), RawQuery(None), keyed(KEY), body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(
            std::fs::read_to_string(&s.config_path).unwrap(),
            before,
            "{value}: nothing written"
        );
    }
    assert_eq!(s.cfg().daemon.api_port, 8199);
}

/// The new text arrives by replacing the file, not by truncating it first: a
/// second name for the old file still holds the old text, whole.
#[test]
fn an_edit_replaces_the_file_instead_of_truncating_it() {
    let s = st("cfgw-replace");
    let alias = s.dir.join("alias.toml");
    std::fs::hard_link(&s.config_path, &alias).unwrap();
    let old = std::fs::read_to_string(&alias).unwrap();
    assert!(edit_config(&s.state, |doc| add_table(doc, "alpha")));
    assert!(std::fs::read_to_string(&s.config_path)
        .unwrap()
        .contains("[alpha]"));
    assert_eq!(
        std::fs::read_to_string(&alias).unwrap(),
        old,
        "the old file was not rewritten in place"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&s.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[cfg(unix)]
#[test]
fn an_edit_keeps_the_mode_of_the_file() {
    use std::os::unix::fs::PermissionsExt;
    let s = st("cfgw-mode");
    std::fs::set_permissions(&s.config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(edit_config(&s.state, |doc| add_table(doc, "alpha")));
    assert_eq!(
        std::fs::metadata(&s.config_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// Two edits at once both land: the second reads what the first wrote.
#[test]
fn two_edits_at_once_both_land() {
    let s = st("cfgw-two");
    let slow = {
        let state = s.state.clone();
        std::thread::spawn(move || {
            edit_config(&state, |doc| {
                std::thread::sleep(std::time::Duration::from_millis(400));
                add_table(doc, "alpha")
            })
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(100));
    let fast = {
        let state = s.state.clone();
        std::thread::spawn(move || edit_config(&state, |doc| add_table(doc, "beta")))
    };
    assert!(slow.join().unwrap());
    assert!(fast.join().unwrap());
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    assert!(
        text.contains("[alpha]") && text.contains("[beta]"),
        "{text}"
    );
}

/// `[mcp] torrent_roots` is written in the file by hand: no route changes it,
/// whatever section and key a request names.
#[tokio::test]
async fn no_route_writes_the_torrent_roots() {
    let s = state_from(
        "cfgw-roots",
        &format!("[daemon]\napi_key = \"{KEY}\"\napi_port = 8199\n\n[mcp]\ntorrent_roots = []\n"),
    );
    let before = std::fs::read_to_string(&s.config_path).unwrap();
    let body = r#"{"changes":[{"section":"mcp","key":"torrent_roots","value":["/etc","/root"]}]}"#;
    let resp = post_settings(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        body.to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        std::fs::read_to_string(&s.config_path).unwrap(),
        before,
        "nothing written"
    );

    // The shared editor every other route goes through refuses it too, and
    // still takes any other edit.
    assert!(!edit_config(&s.state, |doc| {
        crate::tomledit::set_toml_value(doc, "mcp", "torrent_roots", "[\"/etc\"]")
    }));
    assert!(edit_config(&s.state, |doc| add_table(doc, "alpha")));
    assert_eq!(
        std::fs::read_to_string(&s.config_path)
            .unwrap()
            .matches("torrent_roots = []")
            .count(),
        1
    );
}

/// The settings reset rewrites the file from the defaults and carries the
/// secrets over; the list of folders the operator wrote there is carried too,
/// not wiped, and a reset is no way to change it.
#[tokio::test]
async fn a_settings_reset_keeps_the_torrent_roots() {
    let s = state_from(
        "cfgw-reset-roots",
        &format!("[daemon]\napi_key = \"{KEY}\"\napi_port = 8199\n\n[mcp]\ntorrent_roots = [\"/srv/watch\", \"/mnt/user/drop\"]\n"),
    );
    let resp = post_settings_reset(State(s.state.clone()), RawQuery(None), keyed(KEY)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let text = std::fs::read_to_string(&s.config_path).unwrap();
    let doc: toml::Value = toml::from_str(&text).unwrap();
    let roots: Vec<&str> = doc["mcp"]["torrent_roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(roots, ["/srv/watch", "/mnt/user/drop"], "{text}");
}
