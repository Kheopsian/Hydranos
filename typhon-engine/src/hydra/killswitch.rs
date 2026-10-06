//! The kill switch: what leaves outside the configured way, and what does not.
//!
//! It follows the network mode. Direct: disarmed, everything leaves by the
//! host as it always did. SOCKS5 / PROXY v2 / WireGuard / gluetun: armed, and
//! an engine with no way out of its own -- no tunnel, no interface, no proxy
//! -- is BLOCKED (`engines::connect` never puts it on the network) unless it
//! is marked `allow_direct`, which is the operator saying "this one goes
//! direct on purpose". `[daemon] kill_switch` only overrides the deduction:
//! `false` to accept leaks knowingly, `true` to arm even the direct mode.
//!
//! Two halves, because the traffic has two owners. An engine's sockets are
//! pinned to its interface (`netpin`) or sent through its proxy, so the
//! guarantee for an engine is its configuration, checked here before it
//! starts. The daemon's own requests -- lists, ipfilter, update check,
//! webhooks, `.torrent` URLs -- go through ONE client
//! (`typhon_engine::egress`), whose route `[daemon] egress` picks among the
//! engines' ways out (`egress::resolve`) and which is replaced whenever the
//! config is.
//!
//! The verdicts are said three times in the same words: at startup, on
//! `GET /api/network/mode` and on the Network tab. They name the gaps as well
//! as the coverage: a kill switch that is silent about what it does not cover
//! is believed to cover it.

use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::api::AppState;
use crate::config::{Config, LocalEngine, Session};
use typhon_engine::egress::{EngineExit, Resolved, Route};

/// Whether the kill switch is in force: as written, else deduced from the
/// mode (armed in every mode but direct).
pub fn armed(cfg: &Config) -> bool {
    cfg.daemon.kill_switch.unwrap_or_else(|| crate::netmode::current(cfg) != "direct")
}

/// Why it is armed or not, in the words of the file.
pub fn armed_why(cfg: &Config) -> String {
    let mode = crate::netmode::current(cfg);
    match cfg.daemon.kill_switch {
        Some(false) => "disarmed by [daemon] kill_switch = false: engines with no way out of their own leave by the host".into(),
        Some(true) if mode == "direct" => {
            "armed by [daemon] kill_switch = true in direct mode: nothing leaves unless it has an interface, a proxy or allow_direct".into()
        }
        Some(true) => format!("armed by [daemon] kill_switch = true ({mode} mode)"),
        None if mode == "direct" => "disarmed: direct mode, everything leaves by the host".into(),
        None => format!("armed: {mode} mode"),
    }
}

/// What the kill switch decides for one engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// Its own tunnel, interface or proxy carries everything it sends.
    Covered,
    /// The default route, on purpose: direct mode, or `allow_direct`.
    Direct,
    /// Armed, and it has no way out: never put on the network.
    Blocked,
    /// Would be blocked, but the kill switch is disarmed: it leaks, knowingly.
    Uncovered,
}

impl Coverage {
    pub fn word(&self) -> &'static str {
        match self {
            Coverage::Covered => "covered",
            Coverage::Direct => "direct",
            Coverage::Blocked => "blocked",
            Coverage::Uncovered => "uncovered",
        }
    }
}

/// One engine's verdict.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub engine: String,
    pub state: Coverage,
    /// How it leaves, or why it does not.
    pub how: String,
    pub gaps: Vec<String>,
    /// Its way out, for `[daemon] egress` to borrow.
    pub exit: EngineExit,
}

impl Verdict {
    /// The startup line, the API's `summary` entry and the tab's line.
    pub fn line(&self) -> String {
        match self.state {
            Coverage::Covered => format!("{}: covered -- {}", self.engine, self.how),
            Coverage::Direct => format!("{}: direct on purpose -- {}", self.engine, self.how),
            Coverage::Blocked => format!("{}: BLOCKED -- {}", self.engine, self.how),
            Coverage::Uncovered => format!("{}: NOT covered -- {}", self.engine, self.how),
        }
    }
}

/// The engine leaves by the default route on purpose. A WireGuard block
/// saved while it had a tick box wrote `wireguard_enabled = false` for an
/// engine left unticked: that was the same choice, said the old way.
pub fn marked_direct(mode: &str, s: &Session) -> bool {
    s.allow_direct || (mode == "wireguard" && s.wireguard_enabled == Some(false))
}

/// The WireGuard block's choice for this engine: "tunnel", "direct" or
/// "none" (unassigned, which the kill switch blocks).
pub fn wg_assignment(s: &Session) -> &'static str {
    if tunnel_assigned("wireguard", s) {
        "tunnel"
    } else if marked_direct("wireguard", s) {
        "direct"
    } else {
        "none"
    }
}

