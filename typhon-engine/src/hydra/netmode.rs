//! The Network tab's model: which mode is chosen, which keys each mode owns,
//! and what a save actually changes.
//!
//! Apart from the handlers so each rule can be tested as a function. Every one
//! of them was wrong in the same quiet way before 4.4: the mode was guessed
//! from whatever keys were lying around (and could never come out as
//! WireGuard), a save kept the keys of the mode it was leaving, and the answer
//! always said "restart" while the page said it did not need one.

use crate::config::{Config, Session};

/// The modes the tab offers, in its order.
pub const MODES: [&str; 5] = ["direct", "wireguard", "gluetun", "socks5", "proxy_v2"];

/// The SOCKS5 keys: the peer proxy and what announces fall back to it.
/// `announce_proxy` and `announce_ip` belong here because the tab shows them
/// with the proxy and nowhere else: kept after leaving the mode, they would go
/// on steering announces from a page that no longer displays them.
pub const SOCKS5_KEYS: [&str; 6] = [
    "socks5_outbound_host",
    "socks5_outbound_port",
    "socks5_outbound_user",
    "socks5_outbound_pass",
    "announce_proxy",
    "announce_ip",
];
pub const PROXY_V2_KEYS: [&str; 3] = ["listen_port_proxy_v2", "listen_addr_proxy_v2", "proxy_v2_trusted_sources"];
pub const GLUETUN_KEYS: [&str; 3] = ["gluetun_port_forward", "gluetun_url", "gluetun_api_key"];

/// The mode as saved, or as deduced for a file the tab never saved.
///
/// The deduction is the pre-4.4 one, kept for exactly that case: it cannot
/// name WireGuard, which leaves nothing in [race]/[hoard] to find.
pub fn current(cfg: &Config) -> &str {
    let saved = cfg.network.mode.trim();
    if MODES.contains(&saved) {
        return saved;
    }
    deduce(cfg)
}

fn deduce(cfg: &Config) -> &'static str {
    let (race, hoard) = (&cfg.race, &cfg.hoard);
    if hoard.gluetun_port_forward || race.gluetun_port_forward {
        "gluetun"
    } else if race.listen_port_proxy_v2 != 0 || hoard.listen_port_proxy_v2 != 0 {
        "proxy_v2"
    } else if !race.socks5_outbound_host.trim().is_empty() || !hoard.socks5_outbound_host.trim().is_empty() {
        "socks5"
    } else {
        "direct"
    }
}

/// The keys a save in `mode` REMOVES from every engine section: those of the
/// modes it is not. Removed rather than blanked, so an extra engine goes back
/// to inheriting from its role instead of holding an empty override.
pub fn cleared_keys(mode: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if mode != "socks5" && mode != "proxy_v2" {
        out.extend(SOCKS5_KEYS);
    }
    if mode != "proxy_v2" {
        out.extend(PROXY_V2_KEYS);
    }
    if mode != "gluetun" {
        out.extend(GLUETUN_KEYS);
    }
    out
}

/// Everything about a session that an engine reads once, when it joins the
/// network, and that the tab can change. Two equal fingerprints need no
/// restart; anything else does, because none of these is applied live: the
/// listener, the proxy in each binding's egress, the announce policy's proxy
/// and `ip=`, the PROXY v2 listener and the gluetun follower are all set up
/// in `engines::connect`.
fn fingerprint(s: &Session) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{:?}|{}|{}|{}",
        s.listen_port,
        s.bind_interface.trim(),
        s.enable_ipv6,
        s.socks5_outbound_host.trim(),
        s.socks5_outbound_port,
        s.socks5_outbound_user,
        s.socks5_outbound_pass,
        s.announce_proxy.trim(),
        s.announce_ip.trim(),
        s.listen_port_proxy_v2,
        s.listen_addr_proxy_v2.trim(),
        s.proxy_v2_trusted_sources,
        s.gluetun_port_forward,
        s.gluetun_url.trim(),
        s.gluetun_api_key,
    )
}

/// Whether the config now on disk asks the RUNNING engines for anything
/// they do not already have.
///
/// Against what each engine was started with, not against the previous
/// file: two saves in a row, the second undoing the first, need no restart,
/// and a save after an unapplied one still does.
pub fn restart_required(cfg: &Config, running: &[(String, Session)]) -> bool {
    let wanted = cfg.local_engines();
    if wanted.len() != running.len() {
        return true;
    }
    wanted.iter().any(|e| match running.iter().find(|(id, _)| *id == e.id) {
        Some((_, s)) => fingerprint(s) != fingerprint(&e.session),
        None => true,
    })
}

/// The warning for an engine whose announces go through a proxy while it
/// would announce to UDP trackers. Same words as the error those trackers
/// then carry on the Trackers tab, so the two are recognisably one fact.
pub const UDP_BEHIND_PROXY: &str = typhon_engine::tracker::udp::UDP_PROXIED_REFUSAL;

