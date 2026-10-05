//! Managed WireGuard: one tunnel per engine, brought up by Hydranos itself.
//!
//! `wg-quick` is never run, and that is the point. A provider config carries
//! `AllowedIPs = 0.0.0.0/0`, which `wg-quick` reads as "make this the machine's
//! default route": on a host that also serves a library, or has to stay
//! reachable, that is a change with no undo. The `.conf` is parsed
//! (`wgtun::parse`), never executed, and every route installed here goes into
//! a routing table of the tunnel's own.
//!
//! Routing: the engine pins every socket to its device (`SO_BINDTODEVICE`,
//! `bind_interface = wg-<engine>`), and ONE rule sends what is bound to that
//! device to the tunnel's table: `ip rule add oif wg-<engine> lookup <table>`.
//! Chosen over a fwmark rule because the two fail differently. A socket whose
//! mark did not stick falls through to the main table and leaves by the
//! host's default route, in silence. A socket bound to a device that is not
//! there fails with ENODEV: the engine cannot reach anybody, which is what has
//! to happen when the tunnel is down. Nothing that is not bound to the device
//! matches the rule, so `ip route` and the rest of `ip rule` are left as they
//! were -- including WireGuard's own encrypted packets, which keep leaving by
//! the default route as they must.
//!
//! The commands are a PLAN (a list of argv) built by pure functions and run by
//! a `Runner`: the plan is what the unit tests check, the real runner is what
//! the root-only test at the bottom checks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::wgtun::{self, Conf};

/// Every device this module creates starts with this.
pub const PREFIX: &str = "wg-";
/// Linux interface names are at most 15 bytes (IFNAMSIZ - 1).
const IFNAMSIZ_MAX: usize = 15;
/// Routing tables, one per tunnel, from here. Far from 51820, which is the
/// table AND fwmark `wg-quick` picks by default: under `--network host` a
/// tunnel of the host's own lives there, and it is not ours to touch.
pub const TABLE_BASE: u32 = 0x4859_0000;
/// Rule preference, one per tunnel. Before `main` (32766), so the tunnel's
/// table is the one a bound socket meets first.
pub const PREF_BASE: u32 = 5100;
/// A handshake older than this and the tunnel is not carrying anything:
/// WireGuard re-handshakes every two minutes while traffic flows.
pub const HANDSHAKE_FRESH_SECS: u64 = 180;
/// The default MTU of `wg-quick`, for a `.conf` that does not say.
const DEFAULT_MTU: u32 = 1420;

/// The device for an engine: `wg-<engine>`, at most 15 bytes.
///
/// A long or exotic engine id is cut and given a short hash of the whole id,
/// so two ids sharing their first characters still get two devices. Anything
/// outside `[A-Za-z0-9_-]` becomes `_`: a `/` or a space is not a valid
/// interface name, and `ip` would refuse it after the engine was told to bind
/// there.
pub fn device_name(engine_id: &str) -> String {
    let clean: String = engine_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let room = IFNAMSIZ_MAX - PREFIX.len();
    if clean.len() <= room && clean == engine_id {
        return format!("{PREFIX}{clean}");
    }
    // FNV-1a: stable across builds and platforms, which a std hasher is not.
    let mut h: u32 = 0x811c_9dc5;
    for b in engine_id.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    let tag = format!("{:04x}", h & 0xffff);
    let keep = room.min(clean.len()).min(room - tag.len() - 1);
    format!("{PREFIX}{}-{tag}", &clean[..keep])
}

// ---------------------------------------------------------------------------
// What the operator asked for
// ---------------------------------------------------------------------------

/// How the forwarded port is obtained, from the provider table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortForward {
    /// Asked of the tunnel's gateway by NAT-PMP, and followed.
    NatPmp,
    /// Assigned on the provider's website and typed in by the operator.
    Manual,
    /// No forwarding: the engine takes no incoming connection.
    None,
}

/// The VPN providers Hydranos knows how to ask for a forwarded port.
///
/// Field names are capitalised because the Go struct carried no json tags,
/// and the list is ordered by LABEL, not by id -- that is what the picker
/// shows.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Provider {
    #[serde(rename = "ID")]
    pub id: &'static str,
    #[serde(rename = "Label")]
    pub label: &'static str,
    #[serde(rename = "PortForward")]
    pub port_forward: &'static str,
    #[serde(rename = "Note")]
    pub note: &'static str,
}

pub fn providers() -> Vec<Provider> {
    let mut list = vec![
        Provider { id: "proton", label: "Proton VPN", port_forward: "natpmp",
            note: "The port is obtained by NAT-PMP and renewed continuously. Use a server marked P2P." },
        Provider { id: "airvpn", label: "AirVPN", port_forward: "manual",
            note: "AirVPN assigns the port in the client area. Create it there, then type it here." },
        Provider { id: "mullvad", label: "Mullvad", port_forward: "none",
            note: "Mullvad removed port forwarding in 2023. This engine will take no incoming peer connections." },
        Provider { id: "pia", label: "Private Internet Access", port_forward: "manual",
            note: "PIA forwards ports through its own API, which needs the account credentials as well as the config. Not automated yet: set the port by hand, or run PIA behind gluetun." },
        Provider { id: "windscribe", label: "Windscribe", port_forward: "manual",
            note: "Windscribe assigns an ephemeral or static port on its web panel." },
        Provider { id: "natpmp", label: "Other (NAT-PMP capable)", port_forward: "natpmp",
            note: "For any provider whose gateway answers NAT-PMP, the way Proton does." },
        Provider { id: "generic", label: "Other / none", port_forward: "none",
            note: "The tunnel is brought up, no port is requested. Set a port by hand if the provider forwards one." },
    ];
    list.sort_by_key(|p| p.label);
    list
}

pub fn provider(id: &str) -> Option<Provider> {
    providers().into_iter().find(|p| p.id == id)
}

/// Proton's gateway, the same on every server: it is also the DNS address in
/// every Proton `.conf`.
const PROTON_GATEWAY: std::net::Ipv4Addr = std::net::Ipv4Addr::new(10, 2, 0, 1);

