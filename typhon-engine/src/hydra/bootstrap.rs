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
        // Same test, on the same directory, as the one that keeps hydra.db out
        // of WAL (`Store::open`), so the banner appears exactly when what it
        // says is true. Until 4.4 this was a constant "" and the page's banner
        // and first-run warning could never show.
        "network_storage": network_storage(&cfg.daemon.data_dir),
        "store_repair": false,
    }))
    .into_response()
}

/// What the page names in its network-storage warning, or "" on a local disk.
///
/// The page keys its "Got it" on this string, so it must stay stable from one
/// boot to the next: a generic label, not the mount it happened to resolve.
pub(crate) fn network_storage(data_dir: &str) -> &'static str {
    if crate::platform::is_network_fs(std::path::Path::new(data_dir)) {
        "network share"
    } else {
        ""
    }
}

/// The one-time code first-run setup asks for, while no admin account exists.
///
/// Whoever reaches the port is not proof of being the owner: behind Docker's
/// port forwarding, a NAT or a rootless network every caller can look local.
/// Reading the daemon's log is: the code is written there at boot and nowhere
/// else -- not in `/api/setup`, not in an error, not in `/metrics`. It lives
/// in memory only, so a restart prints a new one, and it is dropped as soon
/// as an account exists.
static SETUP_CODE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Ambiguous letters and digits left out: the code is read off a terminal
/// and typed by hand.
const SETUP_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Draw the setup code and print it, if the instance has no admin account.
/// Called once at boot, after logging is up.
pub fn announce_setup_code(config: &crate::config::Config) {
    if !config.auth.password_hash.is_empty() {
        return;
    }
    let code = fresh_setup_code();
    *SETUP_CODE.lock().unwrap_or_else(|e| e.into_inner()) = Some(code.clone());
    let line = format!(
        "no admin account yet: open the web UI and enter the setup code {code} \
         (or run `hydranos reset-password <password>` on the host)"
    );
    tracing::warn!("{line}");
    // A log filter above warn would hide the only place the code is written.
    if !tracing::enabled!(tracing::Level::WARN) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
pub(crate) fn set_setup_code_for_test(code: &str) {
    *SETUP_CODE.lock().unwrap_or_else(|e| e.into_inner()) = Some(code.to_string());
}

#[cfg(test)]
pub(crate) fn setup_code_pending() -> bool {
    SETUP_CODE.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn fresh_setup_code() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 10];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    // 32 symbols: five bits each, no modulo bias. 50 bits in all.
    let chars: String = bytes
        .iter()
        .map(|b| SETUP_CODE_ALPHABET[(*b & 31) as usize] as char)
        .collect();
    format!("{}-{}", &chars[..5], &chars[5..])
}

/// Whether `presented` is the code the log shows. Case, dashes and spaces
/// do not count; the comparison takes the same time whatever matches.
fn setup_code_matches(expected: Option<&str>, presented: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .flat_map(char::to_uppercase)
            .collect()
    };
    match expected {
        Some(code) => {
            let presented = norm(presented);
            !presented.is_empty()
                && crate::api::constant_time_eq(presented.as_bytes(), norm(code).as_bytes())
        }
        None => false,
    }
}

