/// The version check names itself the way every other request of the
/// client does: one identity, not a second one for GitHub to tell apart.
#[tokio::test]
async fn the_release_check_sends_the_clients_user_agent() {
    use axum::http::HeaderMap;
    use axum::routing::get;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let s2 = seen.clone();
    let app = axum::Router::new().route(
        "/tags",
        get(move |h: HeaderMap| {
            let s2 = s2.clone();
            async move {
                *s2.lock().unwrap() = h
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                "[]"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let body = super::fetch_release_tags(&format!("http://{addr}/tags")).await;
    assert_eq!(body.as_deref(), Ok("[]"));
    assert_eq!(*seen.lock().unwrap(), typhon_engine::config::user_agent());
}