/// One engine's tunnel, as the config asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub engine: String,
    pub device: String,
    pub table: u32,
    pub pref: u32,
    pub config_file: String,
    pub provider: String,
    /// The port the provider assigned out of band, for `PortForward::Manual`.
    pub manual_port: u16,
    pub port_forward: PortForward,
}

/// How an engine's port is forwarded: the operator's override when written
/// ("manual", "off"/"none", "natpmp"), the provider's own way otherwise.
pub fn port_forward_of(provider_id: &str, override_mode: &str) -> PortForward {
    match override_mode.trim().to_ascii_lowercase().as_str() {
        "manual" => PortForward::Manual,
        "off" | "none" => PortForward::None,
        "natpmp" => PortForward::NatPmp,
        _ => match provider(provider_id).map(|p| p.port_forward) {
            Some("natpmp") => PortForward::NatPmp,
            Some("manual") => PortForward::Manual,
            _ => PortForward::None,
        },
    }
}

/// The tunnels the config asks for: none unless the mode is WireGuard, then
/// one per engine that has its tunnel switched on and a file named.
///
/// Built from the engines AS WRITTEN, before `apply` moved their interface:
/// it is the input of `apply`, not its output.
pub fn wanted(mode: &str, engines: &[crate::config::LocalEngine]) -> Vec<Want> {
    if mode != "wireguard" {
        return Vec::new();
    }
    engines
        .iter()
        .enumerate()
        .filter(|(_, e)| e.session.wireguard_enabled && !e.session.wireguard_config.trim().is_empty())
        .map(|(slot, e)| Want {
            engine: e.id.clone(),
            device: device_name(&e.id),
            table: TABLE_BASE + slot as u32,
            pref: PREF_BASE + slot as u32,
            config_file: e.session.wireguard_config.trim().to_string(),
            provider: e.session.wireguard_provider.trim().to_string(),
            manual_port: e.session.wireguard_port,
            port_forward: port_forward_of(&e.session.wireguard_provider, &e.session.wireguard_port_forward),
        })
        .collect()
}