/// Create the admin account, first run only.
///
/// Three guards, because this hands out the API key. It refuses once a
/// password exists, so it cannot take over a configured instance; it answers
/// only callers on loopback or a private network; and it wants the setup code
/// from the daemon's log, since in many deployments every caller looks local.
pub async fn setup_password(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
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
    if !is_local_request(peer.ip(), &headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "first-run setup only answers a local caller; \
                          run `hydranos reset-password <password>` on the host"
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
        #[serde(default)]
        setup_code: String,
    }
    let Ok(req) = serde_json::from_str::<Setup>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid body"})),
        )
            .into_response();
    };
    // Held across the write below: two callers with the right code cannot
    // both get past it, and the code is gone once the account exists.
    let mut code = SETUP_CODE.lock().unwrap_or_else(|e| e.into_inner());
    if !setup_code_matches(code.as_deref(), &req.setup_code) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "wrong or missing setup code; it is printed in the Hydranos log \
                          at startup (docker logs hydranos, journalctl -u hydranos, \
                          or hydranos.log)"
            })),
        )
            .into_response();
    }
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
    *code = None;
    drop(code);

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
        && crate::session::verify_password(&req.password, &cfg.auth.password_hash).await;
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
/// quarter of a million torrents take minutes. While they do, the answer
/// comes from `startup::snapshot` behind the gate; this handler only runs
/// once the router is served, so the load is over and every engine's
/// catalogue is what it holds now.
pub async fn startup(State(state): State<AppState>) -> Response {
    let engines: Vec<serde_json::Value> = state
        .engines
        .engines()
        .iter()
        .map(|e| {
            let n = e.manager.len();
            serde_json::json!({"id": e.id, "restored": n, "total": n})
        })
        .collect();
    let total: usize = state.engines.engines().iter().map(|e| e.manager.len()).sum();
    Json(serde_json::json!({
        "ready": true,
        "phase": crate::startup::Phase::Ready.name(),
        "total": total,
        "restored": total,
        "engines": engines,
        "uptime": crate::startup::uptime_secs(),
    }))
    .into_response()
}

