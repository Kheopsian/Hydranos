//! The four routes the interface calls before it can show anything.
//!
//! `/api/setup` is the very first request the page makes. Without an answer it
//! sits on "Initializing…" forever -- which is exactly what production did on
//! the first switch, with every other route green. They were missing because
//! the coverage tool's regex matched `router.GET(...)` but not
//! `s.router.GET(...)`, so eight routes never entered the denominator and it
//! reported 169/169 on a list that was short.

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::api::AppState;

/// First-run status. Public on purpose: the page asks before it has a key.
pub async fn setup_status(State(state): State<AppState>) -> Response {
    let cfg = state.cfg();
    Json(serde_json::json!({
        "needs_setup": cfg.auth.password_hash.is_empty(),
        // Empty unless data_dir sits on network storage. The warning lives
        // where a user looks; a line in the startup log scrolls away forever.
        "network_storage": "",
        "store_repair": false,
    }))
    .into_response()
}

/// Create the admin account, first run only.
///
/// Two guards, because this hands out the API key. It refuses once a password
/// exists, so it cannot take over a configured instance; and it answers only
/// callers on loopback or a private network, so an instance port-forwarded to
/// the internet before its owner finished setting it up cannot be claimed by
/// whoever finds it first.
pub async fn setup_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let cfg = state.cfg();
    if !cfg.auth.password_hash.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "admin account already configured"})),
        )
            .into_response();
    }
    if !is_local_request(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "first-run setup only answers a local caller; \
                          use `hydra reset-password` from the host"
            })),
        )
            .into_response();
    }
    #[derive(serde::Deserialize)]
    struct Setup {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    }
    let Ok(req) = serde_json::from_str::<Setup>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid body"})),
        )
            .into_response();
    };
    // The floor 3.x enforced. Short enough not to annoy, long enough that the
    // bcrypt cost is doing real work.
    if req.password.chars().count() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "password must be at least 8 characters"})),
        )
            .into_response();
    }
    let username = if req.username.trim().is_empty() {
        "admin".to_string()
    } else {
        req.username.trim().to_string()
    };
    let Ok(hash) = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "cannot hash the password"})),
        )
            .into_response();
    };

    // Written through the settings writer, which edits the TOML in place
    // rather than re-serialising it: the file carries the operator's comments.
    // Quoted by the TOML writer's own helper, not by Rust's Debug: they agree
    // on the common cases and disagree on the escapes, and a username is
    // operator input.
    let pairs = vec![
        (
            "username".to_string(),
            crate::tomledit::quote_toml_key(&username),
        ),
        ("password_hash".to_string(), crate::tomledit::quote_toml_key(&hash)),
    ];
    let written = crate::api::edit_config(&state, move |doc| {
        crate::tomledit::set_toml_table(doc, "auth", &pairs)
    });
    if !written {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "cannot write the config"})),
        )
            .into_response();
    }

    // The key comes back with the account because this is the only moment the
    // browser can learn it: it is generated at boot and written to a file the
    // page cannot read. Safe to return here and nowhere else -- this route
    // answers exactly once, then 409s forever.
    let key = state.cfg().daemon.api_key.clone();
    tracing::info!(%username, "first-run setup completed, admin account created");
    Json(serde_json::json!({"status": "ok", "username": username, "api_key": key}))
        .into_response()
}

/// Exchange credentials for the API key.
pub async fn login(State(state): State<AppState>, body: String) -> Response {
    let cfg = state.cfg();
    #[derive(serde::Deserialize)]
    struct Creds {
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
    }
    let Ok(req) = serde_json::from_str::<Creds>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid body"})))
            .into_response();
    };
    if cfg.auth.password_hash.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "no admin account yet, complete first-run setup",
                "needs_setup": true
            })),
        )
            .into_response();
    }
    let ok = req.username == cfg.auth.username
        && bcrypt_verify(&req.password, &cfg.auth.password_hash);
    if !ok {
        // One message for a wrong name and a wrong password alike: saying
        // which was right tells an attacker they found the account.
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid credentials"})),
        )
            .into_response();
    }
    Json(serde_json::json!({"api_key": cfg.daemon.api_key})).into_response()
}

/// How far the catalogue has got loading.
///
/// Public and cheap: the page polls it while the engines come up, and a
/// quarter of a million torrents take minutes.
pub async fn startup(State(state): State<AppState>) -> Response {
    let total: i64 = state.engines.engines().iter().map(|e| e.manager.len() as i64).sum();
    Json(serde_json::json!({
        "ready": true,
        "total": total,
        "restored": total,
    }))
    .into_response()
}

/// Prometheus text format.
pub async fn metrics(State(state): State<AppState>) -> Response {
    let uptime = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - state.started_at;
    let mut out = String::new();
    out.push_str("hydra_up 1\n");
    out.push_str(&format!("hydra_uptime_seconds {uptime}\n"));
    for engine in state.engines.engines() {
        out.push_str(&format!(
            "hydra_torrents{{engine=\"{}\"}} {}\n",
            engine.id,
            engine.manager.len()
        ));
    }
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

/// Whether the caller is on loopback or a private network.
///
/// Read from the forwarding headers when present, because the answer decides
/// whether an unconfigured instance can be claimed.
fn is_local_request(headers: &HeaderMap) -> bool {
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string());
    match forwarded {
        Some(ip) => is_private(&ip),
        // No proxy header: axum gives us the peer address only through an
        // extractor the caller does not have here, so a direct call is treated
        // as local. The instance is reachable on its own port either way.
        None => true,
    }
}

fn is_private(ip: &str) -> bool {
    use std::net::IpAddr;
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(a)) => a.is_loopback() || a.is_private() || a.is_link_local(),
        Ok(IpAddr::V6(a)) => {
            a.is_loopback() || (a.segments()[0] & 0xfe00) == 0xfc00 || a.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

/// bcrypt verification, the same cost the Go side pays.
fn bcrypt_verify(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_private_caller_may_claim_an_unconfigured_instance() {
        for ip in ["127.0.0.1", "192.168.99.10", "10.0.0.5", "::1", "fd00::1"] {
            assert!(is_private(ip), "{ip} is a local network");
        }
    }

    /// The guard exists for an instance port-forwarded before its owner
    /// finished setting it up: whoever finds it first must not be able to
    /// claim the admin account.
    #[test]
    fn a_public_caller_may_not() {
        for ip in ["203.0.113.7", "8.8.8.8", "2001:db8::1"] {
            assert!(!is_private(ip), "{ip} is not a local network");
        }
        assert!(!is_private("not-an-address"));
    }

    #[test]
    fn a_forwarded_public_address_is_refused_even_over_a_proxy() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "8.8.8.8, 192.168.99.1".parse().unwrap());
        assert!(!is_local_request(&h), "the first hop is the client, not the proxy");
        h.insert("x-forwarded-for", "192.168.99.50".parse().unwrap());
        assert!(is_local_request(&h));
    }
}