/// Point each tunnelled engine at its device, and at the provider's port when
/// it was typed in. Called by `Config::local_engines`, so every reader of an
/// engine's session -- the engine itself, its announces, the network check,
/// the restart decision -- sees the interface the engine really uses.
///
/// An engine whose tunnel then fails to come up is still pinned to the
/// device: its sockets fail instead of leaving by the default route.
pub fn apply(mode: &str, engines: &mut [crate::config::LocalEngine]) {
    for w in wanted(mode, engines) {
        if let Some(e) = engines.iter_mut().find(|e| e.id == w.engine) {
            e.session.bind_interface = w.device.clone();
            if w.port_forward == PortForward::Manual && w.manual_port != 0 {
                e.session.listen_port = w.manual_port;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// What a failed step means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFail {
    /// The tunnel cannot come up.
    Abort,
    /// Expected: tearing down what may not be there.
    Ignore,
    /// The IPv6 half is lost, the IPv4 half carries on (and says so).
    DropV6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub argv: Vec<String>,
    /// Fed on stdin. Only `wg setconf` uses it: the private key never goes on
    /// a command line (`ps` shows those) nor in a file of ours.
    pub stdin: Option<String>,
    pub on_fail: OnFail,
    /// Skipped once the IPv6 half has been dropped.
    pub v6: bool,
    /// Run again until it fails: a crash can leave the same rule twice.
    pub repeat: bool,
}

fn step(argv: &[&str], on_fail: OnFail) -> Step {
    Step { argv: argv.iter().map(|s| s.to_string()).collect(), stdin: None, on_fail, v6: false, repeat: false }
}

fn is_v6(cidr: &str) -> bool {
    cidr.contains(':')
}

/// Remove a tunnel, whatever state it was left in. Every step may fail.
///
/// No route flush: the tunnel's table only ever holds routes through its
/// device, and the kernel drops those with the device.
pub fn teardown_plan(device: &str) -> Vec<Step> {
    let mut v4 = step(&["ip", "-4", "rule", "del", "oif", device], OnFail::Ignore);
    v4.repeat = true;
    let mut v6 = step(&["ip", "-6", "rule", "del", "oif", device], OnFail::Ignore);
    v6.repeat = true;
    vec![v4, v6, step(&["ip", "link", "del", "dev", device], OnFail::Ignore)]
}

/// Bring one tunnel up from scratch.
///
/// Idempotent by construction: it starts with the teardown, so a device left
/// by a crash, a restart or an earlier version is rebuilt rather than reused
/// with whatever it carried.
pub fn up_plan(w: &Want, conf: &Conf) -> Result<Vec<Step>, String> {
    let dev = w.device.as_str();
    let has_v4 = conf.addresses.iter().any(|a| !is_v6(a));
    let has_v6 = conf.addresses.iter().any(|a| is_v6(a));
    if !has_v4 && !has_v6 {
        return Err(format!("{}: no Address in [Interface], the tunnel would have no source address", w.config_file));
    }
    let table = w.table.to_string();
    let pref = w.pref.to_string();
    let mut plan = teardown_plan(dev);
    plan.push(step(&["ip", "link", "add", "dev", dev, "type", "wireguard"], OnFail::Abort));
    let mut setconf = step(&["wg", "setconf", dev, "/dev/stdin"], OnFail::Abort);
    setconf.stdin = Some(wgtun::setconf_text(conf));
    plan.push(setconf);
    for a in &conf.addresses {
        if is_v6(a) {
            let mut s = step(&["ip", "-6", "address", "add", a, "dev", dev], OnFail::DropV6);
            s.v6 = true;
            plan.push(s);
        } else {
            plan.push(step(&["ip", "-4", "address", "add", a, "dev", dev], OnFail::Abort));
        }
    }
    let mtu = if conf.mtu > 0 { conf.mtu } else { DEFAULT_MTU }.to_string();
    plan.push(step(&["ip", "link", "set", "dev", dev, "mtu", &mtu, "up"], OnFail::Abort));
    // The peers' AllowedIPs, into the tunnel's table only. `replace`, not
    // `add`: two peers may list the same range.
    let mut routes: Vec<&str> = conf.peers.iter().flat_map(|p| p.allowed_ips.iter().map(String::as_str)).collect();
    routes.dedup();
    for r in routes {
        if is_v6(r) {
            if has_v6 {
                let mut s = step(&["ip", "-6", "route", "replace", r, "dev", dev, "table", &table], OnFail::DropV6);
                s.v6 = true;
                plan.push(s);
            }
        } else if has_v4 {
            plan.push(step(&["ip", "-4", "route", "replace", r, "dev", dev, "table", &table], OnFail::Abort));
        }
    }
    if has_v4 {
        plan.push(step(&["ip", "-4", "rule", "add", "oif", dev, "lookup", &table, "pref", &pref], OnFail::Abort));
    }
    if has_v6 {
        let mut s = step(&["ip", "-6", "rule", "add", "oif", dev, "lookup", &table, "pref", &pref], OnFail::DropV6);
        s.v6 = true;
        plan.push(s);
    }
    Ok(plan)
}

/// Runs one command. The real one spawns it; the tests record it.
pub trait Runner {
    fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<String, String>;
}

/// How a plan went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Set when the IPv6 half was dropped, with why and what fixes it.
    pub degraded: Option<String>,
}

/// The fix named when a v6 address cannot be added: Docker mounts /proc/sys
/// read-only, so the process cannot turn IPv6 on for a device it just made.
pub const V6_FIX: &str = "IPv6 is disabled on new interfaces here; the tunnel carries IPv4 only. \
     To keep both, start the container with --sysctl net.ipv6.conf.default.disable_ipv6=0 \
     (or set that sysctl on the host under --network host).";

pub fn execute(runner: &mut dyn Runner, plan: &[Step]) -> Result<Outcome, String> {
    let mut out = Outcome::default();
    for s in plan {
        if s.v6 && out.degraded.is_some() {
            continue;
        }
        if s.repeat {
            for _ in 0..32 {
                if runner.run(&s.argv, s.stdin.as_deref()).is_err() {
                    break;
                }
            }
            continue;
        }
        match runner.run(&s.argv, s.stdin.as_deref()) {
            Ok(_) => {}
            Err(e) => match s.on_fail {
                OnFail::Ignore => {}
                OnFail::DropV6 => out.degraded = Some(format!("{V6_FIX} ({}: {e})", s.argv.join(" "))),
                OnFail::Abort => return Err(format!("{}: {e}", s.argv.join(" "))),
            },
        }
    }
    Ok(out)
}

/// The runner that does it.
pub struct System;

impl Runner for System {
    fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<String, String> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let (prog, args) = argv.split_first().ok_or("empty command")?;
        let mut child = Command::new(prog)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot run {prog}: {e}"))?;
        if let Some(text) = stdin {
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(text.as_bytes()).map_err(|e| format!("{prog}: {e}"))?;
            }
        }
        let out = child.wait_with_output().map_err(|e| format!("{prog}: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() { format!("exit {}", out.status) } else { err })
        }
    }
}

// ---------------------------------------------------------------------------
// Can this process do it at all
// ---------------------------------------------------------------------------

/// Whether `CapEff` in a `/proc/<pid>/status` text carries CAP_NET_ADMIN.
pub fn net_admin_in(status: &str) -> Option<bool> {
    const CAP_NET_ADMIN: u32 = 12;
    let hex = status.lines().find_map(|l| l.strip_prefix("CapEff:"))?.trim();
    let caps = u64::from_str_radix(hex, 16).ok()?;
    Some(caps & (1 << CAP_NET_ADMIN) != 0)
}

pub const NEEDS_LINUX: &str =
    "Managed WireGuard needs Linux. On this system, bring the tunnel up yourself and pick its interface in Direct mode.";
pub const NEEDS_NET_ADMIN: &str =
    "Managed WireGuard needs the NET_ADMIN capability, and Hydranos does not have it. In Docker, add cap_add: [NET_ADMIN] (docker run --cap-add=NET_ADMIN); with PUID/PGID, also set HYDRANOS_CAP_NET_ADMIN=1.";
pub const NEEDS_TOOLS: &str =
    "Managed WireGuard needs the ip (iproute2) and wg (wireguard-tools) commands, and one of them is not installed.";

/// Why this process cannot manage tunnels, from what it can see. Pure, so
/// each refusal can be tested on a machine that has every privilege.
pub fn support_from(linux: bool, status: Option<&str>, has_tool: &dyn Fn(&str) -> bool) -> Result<(), &'static str> {
    if !linux {
        return Err(NEEDS_LINUX);
    }
    if status.and_then(net_admin_in) != Some(true) {
        return Err(NEEDS_NET_ADMIN);
    }
    if !has_tool("ip") || !has_tool("wg") {
        return Err(NEEDS_TOOLS);
    }
    Ok(())
}

fn in_path(prog: &str) -> bool {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/sbin:/usr/bin:/sbin:/bin".into());
    std::env::split_paths(&path).any(|d| d.join(prog).is_file())
}

/// Whether this process can bring tunnels up, and if not, the sentence that
/// says why and what to change. Asked by the routes before anything is
/// written, and at boot before anything is run.
pub fn support() -> Result<(), &'static str> {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(|f| f.get()) {
        return forced;
    }
    let status = std::fs::read_to_string("/proc/self/status").ok();
    support_from(cfg!(target_os = "linux"), status.as_deref(), &in_path)
}

#[cfg(test)]
thread_local! {
    static FORCED: std::cell::Cell<Option<Result<(), &'static str>>> = const { std::cell::Cell::new(None) };
}

/// Answer `support()` with `v` on this thread, for the route tests: the test
/// container has neither NET_ADMIN nor `wg`, and both answers must be tested.
#[cfg(test)]
pub fn force_support(v: Option<Result<(), &'static str>>) {
    FORCED.with(|f| f.set(v));
}

// ---------------------------------------------------------------------------
// The .conf files
// ---------------------------------------------------------------------------

/// `<data_dir>/wireguard`, where the provider files live.
pub fn conf_dir(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join("wireguard")
}