/// The engine has a managed tunnel assigned (`wgtunnel::wanted`).
fn tunnel_assigned(mode: &str, s: &Session) -> bool {
    mode == "wireguard" && s.wireguard_enabled == Some(true) && !s.wireguard_config.trim().is_empty()
}

/// One engine's verdict. `e` is as `Config::local_engines` resolved it (a
/// tunnelled engine already pinned to its device); `default_iface` is the
/// interface holding the default route, which inside gluetun is its tunnel.
pub fn engine_verdict(
    mode: &str,
    armed: bool,
    e: &LocalEngine,
    tunnel_dns_leak: Option<bool>,
    default_iface: Option<&str>,
) -> Verdict {
    let s = &e.session;
    let iface = s.bind_interface.trim();
    let proxied = !s.socks5_outbound_host.trim().is_empty();
    let tunnelled = tunnel_assigned(mode, s);
    let mut gaps: Vec<String> = Vec::new();
    let own = (tunnelled || !iface.is_empty() || proxied).then(|| {
        Route::new(
            &s.socks5_outbound_host,
            s.socks5_outbound_port,
            &s.socks5_outbound_user,
            &s.socks5_outbound_pass,
            iface,
            false,
        )
    });
    // Armed, no way out: blocked. Disarmed: the same words, and it runs.
    let refuse = |why: String| if armed { (Coverage::Blocked, why) } else { (Coverage::Uncovered, format!("{why}; the kill switch is disarmed, so it leaves by the host")) };
    let (state, how) = if mode == "wireguard" {
        // The WireGuard block offers three choices per engine and nothing
        // else, so they are the only ones read here: an interface typed in
        // another mode is not a tunnel this page shows.
        if tunnelled {
            if tunnel_dns_leak == Some(true) {
                gaps.push("its WireGuard file has no DNS line: tracker names are resolved by the host's resolver".into());
            }
            (Coverage::Covered, format!("its WireGuard tunnel {iface} ({}): a tunnel that is down means no network, never the default route", s.wireguard_config.trim()))
        } else if marked_direct(mode, s) {
            (Coverage::Direct, "\"Direct (default interface)\" chosen in the WireGuard block".to_string())
        } else {
            refuse("not assigned: pick a tunnel or \"Direct (default interface)\" for it in the WireGuard block of the Network tab".into())
        }
    } else if !iface.is_empty() {
        if mode == "gluetun" && Some(iface) != default_iface {
            refuse(format!(
                "pinned to {iface}, not to the interface of gluetun's network ({}): it would leave beside the tunnel",
                default_iface.unwrap_or("none found")
            ))
        } else {
            if tunnel_dns_leak.is_none() && mode != "gluetun" {
                gaps.push(format!("tracker names are resolved by the host's resolver ({iface} is not a tunnel Hydranos manages)"));
            }
            (Coverage::Covered, format!("every socket pinned to {iface}: a missing interface means no network, never the default route"))
        }
    } else if proxied {
        gaps.push("incoming peers reach the host's own listen port".into());
        (Coverage::Covered, "peers, announces and webseeds through its SOCKS5 proxy; DHT, UDP trackers and outgoing uTP off".to_string())
    } else if mode == "gluetun" {
        (Coverage::Covered, "inside gluetun's network: gluetun's firewall is the kill switch".to_string())
    } else if marked_direct(mode, s) {
        (Coverage::Direct, "allow_direct = true: the host's default route, on purpose".to_string())
    } else if mode == "direct" && !armed {
        (Coverage::Direct, "direct mode: the host's default route".to_string())
    } else if mode == "direct" {
        refuse("[daemon] kill_switch = true in direct mode, and it has no interface, no proxy and no allow_direct".into())
    } else {
        refuse(format!("{mode} mode, and it has no proxy, no interface and no allow_direct"))
    };
    let blocked = (state == Coverage::Blocked).then(|| how.clone());
    Verdict {
        engine: e.id.clone(),
        state,
        how,
        gaps,
        exit: EngineExit { id: e.id.clone(), route: own, tunnelled, blocked },
    }
}

/// Every engine's verdict and the daemon's way out.
pub struct Plan {
    pub mode: String,
    pub armed: bool,
    pub engines: Vec<Verdict>,
    pub daemon: Resolved,
}

impl Plan {
    pub fn verdict(&self, id: &str) -> Option<&Verdict> {
        self.engines.iter().find(|v| v.engine == id)
    }

    /// Why this engine is kept off the network, if it is.
    pub fn blocked(&self, id: &str) -> Option<String> {
        self.verdict(id).and_then(|v| v.exit.blocked.clone())
    }

    /// The daemon's line, as the log and the tab say it.
    pub fn daemon_line(&self) -> String {
        format!("the daemon's own requests (tracker lists, IP filter lists, update check, webhooks, .torrent URLs): via {}", self.daemon.via)
    }
}

