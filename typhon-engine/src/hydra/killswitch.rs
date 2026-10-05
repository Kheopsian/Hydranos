//! The kill switch: what leaves outside the configured way, and what does not.
//!
//! Two halves, because the traffic has two owners. An engine's sockets are
//! pinned to its interface (`netpin`) or sent through its proxy, so the
//! guarantee for an engine is its configuration; this module only says which
//! engines have one and which still leave by the default route. The daemon's
//! own requests -- lists, ipfilter, update check, webhooks, `.torrent` URLs --
//! go through ONE client (`typhon_engine::egress`), whose route is built here
//! from `[proxy]` and `[daemon]` and replaced whenever the config is.
//!
//! The report is said twice, at startup and on the Network tab, and it names
//! the gaps as well as the coverage: a kill switch that is silent about what
//! it does not cover is believed to cover it.

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::AppState;
use crate::config::{Config, LocalEngine, Session};
use typhon_engine::egress::Route;

/// The daemon's route as the file describes it.
pub fn route_of(cfg: &Config) -> Route {
    Route::new(
        &cfg.proxy.socks5_host,
        cfg.proxy.socks5_port,
        &cfg.proxy.socks5_user,
        &cfg.proxy.socks5_pass,
        &cfg.daemon.bind_interface,
        cfg.daemon.kill_switch,
    )
}

/// Put the file's route in force. Called at startup and by every config
/// reload, so a save on the Network tab applies at the next request.
pub fn apply(cfg: &Config) {
    // The route is process-wide, and the test binary reloads dozens of
    // configs at once: under test it is set only by the tests that exercise
    // it, never by whichever unrelated test saved a setting last.
    if cfg!(test) {
        return;
    }
    let route = route_of(cfg);
    let shown = route.describe();
    if typhon_engine::egress::set(route) {
        tracing::info!("egress: the daemon's own requests now leave by {shown}");
    }
}

/// What is never covered, whatever the configuration. Said in full so nobody
/// has to infer it from what IS listed.
pub const NOT_COVERED: &[&str] = &[
    "Local traffic stays local: the gluetun control server, other Hydranos nodes, a qBittorrent being imported, and NAT-PMP/UPnP to the home router for an engine that is not pinned.",
    "WireGuard's own encrypted packets leave by the default route, as they must: they are the tunnel.",
    "An engine pinned to an interface Hydranos did not create resolves tracker names with the host's resolver (names, not addresses). A managed WireGuard tunnel uses the DNS server of its file.",
    "The daemon's own requests through an interface without a proxy resolve names with the host's resolver, unless that interface is a managed tunnel.",
    "Incoming peers: an engine behind a SOCKS5 proxy still accepts connections on the host's own listen port.",
];

/// One engine's line in the report.
pub fn engine_coverage(mode: &str, e: &LocalEngine, tunnel_dns_leak: Option<bool>) -> Value {
    let s = &e.session;
    let iface = s.bind_interface.trim();
    let proxied = !s.socks5_outbound_host.trim().is_empty();
    let mut gaps: Vec<String> = Vec::new();
    let (covered, how) = if !iface.is_empty() {
        match tunnel_dns_leak {
            Some(true) => gaps.push(
                "its WireGuard file has no DNS line: tracker names are resolved by the host's resolver".into(),
            ),
            Some(false) => {}
            None => gaps.push(format!(
                "tracker names are resolved by the host's resolver ({iface} is not a tunnel Hydranos manages)"
            )),
        }
        (true, format!("every socket pinned to {iface}: a missing interface means no network, never the default route"))
    } else if proxied {
        gaps.push("incoming peers reach the host's own listen port".into());
        (true, "peers, announces and webseeds through its SOCKS5 proxy; DHT, UDP trackers and outgoing uTP off".into())
    } else if mode == "gluetun" {
        (true, "inside gluetun's network: gluetun's firewall is the kill switch".into())
    } else {
        (false, "no interface and no proxy: it leaves by the host's default route".into())
    };
    json!({ "engine": e.id, "covered": covered, "how": how, "gaps": gaps })
}

