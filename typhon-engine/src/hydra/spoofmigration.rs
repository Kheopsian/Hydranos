//! One-time migration for the release that removed client spoofing.
//!
//! Until this release a per-tracker override could make Hydranos present
//! itself as another client. Removing it means that on the first start after
//! an upgrade, every torrent that was announcing under a borrowed identity
//! starts announcing under its real one -- to trackers that, in some cases,
//! only accepted it because of the disguise.
//!
//! Letting that happen silently would be doing to our users exactly what the
//! spoof did to tracker operators: changing what they are told without telling
//! them. So the torrents concerned are **paused**, once, and the operator
//! decides what to do with them.
//!
//! Only the torrents concerned. A blanket pause would stop the catalogue of
//! everyone who never configured an override, and on a private tracker a mass
//! pause costs seeding time and can trip hit-and-run rules -- getting someone
//! banned in order to protect them from being banned.
//!
//! The migration is driven by the `[announce_clients]` tables still present in
//! `config.toml`. The configuration type no longer has that field, so the
//! tables are read from the raw text; removing them afterwards is what makes
//! this run exactly once.

use std::collections::BTreeSet;

use crate::announce::overrides::{longest_override_key, override_host};

/// The tracker hosts that carried a client override.
///
/// Read from the text rather than the parsed `Config`, because the field that
/// held them has been deleted -- serde now skips the tables in silence, which
/// is what lets an old file keep loading.
pub fn legacy_spoofed_hosts(config_text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut in_flat_table = false;

    for line in config_text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_flat_table = false;
            // `[announce_clients."host"]`, the shape serde writes.
            if let Some(rest) = line.strip_prefix("[announce_clients.") {
                let key = rest.trim_end_matches(']').trim().trim_matches('"');
                if !key.is_empty() {
                    out.insert(key.to_string());
                }
            } else if line == "[announce_clients]" {
                // A hand-written file may put the hosts as keys instead.
                in_flat_table = true;
            }
            continue;
        }
        if in_flat_table && !line.is_empty() && !line.starts_with('#') {
            if let Some((key, _)) = line.split_once('=') {
                let key = key.trim().trim_matches('"');
                if !key.is_empty() {
                    out.insert(key.to_string());
                }
            }
        }
    }
    out
}

/// The same configuration with every `[announce_clients]` table removed.
///
/// Writing this back is the marker that the migration has run: no tables, no
/// hosts, nothing to pause.
pub fn without_legacy_tables(config_text: &str) -> String {
    let mut out = String::with_capacity(config_text.len());
    let mut skipping = false;

    for line in config_text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            skipping = t == "[announce_clients]" || t.starts_with("[announce_clients.");
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Whether this torrent was announcing under a borrowed identity.
///
/// The FIRST tracker decides, because that is the rule the spoof itself used:
/// several private trackers share one swarm, so a torrent could not present a
/// different client to each and the first one won.
pub fn is_affected(trackers: &[Vec<String>], hosts: &BTreeSet<String>) -> bool {
    if hosts.is_empty() {
        return false;
    }
    let Some(first) = trackers.iter().flatten().next() else {
        return false;
    };
    let host = override_host(first);
    longest_override_key(&host, hosts.iter().map(|s| s.as_str())).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[daemon]
api_port = 8199

[announce_clients."t.myanonamouse.net"]
peer_id_prefix = "-qB5220-"
user_agent = "qBittorrent/5.2.2"

[announce_clients."home.opsfet.ch"]
peer_id_prefix = "-qB5220-"
user_agent = "qBittorrent/5.2.2"

[announce_ip_modes]
"gemini-tracker.org" = "v4"
"#;

    fn hosts(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_overridden_hosts_are_read_from_the_raw_text() {
        let got = legacy_spoofed_hosts(CFG);
        assert_eq!(got, hosts(&["home.opsfet.ch", "t.myanonamouse.net"]));
    }

    /// A hand-written file may spell the table flat rather than nested.
    #[test]
    fn a_flat_table_is_understood_too() {
        let cfg = "[announce_clients]\n\"tracker.example\" = { peer_id_prefix = \"-qB5220-\" }\n";
        assert_eq!(legacy_spoofed_hosts(cfg), hosts(&["tracker.example"]));
    }

    /// A configuration that never carried an override migrates nothing, which
    /// is the case that must not pause anybody.
    #[test]
    fn a_config_without_overrides_yields_no_hosts() {
        assert!(legacy_spoofed_hosts("[daemon]\napi_port = 8199\n").is_empty());
    }

    /// Removing the tables is what makes this run once. Everything else in the
    /// file has to survive it untouched.
    #[test]
    fn the_tables_are_removed_and_nothing_else_is() {
        let out = without_legacy_tables(CFG);
        assert!(!out.contains("announce_clients"), "{out}");
        assert!(out.contains("[daemon]"));
        assert!(out.contains("api_port = 8199"));
        assert!(out.contains("[announce_ip_modes]"));
        assert!(out.contains("\"gemini-tracker.org\" = \"v4\""));
        // And a second pass finds nothing left to do.
        assert!(legacy_spoofed_hosts(&out).is_empty());
    }

    #[test]
    fn a_torrent_on_an_overridden_tracker_is_affected() {
        let h = hosts(&["t.myanonamouse.net"]);
        let trackers = vec![vec!["https://t.myanonamouse.net/announce/KEY".to_string()]];
        assert!(is_affected(&trackers, &h));
    }

    /// The first tracker decides, as the spoof itself did: a torrent whose
    /// first tracker was never overridden announced under its real identity
    /// already, so nothing changes for it.
    #[test]
    fn the_first_tracker_decides() {
        let h = hosts(&["t.myanonamouse.net"]);
        let affected = vec![vec![
            "https://t.myanonamouse.net/announce/KEY".to_string(),
            "https://other.example/announce".to_string(),
        ]];
        let untouched = vec![vec![
            "https://other.example/announce".to_string(),
            "https://t.myanonamouse.net/announce/KEY".to_string(),
        ]];
        assert!(is_affected(&affected, &h));
        assert!(!is_affected(&untouched, &h), "its identity was never borrowed");
    }

    /// The case that matters most: someone who never spoofed anything keeps
    /// every torrent running.
    #[test]
    fn no_overrides_pauses_nothing() {
        let trackers = vec![vec!["https://tracker.example/announce".to_string()]];
        assert!(!is_affected(&trackers, &BTreeSet::new()));
    }

    #[test]
    fn a_torrent_with_no_tracker_is_not_affected() {
        assert!(!is_affected(&[], &hosts(&["t.myanonamouse.net"])));
    }
}