/// `[proxy]` with `[daemon] bind_interface`: what `egress = "proxy"` and
/// `"direct"` are made of.
pub fn proxy_route(cfg: &Config) -> Route {
    Route::new(
        &cfg.proxy.socks5_host,
        cfg.proxy.socks5_port,
        &cfg.proxy.socks5_user,
        &cfg.proxy.socks5_pass,
        &cfg.daemon.bind_interface,
        false,
    )
}

/// The whole decision, from the file.
pub fn plan_with(cfg: &Config, tunnel_dns_leak: &dyn Fn(&str) -> Option<bool>, default_iface: Option<&str>) -> Plan {
    let mode = crate::netmode::current(cfg).to_string();
    let armed = armed(cfg);
    let engines: Vec<Verdict> = cfg
        .local_engines()
        .iter()
        .map(|e| engine_verdict(&mode, armed, e, tunnel_dns_leak(&e.id), default_iface))
        .collect();
    let exits: Vec<EngineExit> = engines.iter().map(|v| v.exit.clone()).collect();
    let daemon = typhon_engine::egress::resolve(&mode, armed, &cfg.daemon.egress, &proxy_route(cfg), &exits);
    Plan { mode, armed, engines, daemon }
}

/// The decision for this host: its default interface read now.
pub fn plan(cfg: &Config) -> Plan {
    plan_with(cfg, &|_| None, crate::portmap::default_interface().as_deref())
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
    let p = plan(cfg);
    let shown = p.daemon.via.clone();
    if typhon_engine::egress::set(p.daemon.route) {
        tracing::info!("egress: the daemon's own requests now go via {shown}");
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

/// The whole report, for `GET /api/network/egress`, `GET /api/network/mode`
/// and the startup log. `running_blocked` says which engines the RUNNING
/// process kept off the network (the file may have changed since).
pub fn report_of(cfg: &Config, p: &Plan, running_blocked: &dyn Fn(&str) -> Option<bool>) -> Value {
    let route = &p.daemon.route;
    let engines: Vec<Value> = p
        .engines
        .iter()
        .map(|v| {
            json!({
                "engine": v.engine,
                "state": v.state.word(),
                "covered": matches!(v.state, Coverage::Covered | Coverage::Direct),
                "blocked": v.state == Coverage::Blocked,
                "blocked_now": running_blocked(&v.engine),
                "allow_direct": cfg.local_engines().iter().find(|e| e.id == v.engine).is_some_and(|e| marked_direct(&p.mode, &e.session)),
                "how": v.how,
                "gaps": v.gaps,
                "line": v.line(),
            })
        })
        .collect();
    let mut summary: Vec<String> = p.engines.iter().map(Verdict::line).collect();
    summary.push(p.daemon_line());
    json!({
        "mode": p.mode,
        "armed": p.armed,
        "armed_why": armed_why(cfg),
        // As written: null = deduced from the mode.
        "kill_switch": cfg.daemon.kill_switch,
        "egress": if cfg.daemon.egress.trim().is_empty() { "auto" } else { cfg.daemon.egress.trim() },
        "daemon": {
            "via": p.daemon.via,
            "via_kind": p.daemon.kind,
            "via_engine": p.daemon.engine,
            "route": route.describe(),
            "socks5_host": cfg.proxy.socks5_host,
            "socks5_port": cfg.proxy.socks5_port,
            "socks5_user": cfg.proxy.socks5_user,
            "socks5_pass": cfg.proxy.socks5_pass,
            "bind_interface": cfg.daemon.bind_interface,
            "direct": route.is_direct() && route.refusal.is_empty(),
            "error": route.preflight().err().unwrap_or_default(),
        },
        "engines": engines,
        "all_engines_covered": p.engines.iter().all(|v| matches!(v.state, Coverage::Covered | Coverage::Direct)),
        "blocked": p.engines.iter().filter(|v| v.state == Coverage::Blocked).map(|v| v.engine.clone()).collect::<Vec<_>>(),
        "summary": summary,
        "not_covered": NOT_COVERED,
    })
}

/// The report from the file alone.
#[cfg(test)]
pub fn report(cfg: &Config, tunnel_dns_leak: &dyn Fn(&str) -> Option<bool>) -> Value {
    let p = plan_with(cfg, tunnel_dns_leak, crate::portmap::default_interface().as_deref());
    report_of(cfg, &p, &|_| None)
}

/// The startup lines: one per engine, one for the daemon, and the gaps.
pub fn log_startup(cfg: &Config, tunnel_dns_leak: &dyn Fn(&str) -> Option<bool>) {
    let p = plan_with(cfg, tunnel_dns_leak, crate::portmap::default_interface().as_deref());
    tracing::info!("kill switch: {}", armed_why(cfg));
    for v in &p.engines {
        match v.state {
            Coverage::Covered | Coverage::Direct => tracing::info!(engine = %v.engine, "kill switch: {}", v.line()),
            Coverage::Blocked | Coverage::Uncovered => tracing::warn!(engine = %v.engine, "kill switch: {}", v.line()),
        }
        for g in &v.gaps {
            tracing::warn!(engine = %v.engine, "kill switch: not covered -- {g}");
        }
    }
    match p.daemon.route.preflight() {
        Err(e) => tracing::warn!("kill switch: {} -- REFUSED: {e}", p.daemon_line()),
        Ok(()) => tracing::info!("kill switch: {}", p.daemon_line()),
    }
    if p.armed {
        for g in NOT_COVERED {
            tracing::info!("kill switch: outside its scope -- {g}");
        }
    }
}

/// Whether the file now asks for another set of blocked engines than the
/// running process has. Blocking is decided once, when an engine joins the
/// network, so a change here is a restart like any other network change.
pub fn blocking_changed(cfg: &Config, host: &crate::engines::EngineHost) -> bool {
    let p = plan(cfg);
    host.engines().iter().any(|e| e.blocked.get().is_some() != p.blocked(&e.id).is_some())
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
    let p = plan_with(
        &cfg,
        &|id: &str| reg.get(id).filter(|t| t.created).map(|t| t.dns_leak),
        crate::portmap::default_interface().as_deref(),
    );
    let host = &state.engines;
    report_of(&cfg, &p, &|id: &str| host.engines().iter().find(|e| e.id == id).map(|e| e.blocked.get().is_some()))
}

/// The report, for `GET /api/network/mode` too.
pub fn live(state: &AppState) -> Value {
    live_report(state)
}

/// `GET /api/network/egress`: the kill switch, each engine's verdict and the
/// daemon's way out.
pub async fn get_egress(State(state): State<AppState>, RawQuery(query): RawQuery, headers: HeaderMap) -> Response {
    let query = query.unwrap_or_default();
    if !crate::api::authorised(&state, &headers, &query) {
        return refuse();
    }
    Json(live_report(&state)).into_response()
}

/// `POST /api/network/egress`. Every field is optional and only what is sent
/// is written:
/// - `egress`: "auto" (removes the key), "direct", "proxy", "engine:<id>";
/// - `socks5_host/port/user/pass`: `[proxy]` (an empty host removes it);
/// - `bind_interface`: `[daemon] bind_interface`;
/// - `kill_switch`: true / false, or null / "auto" to remove the key and
///   follow the mode;
/// - `allow_direct`: `{ "<engine>": bool }`.
///
/// The daemon's route applies at once; `allow_direct` decides whether an
/// engine goes on the network, which is done at startup, so the answer says
/// when a restart is needed.
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
    let bad = |msg: String| (StatusCode::BAD_REQUEST, Json(json!({"error": msg}))).into_response();
    let Ok(v) = serde_json::from_str::<Value>(&body) else {
        return bad("the body is not JSON".into());
    };
    let txt = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let egress = match v.get("egress") {
        None => None,
        Some(e) => {
            let e = e.as_str().unwrap_or_default().trim().to_string();
            match typhon_engine::egress::Setting::parse(&e) {
                Err(why) => return bad(why),
                Ok(typhon_engine::egress::Setting::Engine(id)) => {
                    if !state.cfg().local_engines().iter().any(|le| le.id == id) {
                        return bad(format!("no engine {id:?} on this node"));
                    }
                    Some(format!("engine:{id}"))
                }
                Ok(typhon_engine::egress::Setting::Auto) => Some(String::new()),
                Ok(_) => Some(e.to_ascii_lowercase()),
            }
        }
    };
    let proxy = if v.get("socks5_host").is_some() {
        let host = txt("socks5_host");
        let port = v.get("socks5_port").and_then(Value::as_i64).unwrap_or(0);
        if !(0..=65535).contains(&port) || (!host.is_empty() && port == 0) {
            return bad("the proxy port must be between 1 and 65535".into());
        }
        let pass = v.get("socks5_pass").and_then(Value::as_str).unwrap_or_default().to_string();
        Some((host, port, txt("socks5_user"), pass))
    } else {
        None
    };
    let iface = v.get("bind_interface").map(|_| txt("bind_interface"));
    // Absent: untouched. null or "auto": follow the mode.
    let kill: Option<Option<bool>> = match v.get("kill_switch") {
        None => None,
        Some(Value::Bool(b)) => Some(Some(*b)),
        Some(Value::Null) => Some(None),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("auto") || s.is_empty() => Some(None),
        Some(_) => return bad("kill_switch is true, false, or null to follow the network mode".into()),
    };
    let local: Vec<String> = state.cfg().local_engines().into_iter().map(|e| e.id).collect();
    let mut direct: Vec<(String, bool)> = Vec::new();
    if let Some(m) = v.get("allow_direct") {
        let Some(m) = m.as_object() else {
            return bad("allow_direct is an object: {\"<engine>\": true}".into());
        };
        for (id, on) in m {
            if !local.contains(id) {
                return bad(format!("no engine {id:?} on this node"));
            }
            direct.push((id.clone(), on.as_bool().unwrap_or(false)));
        }
    }
    let q = crate::tomledit::quote_toml_key;
    let ok = crate::api::edit_config(&state, |doc| {
        let mut out = doc.to_string();
        if let Some((host, port, user, pass)) = &proxy {
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
                        ("socks5_host".to_string(), q(host)),
                        ("socks5_port".to_string(), port.to_string()),
                        ("socks5_user".to_string(), q(user)),
                        ("socks5_pass".to_string(), q(pass)),
                    ],
                )?;
            }
        }
        match &iface {
            Some(i) if i.is_empty() => out = crate::tomledit::delete_toml_key(&out, "daemon", "bind_interface"),
            Some(i) => out = crate::tomledit::set_toml_table(&out, "daemon", &[("bind_interface".to_string(), q(i))])?,
            None => {}
        }
        match kill {
            Some(Some(b)) => {
                out = crate::tomledit::set_toml_table(&out, "daemon", &[("kill_switch".to_string(), b.to_string())])?
            }
            Some(None) => out = crate::tomledit::delete_toml_key(&out, "daemon", "kill_switch"),
            None => {}
        }
        match &egress {
            Some(e) if e.is_empty() => out = crate::tomledit::delete_toml_key(&out, "daemon", "egress"),
            Some(e) => out = crate::tomledit::set_toml_table(&out, "daemon", &[("egress".to_string(), q(e))])?,
            None => {}
        }
        for (id, on) in &direct {
            out = set_allow_direct(&out, id, *on)?;
        }
        Ok(out)
    });
    if !ok {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "the config could not be written"})))
            .into_response();
    }
    let mut r = live_report(&state);
    r["restart_required"] = json!(blocking_changed(&state.cfg(), &state.engines));
    Json(r).into_response()
}