/// The whole report, for the Network tab and the startup log.
pub fn report(cfg: &Config, tunnel_dns_leak: &dyn Fn(&str) -> Option<bool>) -> Value {
    let route = route_of(cfg);
    let mode = crate::netmode::current(cfg);
    let daemon_error = route.preflight().err().unwrap_or_default();
    let engines: Vec<Value> = cfg
        .local_engines()
        .iter()
        .map(|e| engine_coverage(mode, e, tunnel_dns_leak(&e.id)))
        .collect();
    let all_engines_covered = engines.iter().all(|e| e["covered"] == true);
    json!({
        "kill_switch": cfg.daemon.kill_switch,
        "daemon": {
            "route": route.describe(),
            "socks5_host": cfg.proxy.socks5_host,
            "socks5_port": cfg.proxy.socks5_port,
            "socks5_user": cfg.proxy.socks5_user,
            "socks5_pass": cfg.proxy.socks5_pass,
            "bind_interface": cfg.daemon.bind_interface,
            "direct": route.is_direct(),
            "error": daemon_error,
        },
        "engines": engines,
        "all_engines_covered": all_engines_covered,
        "not_covered": NOT_COVERED,
    })
}

/// The startup lines: one per engine, one for the daemon, and the gaps.
pub fn log_startup(cfg: &Config, tunnel_dns_leak: &dyn Fn(&str) -> Option<bool>) {
    let r = report(cfg, tunnel_dns_leak);
    let on = cfg.daemon.kill_switch;
    let d = &r["daemon"];
    match d["error"].as_str().filter(|e| !e.is_empty()) {
        Some(e) => tracing::warn!("egress: the daemon's own requests are refused: {e}"),
        None if on || !d["direct"].as_bool().unwrap_or(true) => {
            tracing::info!("egress: the daemon's own requests leave by {}", d["route"].as_str().unwrap_or(""))
        }
        None => {}
    }
    if !on {
        return;
    }
    for e in r["engines"].as_array().into_iter().flatten() {
        let id = e["engine"].as_str().unwrap_or("");
        let how = e["how"].as_str().unwrap_or("");
        if e["covered"] == true {
            tracing::info!(engine = id, "kill switch: covered -- {how}");
        } else {
            tracing::warn!(engine = id, "kill switch: NOT covered -- {how}; give it an interface or a proxy");
        }
        for g in e["gaps"].as_array().into_iter().flatten() {
            tracing::warn!(engine = id, "kill switch: not covered -- {}", g.as_str().unwrap_or(""));
        }
    }
    for g in NOT_COVERED {
        tracing::info!("kill switch: outside its scope -- {g}");
    }
}

