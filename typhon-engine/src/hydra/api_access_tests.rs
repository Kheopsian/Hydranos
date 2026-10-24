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
