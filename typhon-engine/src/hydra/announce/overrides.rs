//! Per-tracker overrides: which passkey, which client, which IP mode.
//!
//! All of them are keyed by tracker host, and all of them share one matching
//! rule, which is the whole point of this module. Getting that rule wrong is
//! not a cosmetic bug: a passkey is an account credential, and sending one
//! tracker's passkey to another hands over the account.

/// The host an override key is matched against.
///
/// Accepts a full URL, a `host:port`, or a bare host, because the callers have
/// all three: the tracker list carries URLs, the config carries hosts.
pub fn override_host(target: &str) -> String {
    let t = target.trim();
    if t.is_empty() {
        return String::new();
    }
    if t.contains("://") {
        return match url_host(t) {
            Some(h) => h.to_lowercase(),
            None => String::new(),
        };
    }
    // "[v6]:port" before "host:port": an IPv6 literal is full of colons, so
    // splitting on the last one is only right once the brackets are gone.
    if t.starts_with('[') {
        if let Some(end) = t.find(']') {
            return t[1..end].to_lowercase();
        }
    }
    match t.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => h.to_lowercase(),
        _ => t.trim_matches(|c| c == '[' || c == ']').to_lowercase(),
    }
}

/// The host part of a URL, without pulling in a URL parser for one field.
fn url_host(u: &str) -> Option<&str> {
    let rest = u.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if let Some(end) = authority.strip_prefix('[').and_then(|a| a.find(']')) {
        return Some(&authority[1..=end]);
    }
    let host = authority.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Whether a tracker host matches a configured key: the same host, or a
/// subdomain of it.
///
/// ⚠ This is deliberately NOT a substring test. It used to be, and `"torr"`
/// matched `"torr9.net"` -- so a passkey configured for one tracker was sent to
/// a different one. The old form also walked a Go map, whose iteration order is
/// randomised, so with several keys the wrong match was picked intermittently
/// rather than every time, which is the hardest kind of wrong to notice.
pub fn host_key_matches(host: &str, key: &str) -> bool {
    let key = key.trim().to_lowercase();
    if host.is_empty() || key.is_empty() {
        return false;
    }
    host == key || host.ends_with(&format!(".{key}"))
}

/// The most specific configured key matching this host.
///
/// Longest wins, so overlapping keys ("tr4ker.net" and "tk.tr4ker.net")
/// resolve deterministically to the more specific one whatever the map order.
pub fn longest_override_key<'a, I>(host: &str, keys: I) -> Option<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    keys.into_iter()
        .filter(|k| host_key_matches(host, k))
        .max_by_key(|k| k.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_read_from_a_url_a_pair_or_a_bare_name() {
        assert_eq!(override_host("https://Tracker.TR4KER.net/announce/abc"), "tracker.tr4ker.net");
        assert_eq!(override_host("udp://tracker.torr9.net:6969/announce"), "tracker.torr9.net");
        assert_eq!(override_host("tracker.torr9.net:6969"), "tracker.torr9.net");
        assert_eq!(override_host("Tracker.Torr9.NET"), "tracker.torr9.net");
        assert_eq!(override_host("[2a01:e0a::1]:6969"), "2a01:e0a::1");
        assert_eq!(override_host("   "), "");
    }

    /// ⭐ The bug this rule exists for. A key that is a prefix of another
    /// tracker's name must not match it: the passkey is an account credential,
    /// and matching loosely hands it to the wrong tracker.
    #[test]
    fn a_prefix_is_not_a_match() {
        assert!(!host_key_matches("torr9.net", "torr"));
        assert!(!host_key_matches("tracker.torr9.net", "torr"));
        // A subdomain of the key is, on the dot boundary.
        assert!(host_key_matches("tracker.torr9.net", "torr9.net"));
        assert!(host_key_matches("torr9.net", "torr9.net"));
        // ...but not a name that merely ends with it.
        assert!(!host_key_matches("nottorr9.net", "torr9.net"));
    }

    #[test]
    fn the_most_specific_key_wins_whatever_the_order() {
        let keys = ["tr4ker.net", "tk.tr4ker.net"];
        assert_eq!(longest_override_key("tk.tr4ker.net", keys), Some("tk.tr4ker.net"));
        // Reversed input, same answer: the old map-order dependency is gone.
        let reversed = ["tk.tr4ker.net", "tr4ker.net"];
        assert_eq!(longest_override_key("tk.tr4ker.net", reversed), Some("tk.tr4ker.net"));
        assert_eq!(longest_override_key("other.tr4ker.net", keys), Some("tr4ker.net"));
        assert_eq!(longest_override_key("elsewhere.net", keys), None);
    }
}