/// Why an engine's port is NOT asked of the home router, if it is not.
///
/// A NAT-PMP/UPnP mapping on the box opens a port on the HOST's public
/// address. For an engine that leaves another way -- a tunnel, a proxy, an
/// interface that is not the box's -- that port is one the engine is not
/// reachable on, and opening it publishes the home address to anyone who
/// scans for it. `default_iface` is the interface holding the default route:
/// an engine pinned to THAT one is on the box's network and is mapped.
pub fn home_mapping_refusal(mode: &str, s: &Session, tunnelled: bool, default_iface: Option<&str>) -> Option<String> {
    let iface = s.bind_interface.trim();
    if tunnelled {
        return Some("its port is asked of its WireGuard tunnel's gateway".into());
    }
    if mode == "gluetun" {
        return Some("behind gluetun the port comes from the VPN provider".into());
    }
    if !s.socks5_outbound_host.trim().is_empty() {
        return Some("it goes out through a SOCKS5 proxy; a port on the home router would expose the host's address".into());
    }
    if !iface.is_empty() && Some(iface) != default_iface {
        return Some(format!(
            "it is pinned to {iface}, not to the home router's interface ({})",
            default_iface.unwrap_or("none found")
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

fn refuse() -> Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response()
}

fn live_report(state: &AppState) -> Value {
    let cfg = state.cfg();
    let reg = state.engines.wireguard().clone();
    report(&cfg, &|id: &str| reg.get(id).filter(|t| t.created).map(|t| t.dns_leak))
}

/// `GET /api/network/egress`: the daemon's route and the coverage report.
pub async fn get_egress(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    Json(live_report(&state)).into_response()
}

/// `POST /api/network/egress`: `[proxy]` and `[daemon] bind_interface` /
/// `kill_switch`. Applied at once, no restart: the daemon's client is rebuilt
/// at the next request.
pub async fn post_egress(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: String,
) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    let v: Value = serde_json::from_str(&body).unwrap_or_default();
    let txt = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let host = txt("socks5_host");
    let port = v.get("socks5_port").and_then(Value::as_i64).unwrap_or(0);
    if !(0..=65535).contains(&port) || (!host.is_empty() && port == 0) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "the proxy port must be between 1 and 65535"})))
            .into_response();
    }
    let (user, pass, iface) = (txt("socks5_user"), v.get("socks5_pass").and_then(Value::as_str).unwrap_or_default().to_string(), txt("bind_interface"));
    let kill = v.get("kill_switch").and_then(Value::as_bool).unwrap_or(false);
    let q = crate::tomledit::quote_toml_key;
    let ok = crate::api::edit_config(&state, |doc| {
        let mut out = doc.to_string();
        if host.is_empty() {
            for k in ["socks5_host", "socks5_port", "socks5_user", "socks5_pass"] {
                out = crate::tomledit::delete_toml_key(&out, "proxy", k);
            }
            out = crate::tomledit::prune_empty_table(&out, "proxy");
        } else {
            out = crate::tomledit::set_toml_table(
                &out,
                "proxy",
                &[
                    ("socks5_host".to_string(), q(&host)),
                    ("socks5_port".to_string(), port.to_string()),
                    ("socks5_user".to_string(), q(&user)),
                    ("socks5_pass".to_string(), q(&pass)),
                ],
            )?;
        }
        if iface.is_empty() {
            out = crate::tomledit::delete_toml_key(&out, "daemon", "bind_interface");
        } else {
            out = crate::tomledit::set_toml_table(&out, "daemon", &[("bind_interface".to_string(), q(&iface))])?;
        }
        if kill {
            out = crate::tomledit::set_toml_table(&out, "daemon", &[("kill_switch".to_string(), "true".to_string())])?;
        } else {
            out = crate::tomledit::delete_toml_key(&out, "daemon", "kill_switch");
        }
        Ok(out)
    });
    if !ok {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "the config could not be written"})))
            .into_response();
    }
    Json(live_report(&state)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn the_3x_proxy_section_is_the_daemon_route() {
        let c = cfg("[proxy]\nsocks5_host = \"10.0.0.1\"\nsocks5_port = 1081\nsocks5_user = \"u\"\nsocks5_pass = \"p:w\"\n\
                     [daemon]\nbind_interface = \"wg0\"\nkill_switch = true\n");
        let r = route_of(&c);
        assert_eq!(r.proxy, "socks5h://u:p%3Aw@10.0.0.1:1081");
        assert_eq!(r.interface, "wg0");
        assert!(r.kill_switch);
        assert!(route_of(&cfg("")).is_direct());
    }

    #[test]
    fn engines_without_a_way_out_are_named_as_not_covered() {
        let c = cfg("[daemon]\nkill_switch = true\n[race]\nbind_interface = \"wg-race\"\n[hoard]\nsocks5_outbound_host = \"10.0.0.1\"\n\
                     [[engine]]\nname = \"bare\"\nrole = \"race\"\n[engine.session]\nbind_interface = \"\"\n");
        let r = report(&c, &|id| if id == "race" { Some(false) } else { None });
        let by = |id: &str| r["engines"].as_array().unwrap().iter().find(|e| e["engine"] == id).cloned().unwrap();
        assert_eq!(by("race")["covered"], true);
        assert!(by("race")["gaps"].as_array().unwrap().is_empty(), "a managed tunnel with DNS has no name leak");
        assert_eq!(by("hoard")["covered"], true);
        assert!(by("hoard")["gaps"][0].as_str().unwrap().contains("incoming"));
        assert_eq!(by("bare")["covered"], false, "{r:#}");
        assert_eq!(r["all_engines_covered"], false);
        // The daemon with the kill switch and no route: refused, and said.
        assert!(r["daemon"]["error"].as_str().unwrap().contains("kill switch"));
        // A tunnel without DNS is a named gap.
        let r = report(&c, &|_| Some(true));
        assert!(by_gap(&r, "race").contains("no DNS line"), "{r:#}");
    }

    fn by_gap(r: &Value, id: &str) -> String {
        r["engines"].as_array().unwrap().iter().find(|e| e["engine"] == id).unwrap()["gaps"][0]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn a_pinned_or_proxied_engine_gets_no_home_router_mapping() {
        let s = |iface: &str, socks: &str| Session {
            bind_interface: iface.into(),
            socks5_outbound_host: socks.into(),
            ..Session::with_defaults()
        };
        assert!(home_mapping_refusal("direct", &s("", ""), false, Some("eth0")).is_none(), "a plain engine is mapped");
        assert!(home_mapping_refusal("direct", &s("eth0", ""), false, Some("eth0")).is_none(), "pinned to the box's own interface");
        let r = home_mapping_refusal("direct", &s("wg7", ""), false, Some("eth0")).unwrap();
        assert!(r.contains("wg7") && r.contains("eth0"), "{r}");
        assert!(home_mapping_refusal("socks5", &s("", "10.0.0.1"), false, Some("eth0")).unwrap().contains("SOCKS5"));
        assert!(home_mapping_refusal("wireguard", &s("wg-race", ""), true, Some("eth0")).unwrap().contains("tunnel"));
        assert!(home_mapping_refusal("gluetun", &s("", ""), false, None).unwrap().contains("gluetun"));
    }

    /// The shipped template carries the new keys commented out, and nothing
    /// it holds is called dead.
    #[test]
    fn the_template_documents_the_new_keys_and_none_is_dead() {
        let t = include_str!("../../../configs/default.toml");
        for k in ["# kill_switch", "# bind_interface", "[proxy]", "# socks5_host"] {
            assert!(t.contains(k), "the template documents {k}");
        }
        assert!(crate::deadkeys::config_warnings(t).is_empty());
        let c = cfg(t);
        assert!(!c.daemon.kill_switch, "off unless asked for");
        assert!(route_of(&c).is_direct());
    }

    /// A SOCKS5 server that records the host it is asked for. `.invalid`
    /// names get a canned `200`; anything else is relayed for real, so a
    /// test elsewhere in this process that happens to make a request while
    /// the route points here still reaches its own server.
    async fn relay_socks5(body: &'static [u8]) -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut b = [0u8; 2];
                    s.read_exact(&mut b).await?;
                    let mut m = vec![0u8; b[1] as usize];
                    s.read_exact(&mut m).await?;
                    s.write_all(&[5, 0]).await?;
                    let mut h = [0u8; 4];
                    s.read_exact(&mut h).await?;
                    let host = match h[3] {
                        3 => {
                            let mut n = [0u8; 1];
                            s.read_exact(&mut n).await?;
                            let mut name = vec![0u8; n[0] as usize];
                            s.read_exact(&mut name).await?;
                            String::from_utf8_lossy(&name).into_owned()
                        }
                        1 => {
                            let mut a = [0u8; 4];
                            s.read_exact(&mut a).await?;
                            std::net::Ipv4Addr::from(a).to_string()
                        }
                        _ => return Ok::<(), std::io::Error>(()),
                    };
                    let mut p = [0u8; 2];
                    s.read_exact(&mut p).await?;
                    let port = u16::from_be_bytes(p);
                    log.lock().unwrap().push(host.clone());
                    if host.ends_with(".invalid") {
                        s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                        let mut req = [0u8; 4096];
                        let _ = s.read(&mut req).await?;
                        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        s.write_all(head.as_bytes()).await?;
                        s.write_all(body).await?;
                    } else {
                        let Ok(mut up) = tokio::net::TcpStream::connect((host.as_str(), port)).await else {
                            s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                            return Ok(());
                        };
                        s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                        let _ = tokio::io::copy_bidirectional(&mut s, &mut up).await;
                    }
                    Ok(())
                });
            }
        });
        (port, seen)
    }

    /// Every kind of the daemon's own request goes through the shared client:
    /// the proxy is handed each target's NAME (socks5h), so not even a DNS
    /// query for it left from here.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_daemon_request_goes_through_the_configured_proxy() {
        let (port, seen) = relay_socks5(b"udp://tracker.example:1337/announce\n").await;
        let before = typhon_engine::egress::route();
        typhon_engine::egress::set(Route::new("127.0.0.1", port, "", "", "", true));

        // Tracker lists and webhooks are blocking callers.
        let lists = tokio::task::spawn_blocking(|| crate::trackerlists::fetch("http://lists.hydranos.invalid/best.txt"))
            .await
            .unwrap();
        let hook = tokio::task::spawn_blocking(|| {
            crate::rulesrun::send_webhook("http://hooks.hydranos.invalid/x", &json!({"a": 1}))
        })
        .await
        .unwrap();
        // ipfilter lists, `.torrent` URLs and the update check are async.
        let filter = crate::ipfilter::fetch("http://filters.hydranos.invalid/level1.dat").await;
        let torrent = crate::mcp::fetch_torrent("http://torrents.hydranos.invalid/a.torrent").await;
        let update = crate::api::fetch_release_tags("http://releases.hydranos.invalid/tags").await;
        // Put the route back before any assertion can unwind past it.
        typhon_engine::egress::set((*before).clone());

        assert_eq!(lists.unwrap(), vec!["udp://tracker.example:1337/announce".to_string()]);
        assert!(hook.is_ok(), "{hook:?}");
        assert!(filter.is_ok(), "{filter:?}");
        assert!(torrent.is_ok(), "{torrent:?}");
        assert!(update.is_ok(), "{update:?}");
        let seen = seen.lock().unwrap().clone();
        for host in [
            "lists.hydranos.invalid",
            "hooks.hydranos.invalid",
            "filters.hydranos.invalid",
            "torrents.hydranos.invalid",
            "releases.hydranos.invalid",
        ] {
            assert!(seen.iter().any(|h| h == host), "{host} did not go through the proxy: {seen:?}");
        }
    }
}
