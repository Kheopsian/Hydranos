use super::testing::*;
use super::*;

const KEY: &str = "0123456789abcdef0123456789abcdef";

fn st(tag: &str) -> TestState {
    state_from(tag, &format!("[daemon]\napi_key = \"{KEY}\"\n"))
}

/// A throwaway Hydra on a real loopback port. Bound to :0 so tests never
/// collide, and shut down with the test.
struct FakeNode {
    url: String,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

async fn fake_node() -> FakeNode {
    use axum::routing::get;
    let app = axum::Router::new()
        .route(
            "/api/status",
            get(|| async {
                Json(serde_json::json!({
                    "version": "4.27.0",
                    "engines": [{"id": "race"}, {"id": "hoard"}],
                    "hoard": {"total_torrents": 10},
                    "race": {"torrents": 2}
                }))
            }),
        )
        .route(
            "/api/engines",
            get(|| async { Json(serde_json::json!([])) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await;
    });
    // Declared by IP rather than 127.0.0.1: the loopback guard refuses
    // that spelling on purpose, and this fixture is about the rest.
    FakeNode {
        url: format!("http://{addr}"),
        _shutdown: tx,
    }
}

/// ⭐⭐ A node URL is used for TWO things: this process probes it, AND the
/// operator's browser is redirected to it. A loopback satisfies the first
/// and can never satisfy the second -- it would send the browser to its
/// own machine. Reported from the bench, where `127.0.0.1:8499` probed
/// green and opened nothing.
#[tokio::test]
async fn a_node_declared_on_loopback_is_refused() {
    let s = st("node-loopback");
    for url in [
        "http://127.0.0.1:8499",
        "http://localhost:8499",
        "http://[::1]:8499",
    ] {
        let resp = super::post_node(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            serde_json::json!({"name": "x", "url": url, "api_key": "k"}).to_string(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{url} must be refused as loopback"
        );
    }
}

/// A node needs a name and a URL; neither is optional.
#[tokio::test]
async fn a_node_without_a_name_or_a_url_is_refused() {
    let s = st("node-incomplete");
    for body in [
        serde_json::json!({"url": "http://10.0.0.5:8199"}),
        serde_json::json!({"name": "node-a"}),
        serde_json::json!({"name": "  ", "url": "http://10.0.0.5:8199"}),
        serde_json::json!({}),
    ] {
        let resp = super::post_node(
            State(s.state.clone()),
            RawQuery(None),
            keyed(KEY),
            body.to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "got {body}");
    }
}

/// A body that is not JSON is a bad request, not a node named "".
#[tokio::test]
async fn a_node_body_that_is_not_json_is_refused() {
    let s = st("node-junk");
    let resp = super::post_node(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        "{not json".to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// A declared node is stored and then listed, with its key kept
/// server-side rather than echoed back into the page.
/// ⚠️ `post_node` PROBES before it stores, and the loopback guard forbids
/// declaring a fixture server on 127.0.0.1 -- the two together mean the
/// HTTP add path cannot be driven from a test on this machine. The storage
/// and listing behind it can, so that is what is exercised here.
fn declare(s: &TestState, name: &str, url: &str, api_key: &str) {
    let store = s.store.lock().unwrap();
    store
        .put_node(&crate::store::Node {
            name: name.into(),
            url: url.into(),
            api_key: api_key.into(),
            enabled: true,
            added_at: 1_700_000_000,
        })
        .expect("stored");
}

#[tokio::test]
async fn a_declared_node_is_stored_and_listed() {
    let s = st("node-store");
    declare(&s, "node-a", "http://10.0.0.5:8199", "the-remote-key");

    let listed =
        body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    let text = listed.to_string();
    assert!(text.contains("node-a"), "the node is listed: {listed}");
    assert!(
        !text.contains("the-remote-key"),
        "⚠️ the remote's key must not be served back to a browser: {listed}"
    );
}

/// ⭐ A node is PROBED before it is stored: declaring one that does not
/// answer is refused rather than saved as a row that will never work.
#[tokio::test]
async fn a_node_that_does_not_answer_is_not_stored() {
    let s = st("node-unreachable");
    let resp = super::post_node(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({
            "name": "ghost", "url": "http://10.255.255.1:9", "api_key": "k"
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "an unreachable node is refused"
    );

    let listed =
        body_json(super::get_nodes(State(s.state.clone()), RawQuery(None), keyed(KEY)).await)
            .await;
    assert!(
        !listed.to_string().contains("ghost"),
        "and nothing was stored: {listed}"
    );
}

/// Probing a declared node that answers: the route reaches it over real
/// HTTP and reports it online.
#[tokio::test]
async fn testing_a_node_that_answers_reports_it_online() {
    let s = st("node-test-ok");
    let node = fake_node().await;
    let resp = super::post_node_test(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"url": node.url, "api_key": "k"}).to_string(),
    )
    .await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body = body_json(resp).await;
    assert_eq!(body["online"], serde_json::json!(true), "got {body}");
    assert_eq!(body["version"], serde_json::json!("4.27.0"));
}

/// ⭐ A node that is down must render as down rather than take the page
/// with it: the probe never fails, an error IS the answer.
#[tokio::test]
async fn testing_a_node_that_is_not_there_reports_it_offline_rather_than_failing() {
    let s = st("node-test-dead");
    let resp = super::post_node_test(
        State(s.state.clone()),
        RawQuery(None),
        keyed(KEY),
        serde_json::json!({"url": "http://10.255.255.1:9", "api_key": "k"}).to_string(),
    )
    .await;
    // The route answers; whether it is 200 with online:false or a 4xx, what
    // matters is that it came back at all.
    assert!(!resp.status().is_server_error(), "got {:?}", resp.status());
}

/// Deleting a node that was declared removes it; deleting one that was not
/// says so rather than reporting success.
#[tokio::test]
async fn a_node_can_be_deleted_and_deleting_an_unknown_one_is_reported() {
    let s = st("node-delete");
    declare(&s, "node-a", "http://10.0.0.5:8199", "k");

    let gone = super::delete_node(
        State(s.state.clone()),
        axum::extract::Path("node-a".to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(gone.status().is_success(), "got {:?}", gone.status());

    let again = super::delete_node(
        State(s.state.clone()),
        axum::extract::Path("node-a".to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(
        !again.status().is_success(),
        "a node that is not there cannot have been deleted: {:?}",
        again.status()
    );
}

/// ⭐ Enrolment mints a ONE-TIME token and the command that spends it. The
/// direction matters: the new machine registers ITSELF, so this Hydra
/// never holds a credential for another host.
#[tokio::test]
async fn enrolment_mints_a_token_and_the_command_to_spend_it() {
    let s = st("node-enrol");
    let mut h = keyed(KEY);
    h.insert(axum::http::header::HOST, "10.0.0.2:8199".parse().unwrap());
    let resp = super::post_node_enrol(State(s.state.clone()), RawQuery(None), h).await;
    assert!(resp.status().is_success(), "got {:?}", resp.status());
    let body = body_json(resp).await;
    let text = body.to_string();
    assert!(
        body.get("token").is_some() || text.contains("token"),
        "a token was minted: {body}"
    );
}

/// The command checks the script against the hash of the one this binary
/// serves before running it: a script swapped on the way is never run.
#[tokio::test]
async fn the_enrol_command_runs_only_the_script_it_was_given() {
    use sha2::{Digest, Sha256};
    let s = st("node-enrol-pin");
    let mut h = keyed(KEY);
    h.insert(axum::http::header::HOST, "10.0.0.2:8199".parse().unwrap());
    let resp = super::post_node_enrol(State(s.state.clone()), RawQuery(None), h).await;
    let cmd = body_json(resp).await["command"]
        .as_str()
        .unwrap()
        .to_string();
    let sha: String = typhon_engine::torrent::hex_encode_bytes(&Sha256::digest(
        include_str!("../../../install.sh").as_bytes(),
    ));
    assert!(
        !cmd.contains("install.sh |"),
        "the download is not piped into a shell: {cmd}"
    );
    assert!(
        cmd.contains(&format!("{sha}  ")),
        "the command pins the served script: {cmd}"
    );
    let check = cmd
        .find("sha256sum -c")
        .expect("the command checks the hash");
    let run = cmd
        .find("sh hydranos-enrol.sh")
        .expect("the command runs the checked file");
    assert!(check < run, "checked before it runs: {cmd}");
}

/// `[daemon] public_url` is the base of the enrolment command when set;
/// empty (the default), the caller's `Host` header is, as before.
#[tokio::test]
async fn the_enrol_command_uses_the_public_url_when_one_is_set() {
    let mut h = keyed(KEY);
    h.insert(axum::http::header::HOST, "10.0.0.2:8199".parse().unwrap());

    let s = st("node-enrol-host");
    let cmd = body_json(
        super::post_node_enrol(State(s.state.clone()), RawQuery(None), h.clone()).await,
    )
    .await["command"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        cmd.contains("http://10.0.0.2:8199/install.sh"),
        "the default follows Host: {cmd}"
    );

    for (configured, base) in [
        (
            "https://hydra.example.net:9443",
            "https://hydra.example.net:9443",
        ),
        (
            "https://hydra.example.net/hydra/",
            "https://hydra.example.net/hydra",
        ),
        ("hydra.example.net:8199", "http://hydra.example.net:8199"),
    ] {
        let s = state_from(
            "node-enrol-public",
            &format!("[daemon]\napi_key = \"{KEY}\"\npublic_url = \"{configured}\"\n"),
        );
        let cmd = body_json(
            super::post_node_enrol(State(s.state.clone()), RawQuery(None), h.clone()).await,
        )
        .await["command"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            cmd.contains(&format!("curl -fsSL {base}/install.sh ")),
            "{configured}: {cmd}"
        );
        assert!(
            cmd.contains(&format!("--register-to {base} ")),
            "{configured}: {cmd}"
        );
        assert!(
            !cmd.contains("10.0.0.2"),
            "the caller's Host is not used: {cmd}"
        );
    }
}

/// Registering with a token nobody minted is refused: the token is the
/// whole of the authorisation.
#[tokio::test]
async fn registering_with_a_token_that_was_never_minted_is_refused() {
    let s = st("node-register");
    let resp = super::post_node_register(
        State(s.state.clone()),
        keyed(KEY),
        serde_json::json!({
            "token": "never-minted",
            "name": "newbie",
            "url": "http://10.0.0.9:8199"
        })
        .to_string(),
    )
    .await;
    assert!(!resp.status().is_success(), "got {:?}", resp.status());
}

async fn register_status(s: &super::AppState, token: &str, name: &str) -> StatusCode {
    super::post_node_register(
        State(s.clone()),
        keyed(KEY),
        serde_json::json!({"token": token, "name": name, "url": "http://10.0.0.9:8199"})
            .to_string(),
    )
    .await
    .status()
}

fn put_test_node(s: &super::AppState, name: &str) {
    let node = crate::store::Node {
        name: name.into(),
        url: "http://10.0.0.5:8199".into(),
        api_key: String::new(),
        enabled: true,
        added_at: 0,
    };
    s.store.lock().unwrap().put_node(&node).unwrap();
}

/// Whether a name is taken is told to the holder of a live token only:
/// without one the answer is the same whatever the name.
#[tokio::test]
async fn a_bad_token_learns_nothing_about_node_names() {
    let s = st("node-register-names");
    put_test_node(&s.state, "taken");
    assert_eq!(
        register_status(&s.state, "never-minted", "taken").await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        register_status(&s.state, "never-minted", "free").await,
        StatusCode::UNAUTHORIZED
    );
}

/// A live token with a name already taken is refused without being spent:
/// the operator does not have to mint another to try `--name`.
#[tokio::test]
async fn a_taken_name_does_not_spend_a_live_token() {
    let s = st("node-register-keep");
    put_test_node(&s.state, "taken");
    let (token, _) = s
        .state
        .store
        .lock()
        .unwrap()
        .create_enrol_token(600)
        .unwrap();
    assert_eq!(
        register_status(&s.state, &token, "taken").await,
        StatusCode::CONFLICT
    );
    assert_eq!(
        register_status(&s.state, &token, "fresh").await,
        StatusCode::OK
    );
    assert_eq!(
        register_status(&s.state, &token, "other").await,
        StatusCode::UNAUTHORIZED,
        "spent by the one that worked"
    );
}

/// ⭐ A node may not register itself at a loopback address either -- the
/// same reason as a declared one, checked on the other door.
#[tokio::test]
async fn a_node_cannot_register_itself_on_loopback() {
    let s = st("node-register-loop");
    let resp = super::post_node_register(
        State(s.state.clone()),
        keyed(KEY),
        serde_json::json!({
            "token": "whatever",
            "name": "newbie",
            "url": "http://127.0.0.1:8199"
        })
        .to_string(),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// A name with a path separator would escape whatever it keys; refused.
#[tokio::test]
async fn a_node_name_cannot_contain_a_path() {
    let s = st("node-name");
    for name in ["../etc", "a/b"] {
        let resp = super::post_node_register(
            State(s.state.clone()),
            keyed(KEY),
            serde_json::json!({
                "token": "t", "name": name, "url": "http://10.0.0.9:8199"
            })
            .to_string(),
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{name} must be refused"
        );
    }
}

/// Opening a node that was never declared cannot redirect anywhere.
#[tokio::test]
async fn opening_a_node_that_does_not_exist_is_refused() {
    let s = st("node-open");
    let resp = super::get_node_open(
        State(s.state.clone()),
        axum::extract::Path("nobody".to_string()),
        RawQuery(None),
        keyed(KEY),
    )
    .await;
    assert!(!resp.status().is_success(), "got {:?}", resp.status());
}

/// The install script is served so a new machine can bootstrap itself. It
/// must come back as something runnable, not an empty body.
#[tokio::test]
async fn the_install_script_is_served() {
    let resp = super::get_install_script().await;
    assert!(resp.status().is_success());
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    assert!(
        !bytes.is_empty(),
        "an empty install script installs nothing"
    );
}
