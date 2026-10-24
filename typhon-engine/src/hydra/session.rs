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
    /// Failed logins per caller, for both login routes. See [`Throttle`].
    pub throttle: Throttle,
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

    /// Drop every session: the password they were opened with is gone.
    pub fn revoke_all(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// A name a stranger typed, made safe to put in the log: escaped, so a line
/// break in it cannot start a line of its own, and cut, so it cannot make one
/// line as long as the request.
pub(crate) fn loggable(name: &str) -> String {
    const MAX: usize = 64;
    let mut out: String = name.chars().take(MAX).flat_map(char::escape_debug).collect();
    if name.chars().nth(MAX).is_some() {
        out.push_str("...");
    }
    out
}

/// Failed logins in a row, within [`FAIL_WINDOW`], before a caller is blocked.
const MAX_FAILS: u32 = 5;
const FAIL_WINDOW: Duration = Duration::from_secs(15 * 60);
/// The first block; each one after it doubles, up to [`MAX_BLOCK`].
const FIRST_BLOCK: Duration = Duration::from_secs(60);
const MAX_BLOCK: Duration = Duration::from_secs(3600);
/// Callers tracked at once; the one idle longest is forgotten first, a
/// caller still blocked only when every one is.
const MAX_TRACKED: usize = 4096;

/// Who a failed login is counted against: the socket peer, never a header,
/// for the reason `bootstrap::is_local_request` gives. An IPv6 caller counts
/// by its /64, since one host can draw a new address from it for every try.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Caller(std::net::IpAddr);

impl Caller {
    pub fn of(peer: std::net::IpAddr) -> Self {
        match peer.to_canonical() {
            std::net::IpAddr::V6(a) => {
                let s = a.segments();
                Caller(std::net::IpAddr::V6(std::net::Ipv6Addr::new(
                    s[0], s[1], s[2], s[3], 0, 0, 0, 0,
                )))
            }
            v4 => Caller(v4),
        }
    }
}

#[derive(Clone, Copy)]
struct Tally {
    fails: u32,
    window_start: Instant,
    blocks: u32,
    blocked_until: Option<Instant>,
    last_seen: Instant,
}

/// Failed-login limiter, in memory: a restart lifts every block.
///
/// Behind a reverse proxy every client shares the proxy's address, so a
/// stranger who fails through it blocks the owner's own login too. That is
/// accepted: reading the client from a forwarding header would let the
/// stranger pick a fresh one per try.
#[derive(Clone, Default)]
pub struct Throttle {
    inner: Arc<Mutex<HashMap<Caller, Tally>>>,
}

impl Throttle {
    /// How long this caller is still blocked, if it is.
    pub fn blocked_for(&self, who: Caller) -> Option<Duration> {
        self.blocked_for_at(who, Instant::now())
    }

    fn blocked_for_at(&self, who: Caller, now: Instant) -> Option<Duration> {
        let map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&who)?
            .blocked_until
            .and_then(|until| until.checked_duration_since(now))
            .filter(|left| !left.is_zero())
    }

    pub fn failed(&self, who: Caller) {
        self.failed_at(who, Instant::now())
    }

    fn failed_at(&self, who: Caller, now: Instant) {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !map.contains_key(&who) && map.len() >= MAX_TRACKED {
            // A caller still blocked goes last: otherwise enough fresh
            // addresses would flush a block before it ran out.
            let still_blocked = |t: &Tally| t.blocked_until.is_some_and(|u| u > now);
            if let Some(idle) = map
                .iter()
                .min_by_key(|(_, t)| (still_blocked(t), t.last_seen))
                .map(|(k, _)| *k)
            {
                map.remove(&idle);
            }
        }
        let t = map.entry(who).or_insert(Tally {
            fails: 0,
            window_start: now,
            blocks: 0,
            blocked_until: None,
            last_seen: now,
        });
        // An hour of quiet and the caller starts over, blocks included.
        // Counted from the end of its last block, not the fail that set it:
        // sitting out an hour-long block is not an hour of good behaviour.
        let quiet_since = t.blocked_until.map_or(t.last_seen, |u| u.max(t.last_seen));
        if now.saturating_duration_since(quiet_since) >= MAX_BLOCK {
            t.blocks = 0;
            t.fails = 0;
            t.window_start = now;
        }
        if now.duration_since(t.window_start) >= FAIL_WINDOW {
            t.fails = 0;
            t.window_start = now;
        }
        t.last_seen = now;
        t.fails += 1;
        if t.fails >= MAX_FAILS {
            let doubled = FIRST_BLOCK.saturating_mul(1u32 << t.blocks.min(16));
            t.blocked_until = Some(now + doubled.min(MAX_BLOCK));
            t.blocks += 1;
            t.fails = 0;
            t.window_start = now;
        }
    }

    /// A good password: the caller's slate is clean.
    pub fn succeeded(&self, who: Caller) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).remove(&who);
    }
}

/// How long a login waits for a free check before giving up as busy.
const VERIFY_WAIT: Duration = Duration::from_secs(2);