/// Write or remove one engine's `allow_direct`. Removed rather than written
/// `false`, so an extra engine's block stays as short as its owner wrote it.
pub fn set_allow_direct(doc: &str, id: &str, on: bool) -> Result<String, String> {
    if id == "race" || id == "hoard" {
        if on {
            crate::tomledit::set_toml_table(doc, id, &[("allow_direct".to_string(), "true".to_string())])
        } else {
            Ok(crate::tomledit::delete_toml_key(doc, id, "allow_direct"))
        }
    } else if on {
        crate::tomledit::set_agent_session_key(doc, id, "allow_direct", "true")
            .ok_or_else(|| format!("engine {id}: its block was not found in the file"))
    } else {
        Ok(crate::tomledit::delete_agent_session_key(doc, id, "allow_direct").unwrap_or_else(|| doc.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn cfg(toml_text: &str) -> Config {
        toml::from_str(toml_text).unwrap()
    }

    /// The plan as a test sees it: no tunnel known, `eth0` holds the default
    /// route (which inside gluetun would be its `tun0`).
    fn plan_of(text: &str) -> Plan {
        plan_with(&cfg(text), &|_| None, Some("eth0"))
    }

    fn state(p: &Plan, id: &str) -> Coverage {
        p.verdict(id).unwrap_or_else(|| panic!("no engine {id}")).state.clone()
    }

    /// ⭐⭐ The maintainer's own production: no `[network] mode`, no
    /// `kill_switch`, engines on the default route. Nothing changes: the
    /// switch is disarmed, no engine is blocked, the daemon goes direct and
    /// is not refused.
    #[test]
    fn a_production_file_without_mode_or_kill_switch_blocks_nothing() {
        let text = "[daemon]\napi_host = \"0.0.0.0\"\napi_port = 8199\napi_key = \"k\"\ndata_dir = \"/data\"\n\
                    [race]\nlisten_port = 16171\nmax_connections = 500\n[hoard]\nlisten_port = 16172\nenable_dht = false\n\
                    [[engine]]\nname = \"vpn1\"\nrole = \"hoard\"\n[engine.session]\nlisten_port = 26991\n\
                    [announce_ip_modes]\n\"tracker.example\" = \"v4\"\n";
        let c = cfg(text);
        assert_eq!(crate::netmode::current(&c), "direct");
        assert!(!armed(&c));
        let p = plan_of(text);
        assert!(p.engines.iter().all(|v| v.state == Coverage::Direct && v.exit.blocked.is_none()), "{:?}", p.engines);
        assert_eq!(p.daemon.kind, "direct");
        assert!(p.daemon.route.is_direct() && !p.daemon.route.kill_switch);
        assert!(p.daemon.route.preflight().is_ok());
        let r = report_of(&c, &p, &|_| None);
        assert_eq!(r["armed"], false);
        assert!(r["blocked"].as_array().unwrap().is_empty());
        assert_eq!(r["summary"].as_array().unwrap().len(), 4, "one line per engine and one for the daemon: {r:#}");
    }

    /// ⭐ WireGuard: a tunnel or "Direct" starts, unassigned is blocked; the
    /// verdicts say which, and why.
    #[test]
    fn in_wireguard_mode_an_unassigned_engine_is_blocked() {
        let p = plan_of(
            "[network]\nmode = \"wireguard\"\n\
             [race]\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\n\
             [hoard]\nallow_direct = true\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26991\n",
        );
        assert!(p.armed);
        assert_eq!(state(&p, "race"), Coverage::Covered);
        assert!(p.verdict("race").unwrap().how.contains("wg-race"));
        assert_eq!(state(&p, "hoard"), Coverage::Direct);
        assert_eq!(state(&p, "vpn1"), Coverage::Blocked);
        assert!(p.blocked("race").is_none() && p.blocked("hoard").is_none());
        let why = p.blocked("vpn1").unwrap();
        assert!(why.contains("not assigned"), "{why}");
        assert!(p.verdict("vpn1").unwrap().line().contains("BLOCKED"));
        // A tunnel switched on with no file named is no tunnel.
        let p = plan_of("[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = true\n[hoard]\nallow_direct = true\n");
        assert_eq!(state(&p, "race"), Coverage::Blocked);
    }

    /// ⭐ Migration. A file saved by the tick box: an engine left unticked
    /// (`wireguard_enabled = false`, written) was on the default route on
    /// purpose and stays there. An `[[engine]]` added afterwards has no such
    /// line, and arrives unassigned -- also when its role was direct.
    #[test]
    fn an_unticked_engine_migrates_to_direct_and_a_new_one_arrives_unassigned() {
        let p = plan_of(
            "[network]\nmode = \"wireguard\"\n\
             [race]\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\n\
             [hoard]\nwireguard_enabled = false\nwireguard_config = \"\"\n\
             [[engine]]\nengine_id = \"late\"\nrole = \"hoard\"\n[engine.session]\nlisten_port = 26992\n",
        );
        assert_eq!(state(&p, "hoard"), Coverage::Direct, "unticked and saved = direct");
        assert_eq!(state(&p, "late"), Coverage::Blocked, "created after the save = unassigned");
        let c = cfg("[network]\nmode = \"wireguard\"\n[hoard]\nwireguard_enabled = false\n");
        let hoard = c.local_engines().into_iter().find(|e| e.id == "hoard").unwrap();
        assert_eq!(wg_assignment(&hoard.session), "direct");
        // An extra engine inherits neither its role's tick box nor its
        // role's allow_direct.
        let p = plan_of(
            "[network]\nmode = \"socks5\"\n[hoard]\nallow_direct = true\n\
             [[engine]]\nengine_id = \"x\"\nrole = \"hoard\"\n[engine.session]\nlisten_port = 26993\n",
        );
        assert_eq!(state(&p, "hoard"), Coverage::Direct);
        assert_eq!(state(&p, "x"), Coverage::Blocked);
    }

    /// ⭐ SOCKS5: an engine without a proxy is blocked unless it is marked
    /// direct; PROXY v2 alike; and `kill_switch = false` disarms all of it.
    #[test]
    fn behind_socks5_an_engine_without_a_proxy_is_blocked_unless_direct() {
        let text = "[network]\nmode = \"socks5\"\n[race]\nsocks5_outbound_host = \"10.0.0.1\"\n\
                    [hoard]\nlisten_port = 16172\n";
        let p = plan_of(text);
        assert_eq!(state(&p, "race"), Coverage::Covered);
        assert_eq!(state(&p, "hoard"), Coverage::Blocked);
        assert!(p.blocked("hoard").unwrap().contains("socks5 mode"));
        let p = plan_of(&text.replace("listen_port = 16172", "allow_direct = true"));
        assert_eq!(state(&p, "hoard"), Coverage::Direct, "a mix, on purpose");
        let p = plan_of(&text.replace("mode = \"socks5\"", "mode = \"proxy_v2\""));
        assert_eq!(state(&p, "hoard"), Coverage::Blocked);
        // Disarmed: the same engine runs, named as not covered.
        let p = plan_of(&format!("[daemon]\nkill_switch = false\n{text}"));
        assert!(!p.armed);
        assert_eq!(state(&p, "hoard"), Coverage::Uncovered);
        assert!(p.blocked("hoard").is_none());
        assert!(p.verdict("hoard").unwrap().line().contains("NOT covered"));
    }

    /// gluetun: everything inside its network is covered; an engine pinned
    /// to another interface would leave beside the tunnel, and is blocked.
    #[test]
    fn inside_gluetun_an_engine_pinned_elsewhere_is_blocked() {
        let p = plan_with(
            &cfg("[network]\nmode = \"gluetun\"\n[race]\nbind_interface = \"tun0\"\n[hoard]\nbind_interface = \"eth1\"\n\
                  [[engine]]\nengine_id = \"x\"\nrole = \"race\"\n[engine.session]\nbind_interface = \"\"\nlisten_port = 26994\n"),
            &|_| None,
            Some("tun0"),
        );
        assert_eq!(state(&p, "race"), Coverage::Covered);
        assert_eq!(state(&p, "x"), Coverage::Covered, "unpinned = inside the netns");
        assert_eq!(state(&p, "hoard"), Coverage::Blocked);
        assert!(p.blocked("hoard").unwrap().contains("eth1"));
        assert_eq!(p.daemon.kind, "gluetun");
        assert!(p.daemon.route.preflight().is_ok(), "direct inside the netns is not a leak");
    }

    /// `kill_switch = true` in direct mode arms it: nothing without a way out
    /// leaves, the daemon included, until something is set.
    #[test]
    fn kill_switch_true_arms_even_the_direct_mode() {
        let p = plan_of("[daemon]\nkill_switch = true\n[race]\nbind_interface = \"wg0\"\n");
        assert!(p.armed);
        assert_eq!(state(&p, "race"), Coverage::Covered);
        assert_eq!(state(&p, "hoard"), Coverage::Blocked);
        assert_eq!(p.daemon.kind, "refused");
        assert!(p.daemon.route.preflight().unwrap_err().contains("direct mode"));
        // The daemon's own interface is a way out.
        let p = plan_of("[daemon]\nkill_switch = true\nbind_interface = \"lo\"\n");
        assert!(p.daemon.route.preflight().is_ok());
    }

    /// ⭐ `[daemon] egress = "auto"` in each mode, and a way out that is not
    /// there refuses -- never direct in its place.
    #[test]
    fn the_daemon_follows_the_mode_and_refuses_a_missing_way_out() {
        let direct = plan_of("");
        assert_eq!((direct.daemon.kind, direct.daemon.route.is_direct()), ("direct", true));

        let socks = plan_of(
            "[network]\nmode = \"socks5\"\n[race]\nsocks5_outbound_host = \"10.0.0.1\"\nsocks5_outbound_port = 1081\n\
             [hoard]\nsocks5_outbound_host = \"10.0.0.2\"\n",
        );
        assert_eq!((socks.daemon.kind, socks.daemon.engine.as_str()), ("engine_proxy", "race"), "race first");
        assert_eq!(socks.daemon.route.proxy, "socks5h://10.0.0.1:1081");
        assert!(socks.daemon.route.kill_switch);
        let hoard_only = plan_of("[network]\nmode = \"socks5\"\n[hoard]\nsocks5_outbound_host = \"10.0.0.2\"\n[race]\nallow_direct = true\n");
        assert_eq!(hoard_only.daemon.engine, "hoard", "else the first engine with a proxy");

        let wg = plan_of(
            "[network]\nmode = \"wireguard\"\n[race]\nallow_direct = true\n\
             [hoard]\nwireguard_enabled = true\nwireguard_config = \"h.conf\"\n",
        );
        assert_eq!((wg.daemon.kind, wg.daemon.route.interface.as_str()), ("engine_tunnel", "wg-hoard"), "race has none: the first tunnelled");
        let wg_race = plan_of(
            "[network]\nmode = \"wireguard\"\n[race]\nwireguard_enabled = true\nwireguard_config = \"r.conf\"\n\
             [hoard]\nwireguard_enabled = true\nwireguard_config = \"h.conf\"\n",
        );
        assert_eq!(wg_race.daemon.route.interface, "wg-race");

        let gl = plan_of("[network]\nmode = \"gluetun\"\n");
        assert_eq!(gl.daemon.kind, "gluetun");

        // Nothing to borrow: refused, with the mode's reason.
        for (text, word) in [
            ("[network]\nmode = \"wireguard\"\n", "WireGuard"),
            ("[network]\nmode = \"socks5\"\n[race]\nallow_direct = true\n[hoard]\nallow_direct = true\n", "socks5"),
        ] {
            let p = plan_of(text);
            assert_eq!(p.daemon.kind, "refused", "{text}");
            let e = typhon_engine::egress::client_for(&p.daemon.route).unwrap_err();
            assert!(e.contains(word), "{e}");
        }
        // Explicit settings.
        let named = plan_of("[daemon]\negress = \"engine:hoard\"\n[hoard]\nbind_interface = \"lo\"\n");
        assert_eq!((named.daemon.kind, named.daemon.route.interface.as_str()), ("engine_interface", "lo"));
        let blocked = plan_of("[network]\nmode = \"socks5\"\n[daemon]\negress = \"engine:hoard\"\n[race]\nsocks5_outbound_host = \"10.0.0.1\"\n");
        assert!(blocked.daemon.route.preflight().unwrap_err().contains("blocked"), "a blocked engine lends nothing");
        let proxy = plan_of("[daemon]\negress = \"proxy\"\n[proxy]\nsocks5_host = \"10.0.0.9\"\nsocks5_port = 1080\n");
        assert_eq!(proxy.daemon.route.proxy, "socks5h://10.0.0.9:1080");
        let no_proxy = plan_of("[daemon]\negress = \"proxy\"\n");
        assert_eq!(no_proxy.daemon.kind, "refused");
        let chosen = plan_of("[network]\nmode = \"wireguard\"\n[daemon]\negress = \"direct\"\n");
        assert!(chosen.daemon.route.preflight().is_ok(), "direct chosen is not refused");
        let typo = plan_of("[daemon]\negress = \"tunnel\"\n");
        assert_eq!(typo.daemon.kind, "refused", "a typo is not taken for auto");
        let ghost = plan_of("[daemon]\negress = \"engine:nope\"\n");
        assert!(ghost.daemon.route.preflight().unwrap_err().contains("nope"));
        // Disarmed WireGuard with nothing to lend: direct, as before.
        let off = plan_of("[network]\nmode = \"wireguard\"\n[daemon]\nkill_switch = false\n");
        assert!(off.daemon.route.is_direct() && off.daemon.route.preflight().is_ok());
    }

    /// The report names coverage, gaps and the refusal.
    #[test]
    fn the_report_names_each_engine_and_the_daemon() {
        let c = cfg("[network]\nmode = \"socks5\"\n[race]\nbind_interface = \"wg-race\"\n[hoard]\nsocks5_outbound_host = \"10.0.0.1\"\n\
                     [[engine]]\nname = \"bare\"\nrole = \"race\"\n[engine.session]\nbind_interface = \"\"\n");
        let r = report(&c, &|id| if id == "race" { Some(false) } else { None });
        let by = |id: &str| r["engines"].as_array().unwrap().iter().find(|e| e["engine"] == id).cloned().unwrap();
        assert_eq!(by("race")["state"], "covered");
        assert!(by("race")["gaps"].as_array().unwrap().is_empty(), "a managed tunnel with DNS has no name leak");
        assert_eq!(by("hoard")["covered"], true);
        assert!(by("hoard")["gaps"][0].as_str().unwrap().contains("incoming"));
        assert_eq!(by("bare")["state"], "blocked", "{r:#}");
        assert_eq!(r["blocked"], json!(["bare"]));
        assert_eq!(r["all_engines_covered"], false);
        assert_eq!(r["kill_switch"], Value::Null, "absent = deduced");
        assert_eq!(r["egress"], "auto");
        assert!(r["summary"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("bare: BLOCKED")));
        assert!(r["summary"].as_array().unwrap().last().unwrap().as_str().unwrap().contains("via the proxy of engine hoard"), "{r:#}");
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

    /// The shipped template documents the new keys commented out, nothing it
    /// holds is called dead, and it is disarmed (direct mode, no key).
    #[test]
    fn the_template_documents_the_new_keys_and_none_is_dead() {
        let t = include_str!("../../../configs/default.toml");
        for k in ["# kill_switch", "# bind_interface", "# egress", "# allow_direct", "[proxy]", "# socks5_host"] {
            assert!(t.contains(k), "the template documents {k}");
        }
        assert!(t.contains("where the daemon's own requests (tracker lists, IP filter lists, update check, webhooks, .torrent URLs) go"));
        assert!(crate::deadkeys::config_warnings(t).is_empty(), "{:?}", crate::deadkeys::config_warnings(t));
        let c = cfg(t);
        assert_eq!(c.daemon.kill_switch, None, "deduced from the mode unless written");
        assert!(!armed(&c));
        assert!(plan(&c).daemon.route.is_direct());
        // The new keys, written, are not dead either.
        let written = "[daemon]\nkill_switch = false\negress = \"engine:race\"\n[race]\nallow_direct = true\n\
                       [[engine]]\nname = \"x\"\nrole = \"race\"\n[engine.session]\nallow_direct = true\n";
        assert!(crate::deadkeys::config_warnings(written).is_empty(), "{:?}", crate::deadkeys::config_warnings(written));
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