/// A file name the store accepts: a bare name ending in `.conf`, of the
/// characters a provider actually uses. Anything else could step out of the
/// directory (`../`) or name a file that is not a tunnel.
pub fn valid_conf_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ok_chars = base.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if base.is_empty() || base.starts_with('.') || !ok_chars || base.len() > 96 {
        return Err(format!("{name:?} is not a usable file name: letters, digits, '.', '-' and '_' only"));
    }
    if !base.to_ascii_lowercase().ends_with(".conf") {
        return Err(format!("{name:?} is not a .conf file"));
    }
    Ok(base.to_string())
}

/// Validate and store one provider file at 0600.
///
/// Parsed before anything is written: a file that would not bring a tunnel
/// up is refused now, with the reason, rather than at the next boot. Written
/// to a temporary name and renamed, so a half-written key is never read.
pub fn store_conf(dir: &Path, name: &str, bytes: &[u8]) -> Result<(String, Conf), String> {
    let name = valid_conf_name(name)?;
    let text = std::str::from_utf8(bytes).map_err(|_| format!("{name} is not a text file"))?;
    let conf = wgtun::parse(text).map_err(|e| format!("{name}: {e}"))?;
    if conf.addresses.is_empty() {
        return Err(format!("{name}: no Address in [Interface], the tunnel would have no source address"));
    }
    if conf.peers.is_empty() {
        return Err(format!("{name}: no [Peer], the tunnel would lead nowhere"));
    }
    create_private_dir(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    write_private(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, dir.join(&name)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot store {name}: {e}")
    })?;
    Ok((name, conf))
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    // 0600 from the first byte, not chmod'ed after: in between, the key
    // would be readable by anyone on the host.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A file left by an earlier, laxer write keeps its old mode on open.
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    f.sync_all().map_err(|e| format!("cannot write {}: {e}", path.display()))
}

pub fn load_conf(dir: &Path, name: &str) -> Result<Conf, String> {
    let name = valid_conf_name(name)?;
    let text = std::fs::read_to_string(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
    wgtun::parse(&text).map_err(|e| format!("{name}: {e}"))
}

/// Remove a file. `Ok(false)` when there was none.
pub fn delete_conf(dir: &Path, name: &str) -> Result<bool, String> {
    let name = valid_conf_name(name)?;
    match std::fs::remove_file(dir.join(&name)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {name}: {e}")),
    }
}

/// What the API may say about one stored file: never its keys.
///
/// Built from `wgtun::redacted`, so a field added to the listing later
/// cannot carry a secret by accident.
pub fn list_confs(dir: &Path) -> Vec<serde_json::Value> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_conf_name(n).is_ok())
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| match load_conf(dir, &name) {
            Ok(conf) => {
                let r = wgtun::redacted(&conf);
                serde_json::json!({
                    "name": name,
                    "address": r.addresses.join(", "),
                    "endpoint": r.peers.first().map(|p| p.endpoint.clone()).unwrap_or_default(),
                    "peer_public_key": r.peers.first().map(|p| p.public_key.clone()).unwrap_or_default(),
                })
            }
            Err(e) => serde_json::json!({"name": name, "error": e}),
        })
        .collect()
}

/// The NAT-PMP gateway for a tunnel: Proton's fixed one, or for another
/// NAT-PMP provider the first IPv4 DNS server of its file -- on these
/// gateways the resolver and the mapper are the same box.
pub fn natpmp_gateway(provider_id: &str, conf: &Conf) -> Option<std::net::IpAddr> {
    if provider_id == "proton" {
        return Some(PROTON_GATEWAY.into());
    }
    conf.dns
        .iter()
        .filter_map(|d| d.parse::<std::net::IpAddr>().ok())
        .find(|ip| ip.is_ipv4())
}

// ---------------------------------------------------------------------------
// The devices we made, across restarts
// ---------------------------------------------------------------------------

/// The devices this node brought up, one per line, in `<data_dir>/wireguard`.
///
/// Needed to take down a tunnel the config no longer asks for -- an engine
/// removed, a mode changed while the process was down -- without ever
/// touching a `wg-something` the host made itself under `--network host`.
fn managed_path(dir: &Path) -> PathBuf {
    dir.join("managed.list")
}

pub fn read_managed(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(managed_path(dir))
        .map(|t| t.lines().map(str::trim).filter(|l| l.starts_with(PREFIX)).map(String::from).collect())
        .unwrap_or_default()
}

pub fn write_managed(dir: &Path, devices: &[String]) {
    if devices.is_empty() {
        let _ = std::fs::remove_file(managed_path(dir));
        return;
    }
    if create_private_dir(dir).is_ok() {
        let _ = std::fs::write(managed_path(dir), devices.join("\n") + "\n");
    }
}

// ---------------------------------------------------------------------------
// Live state
// ---------------------------------------------------------------------------

/// One tunnel as this process knows it.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Tunnel {
    pub engine: String,
    pub device: String,
    pub provider: String,
    pub provider_label: String,
    pub config_file: String,
    /// The plan ran to the end.
    pub created: bool,
    /// The IPv6 half was dropped, and why.
    pub degraded: String,
    /// The last failure: bring-up, or the port forward.
    pub last_error: String,
    /// 0 = none (yet, or ever for this provider).
    pub forwarded_port: u16,
    pub port_forward: String,
}

/// The tunnels of this process, by engine.
#[derive(Default)]
pub struct Registry {
    tunnels: Mutex<BTreeMap<String, Tunnel>>,
}

impl Registry {
    pub fn set(&self, t: Tunnel) {
        self.tunnels.lock().unwrap_or_else(|p| p.into_inner()).insert(t.engine.clone(), t);
    }

    pub fn update(&self, engine: &str, f: impl FnOnce(&mut Tunnel)) {
        if let Some(t) = self.tunnels.lock().unwrap_or_else(|p| p.into_inner()).get_mut(engine) {
            f(t);
        }
    }

    pub fn get(&self, engine: &str) -> Option<Tunnel> {
        self.tunnels.lock().unwrap_or_else(|p| p.into_inner()).get(engine).cloned()
    }