/// Every check slot stayed taken for [`VERIFY_WAIT`].
#[derive(Debug, PartialEq, Eq)]
pub struct Busy;

/// How many password checks may run at once, across both login routes.
///
/// A bcrypt check is a quarter of a second of CPU by design. Two at a time is
/// plenty for the one admin account there is, and a burst of logins then
/// queues as idle futures instead of taking every core.
const VERIFY_SLOTS: usize = 2;

/// Check a password against the admin account's bcrypt hash, off the async
/// runtime.
///
/// Run inline in a handler, the check held its worker thread for its whole
/// cost, and everything else scheduled there -- API, SSE, background tasks --
/// waited behind it. The slot is moved into the blocking task, so a client
/// that hangs up mid-check does not free it before the CPU is.
///
/// The wait for a slot is bounded: a flood of logins from many addresses,
/// which no per-caller limit sees, would otherwise queue the owner's own
/// login behind all of them.
pub async fn verify_password(password: &str, hash: &str) -> Result<bool, Busy> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    if hash.is_empty() {
        return Ok(false);
    }
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(VERIFY_SLOTS)));
    let slot = match tokio::time::timeout(VERIFY_WAIT, slots.clone().acquire_owned()).await {
        Ok(Ok(slot)) => slot,
        Ok(Err(_)) => return Ok(false),
        Err(_) => return Err(Busy),
    };
    let (password, hash) = (password.to_owned(), hash.to_owned());
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        bcrypt::verify(password, &hash).unwrap_or(false)
    })
    .await
    .map_err(|_| Busy)
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

    fn caller(ip: &str) -> Caller {
        Caller::of(ip.parse().unwrap())
    }

    #[test]
    fn five_fails_block_a_caller_and_each_block_doubles_up_to_an_hour() {
        let t = Throttle::default();
        let who = caller("203.0.113.9");
        let mut now = Instant::now();
        for _ in 0..4 {
            t.failed_at(who, now);
        }
        assert_eq!(t.blocked_for_at(who, now), None, "four fails are not a block");
        t.failed_at(who, now);
        assert_eq!(t.blocked_for_at(who, now), Some(FIRST_BLOCK));
        assert_eq!(t.blocked_for_at(caller("203.0.113.10"), now), None, "only that caller");
        let mut expect = FIRST_BLOCK;
        for _ in 0..8 {
            now += expect;
            assert_eq!(t.blocked_for_at(who, now), None, "the block ends");
            for _ in 0..5 {
                t.failed_at(who, now);
            }
            expect = (expect * 2).min(MAX_BLOCK);
            assert_eq!(t.blocked_for_at(who, now), Some(expect));
        }
        assert_eq!(expect, MAX_BLOCK);
        // An hour of quiet after the block ends, and the caller starts over.
        now += MAX_BLOCK * 2;
        for _ in 0..5 {
            t.failed_at(who, now);
        }
        assert_eq!(t.blocked_for_at(who, now), Some(FIRST_BLOCK));
        t.succeeded(who);
        assert_eq!(t.blocked_for_at(who, now), None, "a good login clears it");
    }

    #[test]
    fn fails_spread_wider_than_the_window_do_not_add_up() {
        let t = Throttle::default();
        let who = caller("203.0.113.9");
        let mut now = Instant::now();
        for _ in 0..12 {
            t.failed_at(who, now);
            t.failed_at(who, now);
            now += FAIL_WINDOW / 2 + Duration::from_secs(1);
            t.failed_at(who, now);
            now += FAIL_WINDOW;
        }
        assert_eq!(t.blocked_for_at(who, now), None);
    }

    #[test]
    fn an_ipv6_caller_counts_by_its_64() {
        assert_eq!(caller("2001:db8:1:2::1"), caller("2001:db8:1:2:ffff::9"));
        assert_ne!(caller("2001:db8:1:2::1"), caller("2001:db8:1:3::1"));
        assert_eq!(caller("::ffff:192.0.2.1"), caller("192.0.2.1"));
        let t = Throttle::default();
        let now = Instant::now();
        for i in 0..5u16 {
            let ip = std::net::Ipv6Addr::new(0x2001, 0xdb8, 1, 2, i, i, i, i);
            t.failed_at(Caller::of(ip.into()), now);
        }
        assert!(t.blocked_for_at(caller("2001:db8:1:2::77"), now).is_some());
    }

    #[test]
    fn the_table_forgets_the_idlest_caller_not_a_blocked_one_in_use() {
        let t = Throttle::default();
        let start = Instant::now();
        let blocked = caller("198.51.100.1");
        for _ in 0..5 {
            t.failed_at(blocked, start);
        }
        for i in 0..MAX_TRACKED as u32 {
            let ip = std::net::Ipv4Addr::from(0x0a00_0000 + i);
            t.failed_at(Caller::of(ip.into()), start + Duration::from_millis(1));
        }
        assert_eq!(t.inner.lock().unwrap().len(), MAX_TRACKED);
        assert!(
            t.blocked_for_at(blocked, start + Duration::from_millis(2)).is_some(),
            "the idlest caller, but blocked: fresh addresses do not flush it"
        );
    }

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