/// What the operator should know about the config now on disk, said when
/// they save it rather than discovered later on the Trackers tab.
///
/// `has_udp` answers "does this engine hold a torrent with a udp:// tracker".
pub fn warnings(cfg: &Config, has_udp: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    let udp_behind_proxy = cfg.local_engines().iter().any(|e| {
        typhon_engine::tracker::http::effective_proxy(&crate::engines::session_http_proxy(&e.session)).is_some()
            && (e.session.udp_trackers() || has_udp(&e.id))
    });
    if udp_behind_proxy {
        out.push(UDP_BEHIND_PROXY.to_string());
    }
    let mode = current(cfg);
    if mode == "proxy_v2" && cfg.race.socks5_outbound_host.trim().is_empty() {
        out.push("No SOCKS5 proxy is set: incoming peers come through the relay, but outgoing connections and announces leave directly.".to_string());
    }
    if mode != "socks5" && mode != "proxy_v2" && typhon_engine::tracker::http::env_announce_proxy().is_some() {
        out.push("TYPHON_ANNOUNCE_PROXY is set in the environment: announces and webseed fetches go through it, whatever mode this page shows.".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(text: &str) -> Config {
        toml::from_str(text).expect("config parses")
    }

    fn running(c: &Config) -> Vec<(String, Session)> {
        c.local_engines().into_iter().map(|e| (e.id, e.session)).collect()
    }

    /// A saved mode is read back as saved, WireGuard included, which the
    /// deduction could never produce.
    #[test]
    fn the_saved_mode_wins_and_an_old_file_is_still_deduced() {
        assert_eq!(current(&cfg("[network]\nmode = \"wireguard\"\n")), "wireguard");
        assert_eq!(current(&cfg("[race]\nsocks5_outbound_host = \"10.0.0.1\"\n")), "socks5");
        assert_eq!(current(&cfg("[race]\nlisten_port_proxy_v2 = 16271\n")), "proxy_v2");
        assert_eq!(current(&cfg("[hoard]\ngluetun_port_forward = true\n")), "gluetun");
        assert_eq!(current(&cfg("")), "direct");
        assert_eq!(current(&cfg("[network]\nmode = \"nonsense\"\n")), "direct", "unknown = deduced");
    }

    #[test]
    fn each_mode_clears_what_it_does_not_own() {
        let direct = cleared_keys("direct");
        assert!(SOCKS5_KEYS.iter().chain(&PROXY_V2_KEYS).chain(&GLUETUN_KEYS).all(|k| direct.contains(k)));
        let socks = cleared_keys("socks5");
        assert!(!socks.contains(&"socks5_outbound_host") && socks.contains(&"listen_port_proxy_v2"));
        let pv2 = cleared_keys("proxy_v2");
        assert!(!pv2.contains(&"socks5_outbound_host") && !pv2.contains(&"listen_port_proxy_v2"));
        assert!(pv2.contains(&"gluetun_url"));
        let gl = cleared_keys("gluetun");
        assert!(!gl.contains(&"gluetun_url") && gl.contains(&"socks5_outbound_host"));
    }

    /// ⭐ Exact, both ways: an unchanged network needs no restart, any
    /// boot-time key changed does, and undoing an unapplied change is free.
    #[test]
    fn a_restart_is_asked_for_exactly_when_the_engines_would_change() {
        let before = cfg("[race]\nlisten_port = 16171\n[hoard]\nlisten_port = 16172\n");
        let run = running(&before);
        assert!(!restart_required(&before, &run), "same config, nothing to do");
        let mode_only = cfg("[network]\nmode = \"direct\"\n[race]\nlisten_port = 16171\n[hoard]\nlisten_port = 16172\n");
        assert!(!restart_required(&mode_only, &run), "recording the mode changes no engine");
        let socks = cfg("[race]\nlisten_port = 16171\nsocks5_outbound_host = \"10.0.0.1\"\n[hoard]\nlisten_port = 16172\n");
        assert!(restart_required(&socks, &run), "a proxy is set up at boot");
        let port = cfg("[race]\nlisten_port = 16999\n[hoard]\nlisten_port = 16172\n");
        assert!(restart_required(&port, &run));
    }

    #[test]
    fn udp_trackers_behind_a_proxy_are_warned_about_when_saved() {
        let none = |_: &str| false;
        let proxied = cfg("[race]\nsocks5_outbound_host = \"10.0.0.1\"\n");
        assert!(warnings(&proxied, &none).iter().any(|w| w == UDP_BEHIND_PROXY), "UDP is on by default");
        let udp_off = cfg("[race]\nsocks5_outbound_host = \"10.0.0.1\"\nenable_udp_trackers = false\n\
                           [hoard]\nenable_udp_trackers = false\n");
        assert!(!warnings(&udp_off, &none).iter().any(|w| w == UDP_BEHIND_PROXY), "nothing UDP to lose");
        let some = |id: &str| id == "race";
        assert!(warnings(&udp_off, &some).iter().any(|w| w == UDP_BEHIND_PROXY), "but torrents that list one");
        assert!(!warnings(&cfg(""), &some).iter().any(|w| w == UDP_BEHIND_PROXY), "no proxy, no warning");
    }
}