    pub fn all(&self) -> Vec<Tunnel> {
        self.tunnels.lock().unwrap_or_else(|p| p.into_inner()).values().cloned().collect()
    }

    /// Forget every tunnel, handing back what was there to take down.
    pub fn drain(&self) -> Vec<Tunnel> {
        std::mem::take(&mut *self.tunnels.lock().unwrap_or_else(|p| p.into_inner())).into_values().collect()
    }
}

/// What `wg` says about a device now: the newest handshake's age and the
/// peer's endpoint. `None` when the device is not there.
///
/// Asked field by field (`latest-handshakes`, `endpoints`), never `wg show
/// dump` or `wg showconf`: those print the private key, and this text ends
/// up in an API answer.
pub fn live(runner: &mut dyn Runner, device: &str) -> Option<(Option<u64>, String)> {
    let dev = device.to_string();
    let hs = runner.run(&["wg".into(), "show".into(), dev.clone(), "latest-handshakes".into()], None).ok()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let newest = hs
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        .filter(|t| *t > 0)
        .max()
        .map(|t| now.saturating_sub(t));
    let endpoint = runner
        .run(&["wg".into(), "show".into(), dev, "endpoints".into()], None)
        .ok()
        .and_then(|t| t.lines().find_map(|l| l.split_whitespace().nth(1).map(String::from)))
        .filter(|e| e != "(none)")
        .unwrap_or_default();
    Some((newest, endpoint))
}

/// Bring the wanted tunnels up and take down the ones no longer wanted.
///
/// Blocking (it runs `ip` and `wg`): call it off the async runtime. A tunnel
/// that fails is recorded with its reason and left down; its engine stays
/// pinned to the missing device and reaches nobody.
pub fn reconcile(runner: &mut dyn Runner, dir: &Path, wants: &[Want], registry: &Registry) {
    let wanted_devices: Vec<String> = wants.iter().map(|w| w.device.clone()).collect();
    for stale in read_managed(dir) {
        if !wanted_devices.contains(&stale) {
            let _ = execute(runner, &teardown_plan(&stale));
            tracing::info!(device = %stale, "wireguard: tunnel no longer asked for, taken down");
        }
    }
    if wants.is_empty() {
        write_managed(dir, &[]);
        return;
    }
    let supported = support();
    let mut made = Vec::new();
    for w in wants {
        let label = provider(&w.provider).map(|p| p.label).unwrap_or_default();
        let mut t = Tunnel {
            engine: w.engine.clone(),
            device: w.device.clone(),
            provider: w.provider.clone(),
            provider_label: label.to_string(),
            config_file: w.config_file.clone(),
            port_forward: match w.port_forward {
                PortForward::NatPmp => "natpmp",
                PortForward::Manual => "manual",
                PortForward::None => "none",
            }
            .into(),
            forwarded_port: if w.port_forward == PortForward::Manual { w.manual_port } else { 0 },
            ..Default::default()
        };
        let result = supported
            .map_err(String::from)
            .and_then(|()| load_conf(dir, &w.config_file))
            .and_then(|conf| up_plan(w, &conf))
            .and_then(|plan| {
                // Recorded before running: a plan that dies halfway still
                // leaves a device that must be taken down later.
                made.push(w.device.clone());
                write_managed(dir, &made);
                execute(runner, &plan)
            });
        match result {
            Ok(outcome) => {
                t.created = true;
                if let Some(d) = outcome.degraded {
                    tracing::warn!(engine = %w.engine, device = %w.device, "wireguard: {d}");
                    t.degraded = d;
                }
                tracing::info!(
                    engine = %w.engine, device = %w.device, table = w.table,
                    "wireguard: tunnel up; this engine's sockets are pinned to it"
                );
            }
            Err(e) => {
                let _ = execute(runner, &teardown_plan(&w.device));
                tracing::error!(
                    engine = %w.engine, device = %w.device, error = %e,
                    "wireguard: the tunnel did not come up; the engine stays pinned to it and reaches nobody (no direct fallback)"
                );
                t.last_error = e;
            }
        }
        registry.set(t);
    }
}