/// Prometheus text format: per-engine series from counters already kept
/// (see `obs`), never a walk of the catalogue per scrape.
pub async fn metrics(State(state): State<AppState>) -> Response {
    let out = crate::obs::metrics(&state);
    ([(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

/// Whether the caller is on loopback or a private network.
///
/// The socket peer decides first: a public peer is refused whatever headers
/// it sends, since anyone can write `X-Forwarded-For: 127.0.0.1`. A private
/// peer may be a reverse proxy, so every address its forwarding headers name
/// must be private too. 4.3 read only the header and let a direct call
/// without one through, which made the guard a no-op on a forwarded port.
///
/// Every hop, not the first: a proxy appends the address it saw to whatever
/// the client already sent, so the leftmost entry is the client's own claim
/// and only the ones after it were written by a proxy. Which of those is the
/// client depends on how many proxies there are, which this guard cannot
/// know; requiring all of them to be local needs no such count. The same
/// goes for `X-Real-IP` and `Forwarded`, which some proxies set instead, and
/// an entry that does not parse is not taken for a local one.
fn is_local_request(peer: std::net::IpAddr, headers: &HeaderMap) -> bool {
    if !is_private(&peer.to_canonical().to_string()) {
        return false;
    }
    forwarded_hops(headers).iter().all(|hop| is_private_hop(hop))
}

/// Every client address the forwarding headers name, in no particular order.
fn forwarded_hops(headers: &HeaderMap) -> Vec<String> {
    let mut hops = Vec::new();
    for name in ["x-forwarded-for", "x-real-ip"] {
        for value in headers.get_all(name) {
            // A value that is not text still names a hop, one that can't be checked.
            let text = value.to_str().unwrap_or("?");
            hops.extend(text.split(',').map(|s| s.trim().to_string()));
        }
    }
    for value in headers.get_all("forwarded") {
        let text = value.to_str().unwrap_or("for=?");
        for pair in text.split([',', ';']) {
            let mut kv = pair.splitn(2, '=');
            let key = kv.next().unwrap_or("").trim();
            if key.eq_ignore_ascii_case("for") {
                hops.push(kv.next().unwrap_or("").trim().trim_matches('"').to_string());
            }
        }
    }
    hops
}

/// One forwarded address, as proxies write them: bare, with a port, or
/// bracketed IPv6 with or without one.
fn is_private_hop(hop: &str) -> bool {
    if let Ok(sa) = hop.parse::<std::net::SocketAddr>() {
        return is_private(&sa.ip().to_canonical().to_string());
    }
    let bare = hop.trim_start_matches('[').trim_end_matches(']');
    match bare.parse::<std::net::IpAddr>() {
        Ok(ip) => is_private(&ip.to_canonical().to_string()),
        Err(_) => false,
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
        let proxy: std::net::IpAddr = "192.168.99.1".parse().unwrap();
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.9, 192.168.99.1".parse().unwrap());
        assert!(!is_local_request(proxy, &h), "a public hop anywhere in the chain is refused");
        h.insert("x-forwarded-for", "192.168.99.50".parse().unwrap());
        assert!(is_local_request(proxy, &h));
    }

    /// A proxy appends what it saw after what the client sent: the address
    /// it vouches for is on the right, and the left is the client's to write.
    #[test]
    fn a_client_written_hop_does_not_hide_the_one_the_proxy_appended() {
        let proxy: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let refused = |name: &str, value: &str| {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
            assert!(!is_local_request(proxy, &h), "{name}: {value}");
        };
        refused("x-forwarded-for", "10.0.0.1, 203.0.113.9");
        refused("x-forwarded-for", "10.0.0.1,203.0.113.9:4711");
        refused("x-forwarded-for", "10.0.0.1, [2001:db8::1]:443");
        refused("x-forwarded-for", "10.0.0.1, unknown");
        refused("x-real-ip", "203.0.113.9");
        refused("forwarded", "for=10.0.0.1, for=203.0.113.9;proto=https");
        refused("forwarded", "for=\"[2001:db8::1]:4711\"");

        // Two header lines are one list: the second may be the proxy's.
        let mut h = HeaderMap::new();
        h.append("x-forwarded-for", "10.0.0.1".parse().unwrap());
        h.append("x-forwarded-for", "203.0.113.9".parse().unwrap());
        assert!(!is_local_request(proxy, &h), "a second header line counts");

        // A chain that stays on the LAN is still a local caller.
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "192.168.1.5, 10.0.0.2:8080, [fd00::7]".parse().unwrap());
        h.insert("x-real-ip", "192.168.1.5".parse().unwrap());
        h.insert("forwarded", "for=192.168.1.5;proto=http".parse().unwrap());
        assert!(is_local_request(proxy, &h));
    }

    /// The hole 4.3 had: a direct call from the internet, with no header,
    /// was taken for a local one.
    #[test]
    fn a_public_peer_is_refused_with_or_without_a_header() {
        let public: std::net::IpAddr = "203.0.113.7".parse().unwrap();
        assert!(!is_local_request(public, &HeaderMap::new()));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "127.0.0.1".parse().unwrap());
        assert!(!is_local_request(public, &h), "a forged header does not make a public peer local");
        let mapped: std::net::IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        assert!(is_local_request(mapped, &HeaderMap::new()), "an IPv4-mapped LAN peer is local");
    }

    #[test]
    fn the_setup_code_is_typeable_and_matched_loosely_but_never_when_absent() {
        let a = fresh_setup_code();
        assert_eq!(a.len(), 11);
        assert!(a.chars().all(|c| c == '-' || SETUP_CODE_ALPHABET.contains(&(c as u8))), "{a}");
        assert_ne!(a, fresh_setup_code());
        assert!(setup_code_matches(Some(&a), &a.to_lowercase().replace('-', " ")));
        assert!(!setup_code_matches(Some(&a), ""));
        assert!(!setup_code_matches(Some("ABCDE-FGH23"), "ABCDE-FGH24"));
        assert!(!setup_code_matches(None, ""), "no code drawn means no setup by code");
        assert!(!setup_code_matches(None, "ABCDE-FGH23"));
    }

    /// A local disk must answer "", or every install would show the
    /// network-storage banner. A directory that does not exist is not a share
    /// either: statfs fails and the answer stays empty rather than alarming.
    #[test]
    fn a_local_data_dir_is_not_network_storage() {
        let dir = std::env::temp_dir();
        assert_eq!(network_storage(&dir.to_string_lossy()), "");
        assert_eq!(network_storage("/no/such/data_dir"), "");
    }
}
