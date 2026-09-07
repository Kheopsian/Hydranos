//! Announcing to trackers.
//!
//! In 3.x this was the Go control plane's job -- the Rust engine's
//! `start_announce_loop` is the dial queue consumer, not this. Hydra 4 has no
//! control plane to defer to, so it lives here.
//!
//! Nothing else in the process tells a tracker we exist. When this is not
//! running, the engines seed, listen and connect, and every tracker forgets
//! about all of it within an announce interval.

pub mod breaker;
pub mod cache;
pub mod overrides;
pub mod policy;
pub mod runner;
pub mod scheduler;
pub mod url;

use crate::config::Config;
use policy::{ClientSpoof, Policy};

/// Build the announce policy from the operator's config.
///
/// Reads the same three tables 3.x reads, under the same keys: an operator
/// upgrading does not re-declare anything.
pub fn policy_from_config(config: &Config, peer_id: String, public_ip: String) -> Policy {
    Policy {
        passkeys: config.announce_passkeys.clone(),
        clients: config
            .announce_clients
            .iter()
            .map(|(host, c)| {
                (
                    host.clone(),
                    ClientSpoof {
                        peer_id_prefix: c.peer_id_prefix.clone(),
                        user_agent: c.user_agent.clone(),
                    },
                )
            })
            .collect(),
        secondary_stats: config.announce_secondary_stats.clone(),
        peer_id,
        // The User-Agent carries the version, as 3.x did: a tracker operator
        // asking "which client is this" gets an answer.
        user_agent: format!("Hydra/{}", crate::api::HYDRA_VERSION),
        public_ip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_read_from_the_same_config_keys_as_3x() {
        let mut config = Config::default();
        config.announce_passkeys.insert("tr4ker.net".into(), "KEY".into());
        config.announce_clients.insert(
            "t.myanonamouse.net".into(),
            crate::config::AnnounceClient {
                peer_id_prefix: "-qB5220-".into(),
                user_agent: "qBittorrent/5.2.2".into(),
            },
        );
        let p = policy_from_config(&config, "-TY0001-abcdefghijkl".into(), String::new());
        assert_eq!(p.passkeys.get("tr4ker.net").map(String::as_str), Some("KEY"));
        assert_eq!(
            p.clients.get("t.myanonamouse.net").map(|c| c.peer_id_prefix.as_str()),
            Some("-qB5220-")
        );
        assert!(p.user_agent.starts_with("Hydra/"));
    }
}