/// Take every tunnel of this process down. Blocking.
pub fn down_all(runner: &mut dyn Runner, dir: &Path, registry: &Registry) -> usize {
    let mut devices: Vec<String> = registry.drain().into_iter().map(|t| t.device).collect();
    for d in read_managed(dir) {
        if !devices.contains(&d) {
            devices.push(d);
        }
    }
    for d in &devices {
        let _ = execute(runner, &teardown_plan(d));
    }
    write_managed(dir, &[]);
    devices.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "\
[Interface]
PrivateKey = aPrivateKeyValue=
Address = 10.2.0.2/32, fd00::2/128
DNS = 10.2.0.1

[Peer]
PublicKey = aPublicKeyValue=
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = 192.0.2.10:51820
";

    fn want(engine: &str) -> Want {
        Want {
            engine: engine.into(),
            device: device_name(engine),
            table: TABLE_BASE,
            pref: PREF_BASE,
            config_file: "p.conf".into(),
            provider: "proton".into(),
            manual_port: 0,
            port_forward: PortForward::NatPmp,
        }
    }

    /// Records every command; fails the ones a closure says fail.
    struct Fake {
        ran: Vec<String>,
        stdin: Vec<String>,
        fail: Box<dyn Fn(&str) -> Option<String>>,
    }

    impl Fake {
        fn new(fail: impl Fn(&str) -> Option<String> + 'static) -> Self {
            Fake { ran: Vec::new(), stdin: Vec::new(), fail: Box::new(fail) }
        }
    }

    impl Runner for Fake {
        fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<String, String> {
            let line = argv.join(" ");
            self.ran.push(line.clone());
            if let Some(s) = stdin {
                self.stdin.push(s.to_string());
            }
            match (self.fail)(&line) {
                Some(e) => Err(e),
                None => Ok(String::new()),
            }
        }
    }

    #[test]
    fn a_device_is_named_after_its_engine_within_fifteen_bytes() {
        assert_eq!(device_name("race"), "wg-race");
        assert_eq!(device_name("hoard"), "wg-hoard");
        let long = device_name("a-very-long-engine-name");
        assert!(long.len() <= 15 && long.starts_with("wg-"), "{long}");
        // Two long ids sharing their start are still two devices.
        assert_ne!(device_name("tunnel-number-one"), device_name("tunnel-number-two"));
        // A character no interface name may carry never reaches `ip`.
        let odd = device_name("vpn/1 x");
        assert!(odd.len() <= 15 && !odd.contains('/') && !odd.contains(' '), "{odd}");
        assert_ne!(device_name("vpn_1"), device_name("vpn/1"), "cleaning alone would merge them");
    }

    /// ⭐ The plan never touches the main table, never adds a default route
    /// outside the tunnel's table, and never puts the key on a command line.
    #[test]
    fn the_plan_routes_only_inside_the_tunnels_own_table() {
        let conf = wgtun::parse(CONF).unwrap();
        let plan = up_plan(&want("race"), &conf).unwrap();
        let lines: Vec<String> = plan.iter().map(|s| s.argv.join(" ")).collect();
        let table = TABLE_BASE.to_string();
        for l in &lines {
            assert!(!l.contains("aPrivateKeyValue"), "the key is on a command line: {l}");
            if l.contains(" route ") && !l.contains(" del") {
                assert!(l.ends_with(&format!("table {table}")), "a route outside the tunnel's table: {l}");
            }
            if l.contains(" rule add") {
                assert!(l.contains("oif wg-race"), "a rule that matches more than this engine: {l}");
            }
        }
        assert!(lines.contains(&format!("ip -4 route replace 0.0.0.0/0 dev wg-race table {table}")));
        assert!(lines.contains(&format!("ip -6 route replace ::/0 dev wg-race table {table}")));
        assert!(lines.contains(&format!("ip -4 rule add oif wg-race lookup {table} pref {PREF_BASE}")));
        assert!(lines.contains(&"ip link set dev wg-race mtu 1420 up".to_string()), "{lines:#?}");
        let setconf = plan.iter().find(|s| s.argv[0] == "wg").unwrap();
        assert_eq!(setconf.argv, ["wg", "setconf", "wg-race", "/dev/stdin"]);
        let fed = setconf.stdin.as_deref().unwrap();
        assert!(fed.contains("PrivateKey = aPrivateKeyValue=") && !fed.contains("Address"), "{fed}");
    }

    /// ⭐ Idempotent: the plan starts by removing whatever an earlier run, a
    /// crash or a restart left -- rules (as many as there are) and the device.
    #[test]
    fn a_device_left_from_before_is_rebuilt_not_reused() {
        let conf = wgtun::parse(CONF).unwrap();
        let plan = up_plan(&want("race"), &conf).unwrap();
        let first_add = plan.iter().position(|s| s.argv.join(" ").starts_with("ip link add")).unwrap();
        let before: Vec<String> = plan[..first_add].iter().map(|s| s.argv.join(" ")).collect();
        assert_eq!(before, ["ip -4 rule del oif wg-race", "ip -6 rule del oif wg-race", "ip link del dev wg-race"]);
        assert!(plan[..first_add].iter().all(|s| s.on_fail == OnFail::Ignore));

        // "Cannot find device" on a first run is not a failure; a rule del
        // repeats until there is nothing left to delete, and no further.
        let mut fake = Fake::new(|l| {
            (l == "ip link del dev wg-race").then(|| "Cannot find device \"wg-race\"".into())
        });
        execute(&mut fake, &plan).expect("a first run comes up");
        let dels = fake.ran.iter().filter(|l| *l == "ip -4 rule del oif wg-race").count();
        assert!(dels > 1 && dels <= 32, "repeated, and bounded even when it never fails: {dels}");
    }

    /// A host that defaults IPv6 off on new devices keeps the v4 half, and
    /// says which half it lost and how to get it back.
    #[test]
    fn a_refused_v6_address_keeps_the_v4_tunnel_and_says_so() {
        let conf = wgtun::parse(CONF).unwrap();
        let plan = up_plan(&want("race"), &conf).unwrap();
        let mut fake = Fake::new(|l| l.starts_with("ip -6 address add").then(|| "Permission denied".into()));
        let out = execute(&mut fake, &plan).expect("v4 still comes up");
        assert!(out.degraded.as_deref().unwrap_or("").contains("disable_ipv6=0"), "{out:?}");
        assert!(!fake.ran.iter().any(|l| l.starts_with("ip -6 route") || l.starts_with("ip -6 rule add")), "{:#?}", fake.ran);
        assert!(fake.ran.iter().any(|l| l.starts_with("ip -4 rule add")));
    }

    #[test]
    fn a_failed_step_stops_the_plan_with_the_command_and_the_reason() {
        let conf = wgtun::parse(CONF).unwrap();
        let plan = up_plan(&want("race"), &conf).unwrap();
        let mut fake = Fake::new(|l| l.starts_with("ip link add").then(|| "Operation not supported".into()));
        let err = execute(&mut fake, &plan).unwrap_err();
        assert!(err.contains("ip link add dev wg-race type wireguard") && err.contains("not supported"), "{err}");
        assert!(!fake.ran.iter().any(|l| l.starts_with("wg setconf")), "nothing runs after the failure");
    }

    /// ⭐ Each refusal names its fix; none of them is silent.
    #[test]
    fn without_the_privilege_the_mode_is_refused_with_the_reason() {
        let all = |_: &str| true;
        let root = "Name:\thydranos\nCapEff:\t000001ffffffffff\n";
        let no_admin = "Name:\thydranos\nCapEff:\t00000000a80425fb\n";
        assert_eq!(support_from(true, Some(root), &all), Ok(()));
        assert_eq!(support_from(true, Some(no_admin), &all), Err(NEEDS_NET_ADMIN));
        assert_eq!(support_from(true, None, &all), Err(NEEDS_NET_ADMIN), "unknown is not yes");
        assert_eq!(support_from(false, Some(root), &all), Err(NEEDS_LINUX));
        assert_eq!(support_from(true, Some(root), &|p: &str| p != "wg"), Err(NEEDS_TOOLS));
        assert!(NEEDS_NET_ADMIN.contains("NET_ADMIN") && NEEDS_NET_ADMIN.contains("cap_add"));
        // Docker's default set: no NET_ADMIN (bit 12).
        assert_eq!(net_admin_in("CapEff:\t00000000a80425fb"), Some(false));
        assert_eq!(net_admin_in("CapEff:\t0000000000001000"), Some(true));
    }

    #[test]
    fn a_stored_file_is_private_validated_and_never_listed_with_its_keys() {
        let dir = std::env::temp_dir().join(format!("wgtunnel-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (name, _) = store_conf(&dir, "proton-ch.conf", CONF.as_bytes()).expect("stored");
        assert_eq!(name, "proton-ch.conf");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(&name)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let listed = serde_json::to_string(&list_confs(&dir)).unwrap();
        assert!(!listed.contains("aPrivateKeyValue"), "{listed}");
        assert!(listed.contains("10.2.0.2/32") && listed.contains("192.0.2.10:51820"), "{listed}");

        assert!(store_conf(&dir, "../escape.conf", CONF.as_bytes()).map(|(n, _)| n == "escape.conf").unwrap_or(true));
        assert!(store_conf(&dir, "x.txt", CONF.as_bytes()).is_err(), "not a .conf");
        assert!(store_conf(&dir, "bad.conf", b"[Interface]\nAddress = 10.0.0.2/32\n").is_err(), "no key");
        assert!(store_conf(&dir, "nopeer.conf", b"[Interface]\nPrivateKey = k\nAddress = 10.0.0.2/32\n").is_err());
        assert!(!dir.join("bad.conf").exists(), "a refused file is not written");

        assert_eq!(delete_conf(&dir, "proton-ch.conf"), Ok(true));
        assert_eq!(delete_conf(&dir, "proton-ch.conf"), Ok(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn engines(text: &str) -> Vec<crate::config::LocalEngine> {
        let cfg: crate::config::Config = toml::from_str(text).unwrap();
        cfg.local_engines()
    }

    /// Only in WireGuard mode, only for an engine whose tunnel is on, and
    /// the engine is then pinned to the device -- also when the tunnel will
    /// fail to come up, which is what keeps it from leaving directly.
    #[test]
    fn a_tunnelled_engine_is_pinned_to_its_device() {
        let text = "[network]\nmode = \"wireguard\"\n\
                    [race]\nlisten_port = 16171\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\nwireguard_provider = \"proton\"\n\
                    [hoard]\nlisten_port = 16172\nbind_interface = \"eth9\"\n";
        let e = engines(text);
        assert_eq!(e[0].session.bind_interface, "wg-race");
        assert_eq!(e[1].session.bind_interface, "eth9", "an engine without a tunnel keeps its own");
        let direct = engines(&text.replace("mode = \"wireguard\"", "mode = \"direct\""));
        assert_eq!(direct[0].session.bind_interface, "", "another mode brings no tunnel up");

        let manual = engines(&text.replace("wireguard_provider = \"proton\"", "wireguard_provider = \"airvpn\"\nwireguard_port = 40123"));
        assert_eq!(manual[0].session.listen_port, 40123, "the provider's port is the listen port");
    }

    /// An extra engine inherits its role's way out, but not its tunnel: two
    /// engines on one tunnel is one address and one port.
    #[test]
    fn an_extra_engine_does_not_inherit_its_roles_tunnel() {
        let e = engines(
            "[network]\nmode = \"wireguard\"\n\
             [race]\nlisten_port = 16171\nwireguard_enabled = true\nwireguard_config = \"a.conf\"\n\
             [hoard]\nlisten_port = 16172\n\
             [[engine]]\nengine_id = \"vpn1\"\nrole = \"race\"\n[engine.session]\nlisten_port = 26991\n",
        );
        let vpn1 = e.iter().find(|e| e.id == "vpn1").unwrap();
        assert!(!vpn1.session.wireguard_enabled);
        assert_eq!(vpn1.session.bind_interface, "");
        let w = wanted("wireguard", &e);
        assert_eq!(w.len(), 1);
        assert_ne!(w[0].table, 0);
    }

    #[test]
    fn the_port_is_forwarded_the_way_the_provider_does_unless_overridden() {
        assert_eq!(port_forward_of("proton", ""), PortForward::NatPmp);
        assert_eq!(port_forward_of("airvpn", ""), PortForward::Manual);
        assert_eq!(port_forward_of("mullvad", ""), PortForward::None);
        assert_eq!(port_forward_of("proton", "off"), PortForward::None);
        assert_eq!(port_forward_of("unknown", ""), PortForward::None);
        let conf = wgtun::parse(CONF).unwrap();
        assert_eq!(natpmp_gateway("proton", &conf), Some("10.2.0.1".parse().unwrap()));
        assert_eq!(natpmp_gateway("natpmp", &conf), Some("10.2.0.1".parse().unwrap()), "from DNS");
    }

    /// A device we did not make is never touched; one we made and no longer
    /// want is taken down.
    #[test]
    fn only_our_own_stale_devices_are_taken_down() {
        let dir = std::env::temp_dir().join(format!("wgtunnel-managed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        write_managed(&dir, &["wg-old".into()]);
        let mut fake = Fake::new(|_| None);
        let reg = Registry::default();
        reconcile(&mut fake, &dir, &[], &reg);
        assert!(fake.ran.contains(&"ip link del dev wg-old".to_string()), "{:#?}", fake.ran);
        assert!(!fake.ran.iter().any(|l| l.contains("wg-host")));
        assert!(read_managed(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Status text from `wg` is read field by field; no private key passes.
    #[test]
    fn the_live_status_reads_the_handshake_and_endpoint() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        struct Wg(u64);
        impl Runner for Wg {
            fn run(&mut self, argv: &[String], _: Option<&str>) -> Result<String, String> {
                match argv[3].as_str() {
                    "latest-handshakes" => Ok(format!("aPublicKeyValue=\t{}\n", self.0 - 30)),
                    "endpoints" => Ok("aPublicKeyValue=\t192.0.2.10:51820\n".into()),
                    _ => Err("no".into()),
                }
            }
        }
        let (age, ep) = live(&mut Wg(now), "wg-race").unwrap();
        assert!(age.unwrap() >= 30 && age.unwrap() < 40);
        assert_eq!(ep, "192.0.2.10:51820");
    }
}

/// The real thing, as root: two network namespaces, a local `wg` peer in the
/// second, the tunnel brought up by the plan in the first, a socket pinned to
/// the device reaching the peer THROUGH the tunnel, and the teardown leaving
/// the namespace's `ip route` and `ip rule` as they were.
///
/// Needs NET_ADMIN, `ip`, `wg` and the wireguard kernel module, so it is
/// ignored by default. Run it with:
///
/// ```text
/// docker run --rm --privileged -v <repo>:/build -w /build/typhon-engine rust:1-bookworm \
///   sh -c 'apt-get update -qq && apt-get install -y -qq iproute2 wireguard-tools iputils-ping >/dev/null && \
///          HYDRANOS_WG_ROOT_TEST=1 cargo test --bin hydranos wgtunnel::root -- --ignored --test-threads=1'
/// ```
#[cfg(all(test, target_os = "linux"))]
mod root {
    use super::*;

    fn sh(cmd: &str) -> String {
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).output().expect("sh");
        assert!(out.status.success(), "{cmd}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Runs every command inside a network namespace.
    struct InNs(&'static str);
    impl Runner for InNs {
        fn run(&mut self, argv: &[String], stdin: Option<&str>) -> Result<String, String> {
            let mut full: Vec<String> = vec!["ip".into(), "netns".into(), "exec".into(), self.0.into()];
            full.extend(argv.iter().cloned());
            System.run(&full, stdin)
        }
    }

    #[test]
    #[ignore = "needs NET_ADMIN and the wireguard module: see the module doc for the command"]
    fn wgtunnel_root_a_tunnel_comes_up_carries_a_pinned_socket_and_goes() {
        if std::env::var("HYDRANOS_WG_ROOT_TEST").as_deref() != Ok("1") {
            panic!("set HYDRANOS_WG_ROOT_TEST=1: this test creates network namespaces and interfaces");
        }
        let (a, b) = ("hy-wgt-a", "hy-wgt-b");
        let _ = sh(&format!("ip netns del {a} 2>/dev/null; ip netns del {b} 2>/dev/null; true"));
        sh(&format!("ip netns add {a} && ip netns add {b}"));
        // The "internet" between them: a veth pair, 192.0.2.0/24 (TEST-NET-1).
        sh(&format!(
            "ip link add hy-va netns {a} type veth peer name hy-vb netns {b} && \
             ip -n {a} addr add 192.0.2.1/24 dev hy-va && ip -n {a} link set hy-va up && ip -n {a} link set lo up && \
             ip -n {b} addr add 192.0.2.2/24 dev hy-vb && ip -n {b} link set hy-vb up && ip -n {b} link set lo up && \
             ip -n {a} route add default via 192.0.2.2"
        ));
        let ka = sh("wg genkey").trim().to_string();
        let kb = sh("wg genkey").trim().to_string();
        let pa = sh(&format!("echo {ka} | wg pubkey")).trim().to_string();
        let pb = sh(&format!("echo {kb} | wg pubkey")).trim().to_string();
        // The provider side, by hand: 10.66.0.1 behind its own wg device.
        sh(&format!(
            "ip -n {b} link add wgp type wireguard && \
             echo {kb} > /tmp/hy-wgt-kb && ip netns exec {b} wg set wgp private-key /tmp/hy-wgt-kb listen-port 51820 peer {pa} allowed-ips 10.66.0.2/32 && \
             ip -n {b} addr add 10.66.0.1/24 dev wgp && ip -n {b} link set wgp up && rm -f /tmp/hy-wgt-kb"
        ));
        let before_route = sh(&format!("ip -n {a} route; ip -n {a} -6 route"));
        let before_rule = sh(&format!("ip -n {a} rule; ip -n {a} -6 rule"));

        let conf = wgtun::parse(&format!(
            "[Interface]\nPrivateKey = {ka}\nAddress = 10.66.0.2/32\n\n[Peer]\nPublicKey = {pb}\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.2:51820\n"
        ))
        .unwrap();
        let w = Want {
            engine: "race".into(),
            device: device_name("race"),
            table: TABLE_BASE,
            pref: PREF_BASE,
            config_file: "t.conf".into(),
            provider: "generic".into(),
            manual_port: 0,
            port_forward: PortForward::None,
        };
        let plan = up_plan(&w, &conf).unwrap();
        execute(&mut InNs(a), &plan).expect("the tunnel comes up");
        // Twice: idempotent over a device that is already there.
        execute(&mut InNs(a), &plan).expect("and comes up again over itself");

        // The host's own routing did not move.
        assert_eq!(sh(&format!("ip -n {a} route; ip -n {a} -6 route")), before_route, "the main table changed");
        // A socket pinned to the device (ping -I is SO_BINDTODEVICE) reaches
        // the provider's 10.66.0.1 through the tunnel; an unpinned lookup does
        // not even consider the tunnel.
        sh(&format!("ip netns exec {a} ping -c 3 -W 2 -I wg-race 10.66.0.1"));
        let unpinned = sh(&format!("ip -n {a} route get 10.66.0.1"));
        assert!(!unpinned.contains("wg-race"), "an unpinned socket was routed into the tunnel: {unpinned}");
        let (age, endpoint) = live(&mut InNs(a), "wg-race").expect("the device exists");
        assert!(age.is_some(), "no handshake: the pinned traffic did not go through WireGuard");
        assert_eq!(endpoint, "192.0.2.2:51820");

        execute(&mut InNs(a), &teardown_plan("wg-race")).unwrap();
        assert_eq!(sh(&format!("ip -n {a} route; ip -n {a} -6 route")), before_route);
        assert_eq!(sh(&format!("ip -n {a} rule; ip -n {a} -6 rule")), before_rule, "a rule was left behind");
        assert!(live(&mut InNs(a), "wg-race").is_none(), "the device is gone");
        let _ = sh(&format!("ip netns del {a}; ip netns del {b}"));
    }
}
