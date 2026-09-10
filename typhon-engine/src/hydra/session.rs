//! Cookie sessions for the qBittorrent shim.
//!
//! Hydra authenticates with an API key, which is the right shape for a caller
//! that can set a header. The *arr stack cannot: Sonarr, Radarr, autobrr and
//! cross-seed all speak to a "qBittorrent" through a form with a host, a port,
//! a username and a password, and nowhere to put a key. They log in and then
//! ride a cookie.
//!
//! Until this module existed `/api/v2/auth/login` answered `Ok.` to anything,
//! set no cookie, and every call after it was refused -- so the clients had no
//! working path at all once the instance stopped accepting the placeholder key
//! for free. That is the hole this closes: the same admin account the WebUI
//! logs in with, exchanged for a session cookie that `authorised` accepts.
//!
//! Design notes worth keeping:
//!
//!  * **Sessions live in memory only.** A restart logs every client out, and
//!    every client logs straight back in -- that is what qBittorrent does too,
//!    and it keeps a stolen cookie from outliving the process.
//!  * **The token is compared in constant time.** A session id is a bearer
//!    credential exactly like the key is.
//!  * **Sweeping is done on use, not on a timer.** The table holds a handful
//!    of entries, so a scan while inserting costs nothing and saves a task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a session stays valid without being used.
///
/// qBittorrent defaults to an hour. The *arr stack polls on a timer far
/// shorter than that, so in practice a client's session never expires while it
/// is running, and a client that stops loses it.
const TTL: Duration = Duration::from_secs(3600);

/// Cap on live sessions.
///
/// A client that logs in on every request instead of keeping its cookie would
/// otherwise grow this table without bound. Reaching the cap drops the oldest.
const MAX_SESSIONS: usize = 256;

/// The name qBittorrent gives its session cookie. Clients look for this exact
/// name in `Set-Cookie`; anything else and they carry nothing back.
pub const COOKIE_NAME: &str = "SID";

#[derive(Clone, Default)]
pub struct Sessions {
    inner: Arc<Mutex<HashMap<String, Instant>>>,
}

impl Sessions {
    /// Mint a session and return its id.
    pub fn create(&self) -> String {
        let sid = fresh_sid();
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, seen| now.duration_since(*seen) < TTL);
        if map.len() >= MAX_SESSIONS {
            // Oldest first: the one that has gone longest without a request is
            // the one least likely to be a client still working.
            if let Some(oldest) = map.iter().min_by_key(|(_, seen)| **seen).map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(sid.clone(), now);
        sid
    }

    /// Is this session id live? Touches it, so an active client stays in.
    pub fn validate(&self, sid: &str) -> bool {
        if sid.is_empty() {
            return false;
        }
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, seen| now.duration_since(*seen) < TTL);
        // Constant time against every live id, so a caller cannot learn a
        // prefix from how long the answer took.
        let mut found: Option<String> = None;
        for key in map.keys() {
            if crate::api::constant_time_eq(key.as_bytes(), sid.as_bytes()) {
                found = Some(key.clone());
            }
        }
        match found {
            Some(key) => {
                map.insert(key, now);
                true
            }
            None => false,
        }
    }

    /// Drop a session. Logging out must actually log out.
    pub fn revoke(&self, sid: &str) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(sid);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 32 bytes of system entropy, hex encoded -- the same source as the API key.
fn fresh_sid() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Pull one cookie's value out of a `Cookie:` header.
///
/// Hand rolled because the header is a handful of pairs and pulling in a
/// cookie crate to split on `;` would be the larger change.
pub fn cookie_value(header: &str, name: &str) -> Option<String> {
    for pair in header.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=') {
            if key.trim() == name {
                return Some(value.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_session_validates_and_a_made_up_one_does_not() {
        let s = Sessions::default();
        let sid = s.create();
        assert!(s.validate(&sid));
        assert!(!s.validate("deadbeef"));
        assert!(!s.validate(""));
    }

    #[test]
    fn revoking_ends_the_session() {
        let s = Sessions::default();
        let sid = s.create();
        s.revoke(&sid);
        assert!(!s.validate(&sid));
    }

    #[test]
    fn two_sessions_are_never_the_same() {
        let s = Sessions::default();
        assert_ne!(s.create(), s.create());
    }

    #[test]
    fn the_table_is_capped() {
        let s = Sessions::default();
        for _ in 0..(MAX_SESSIONS + 20) {
            s.create();
        }
        assert!(s.len() <= MAX_SESSIONS);
    }

    #[test]
    fn cookies_are_split_on_semicolons() {
        assert_eq!(cookie_value("SID=abc", "SID").as_deref(), Some("abc"));
        assert_eq!(cookie_value("a=1; SID=abc; b=2", "SID").as_deref(), Some("abc"));
        assert_eq!(cookie_value("a=1; SID=\"abc\"", "SID").as_deref(), Some("abc"));
        assert_eq!(cookie_value("a=1; b=2", "SID"), None);
        // A cookie whose name merely ends in SID is a different cookie.
        assert_eq!(cookie_value("XSID=abc", "SID"), None);
    }
}
