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
use policy::Policy;
use std::sync::{Arc, RwLock};

/// The live announce policy for one engine.
///
/// `RwLock<Arc<Policy>>` rather than `RwLock<Policy>`: the announce closure
/// clones what it finds, and cloning an Arc is a refcount bump while cloning a
/// Policy copies four BTreeMaps. The lock is therefore held for the length of
/// a pointer copy, never for the length of an announce.
pub type PolicyHandle = Arc<RwLock<Arc<Policy>>>;

/// Rebuild every engine's policy from the config now on disk.
///
/// The three per-engine fields -- `peer_id`, `user_agent`, `public_ip` -- are
/// carried over from the policy being replaced. They are derived from the
/// engine's own binding, not from these tables, and rebuilding them from the
/// config would hand every engine the same identity.
pub fn refresh_policies(config: &Config, engines: &[crate::engines::Engine]) -> usize {
    let mut done = 0;
    for engine in engines {
        let Some(handle) = engine.announce_policy.get() else {
            continue; // offline engine: no runner, nothing to refresh
        };
        let Ok(mut slot) = handle.write() else { continue };
        let old = slot.clone();
        let mut next = policy_from_config(
            config,
            old.peer_id.clone(),
            old.public_ip.clone(),
        );
        next.user_agent = old.user_agent.clone();
        *slot = Arc::new(next);
        done += 1;
    }
    done
}

/// Build the announce policy from the operator's config.
///
/// Reads the same three tables 3.x reads, under the same keys: an operator
/// upgrading does not re-declare anything.
pub fn policy_from_config(config: &Config, peer_id: String, public_ip: String) -> Policy {
    Policy {
        passkeys: config.announce_passkeys.clone(),
        ip_modes: config.announce_ip_modes.clone(),
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

    /// The bug this replaced: `policy_from_config` was called once at startup
    /// and the result moved into the runner, so an override written later was
    /// in the file, in the UI, and nowhere near the announce.
    #[test]
    fn a_swapped_policy_is_seen_by_whoever_reads_the_handle() {
        let mut config = Config::default();
        let handle: PolicyHandle = Arc::new(RwLock::new(Arc::new(policy_from_config(
            &config,
            "-HY4R00-aaaaaaaaaaaa".into(),
            "203.0.113.9".into(),
        ))));

        // A reader that captured the Arc once -- the runner's old behaviour.
        let captured = handle.read().unwrap().clone();
        assert!(captured.passkeys.is_empty());

        config.announce_passkeys.insert("tracker.example".into(), "KEY".into());
        {
            let mut slot = handle.write().unwrap();
            let old = slot.clone();
            let mut next = policy_from_config(&config, old.peer_id.clone(), old.public_ip.clone());
            next.user_agent = old.user_agent.clone();
            *slot = Arc::new(next);
        }

        // Whoever captured early still sees nothing: that IS the old bug.
        assert!(captured.passkeys.is_empty(), "a captured Arc must not change under us");

        // Whoever re-reads sees the override. That is the fix.
        let fresh = handle.read().unwrap().clone();
        assert_eq!(
            fresh.passkeys.get("tracker.example").map(String::as_str),
            Some("KEY")
        );
    }

    /// peer_id, user_agent and public_ip come from the engine's own binding,
    /// not from these tables. Rebuilding them from the config would hand every
    /// engine the same identity.
    #[test]
    fn a_refresh_keeps_the_per_engine_identity() {
        let config = Config::default();
        let handle: PolicyHandle = Arc::new(RwLock::new(Arc::new(Policy {
            peer_id: "-HY4R00-unique-one".into(),
            user_agent: "Hydra/9.9.9".into(),
            public_ip: "198.51.100.4".into(),
            ..policy_from_config(&Config::default(), String::new(), String::new())
        })));

        {
            let mut slot = handle.write().unwrap();
            let old = slot.clone();
            let mut next = policy_from_config(&config, old.peer_id.clone(), old.public_ip.clone());
            next.user_agent = old.user_agent.clone();
            *slot = Arc::new(next);
        }

        let p = handle.read().unwrap().clone();
        assert_eq!(p.peer_id, "-HY4R00-unique-one");
        assert_eq!(p.public_ip, "198.51.100.4");
        assert_eq!(p.user_agent, "Hydra/9.9.9");
    }

    #[test]
    fn the_policy_is_read_from_the_same_config_keys_as_3x() {
        let mut config = Config::default();
        config.announce_passkeys.insert("tr4ker.net".into(), "KEY".into());
        let p = policy_from_config(&config, "-TY0001-abcdefghijkl".into(), String::new());
        assert_eq!(p.passkeys.get("tr4ker.net").map(String::as_str), Some("KEY"));
        assert!(p.user_agent.starts_with("Hydra/"));
    }
}
