//! Tracker lists fetched from a URL, for the "add trackers" workflow action.
//!
//! A list like ngosang/trackerslist is a text file, one announce URL per line.
//! It is NOT a tracker: put as one, it answers its own text to every announce
//! (`unexpected byte 'h' at 0`). The action takes the list's URL, fetches it,
//! and adds what it contains.
//!
//! Fetched at most once a day per URL and kept in memory. A list that cannot
//! be fetched falls back to the last copy that could; with none, the action
//! fails out loud rather than adding nothing and reporting success.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a fetched list is used before being fetched again.
const TTL: Duration = Duration::from_secs(24 * 3600);
/// More than this is not a tracker list: ngosang's biggest is under 200.
const MAX_URLS: usize = 500;
/// Nor is a body past this.
const MAX_BYTES: usize = 1 << 20;

static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<String>)>>> = Mutex::new(None);

/// Is this an announce URL a torrent can carry? `http(s)://` or `udp://`,
/// with a host.
pub fn is_announce_url(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    let rest = ["http://", "https://", "udp://"].iter().find_map(|p| l.strip_prefix(p));
    matches!(rest, Some(r) if !r.is_empty() && !r.starts_with('/') && !r.starts_with(':'))
}

/// The announce URLs in a list's body: one per line, blank lines and `#`
/// comments skipped, anything that is not an announce URL dropped, each kept
/// once, in order.
pub fn parse(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        let u = line.trim();
        if u.is_empty() || u.starts_with('#') || !is_announce_url(u) {
            continue;
        }
        if !out.iter().any(|x| x == u) {
            out.push(u.to_string());
        }
        if out.len() >= MAX_URLS {
            break;
        }
    }
    out
}

/// The list as last fetched, if it was, however old. For the convergence
/// check, which must not wait on the network.
pub fn cached(url: &str) -> Option<Vec<String>> {
    CACHE.lock().ok()?.as_ref()?.get(url).map(|(_, v)| v.clone())
}

/// The list, fetched when the cached copy is missing or older than a day.
pub fn get(url: &str) -> Result<Vec<String>, String> {
    if let Some((at, v)) = CACHE.lock().ok().and_then(|c| c.as_ref()?.get(url).cloned()) {
        if at.elapsed() < TTL {
            return Ok(v);
        }
    }
    match fetch(url) {
        Ok(v) => {
            if let Ok(mut c) = CACHE.lock() {
                c.get_or_insert_with(HashMap::new).insert(url.to_string(), (Instant::now(), v.clone()));
            }
            Ok(v)
        }
        // Yesterday's list beats no list: the trackers in it did not stop
        // existing because GitHub did not answer.
        Err(e) => cached(url).ok_or(e),
    }
}

fn fetch(url: &str) -> Result<Vec<String>, String> {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(typhon_engine::config::user_agent())
            .build()
            .unwrap_or_default()
    });
    let resp = client.get(url).send().map_err(|e| format!("tracker list {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("tracker list {url}: HTTP {}", resp.status().as_u16()));
    }
    let body = resp.bytes().map_err(|e| format!("tracker list {url}: {e}"))?;
    if body.len() > MAX_BYTES {
        return Err(format!("tracker list {url}: {} bytes, not a tracker list", body.len()));
    }
    let urls = parse(&String::from_utf8_lossy(&body));
    if urls.is_empty() {
        return Err(format!("tracker list {url}: no announce URL in it"));
    }
    Ok(urls)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_list_is_read_one_announce_url_per_line() {
        let body = "udp://tracker.opentrackr.org:1337/announce\n\n# comment\nhttp://t.example/announce\nnot a url\nudp://tracker.opentrackr.org:1337/announce\nftp://x/y\n";
        assert_eq!(
            parse(body),
            vec!["udp://tracker.opentrackr.org:1337/announce", "http://t.example/announce"]
        );
    }

    #[test]
    fn only_tracker_schemes_with_a_host_count() {
        assert!(is_announce_url("udp://t.example:80/announce"));
        assert!(is_announce_url("HTTPS://t.example/announce"));
        assert!(!is_announce_url("http://"));
        assert!(!is_announce_url("magnet:?xt=urn:btih:00"));
        assert!(!is_announce_url("wss://t.example"));
    }
}
