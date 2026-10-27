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

// A one-off shape; naming it would add an alias that nothing else uses.
#[allow(clippy::type_complexity)]
static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<String>)>>> = Mutex::new(None);

/// Is this an announce URL a torrent can carry? `http(s)://` or `udp://`,
/// with a host.
pub fn is_announce_url(u: &str) -> bool {
    let l = u.trim().to_ascii_lowercase();
    let rest = ["http://", "https://", "udp://"]
        .iter()
        .find_map(|p| l.strip_prefix(p));
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
    CACHE
        .lock()
        .ok()?
        .as_ref()?
        .get(url)
        .map(|(_, v)| v.clone())
}

/// The list, fetched when the cached copy is missing or older than a day.
pub fn get(url: &str) -> Result<Vec<String>, String> {
    if let Some((at, v)) = CACHE
        .lock()
        .ok()
        .and_then(|c| c.as_ref()?.get(url).cloned())
    {
        if at.elapsed() < TTL {
            return Ok(v);
        }
    }
    match fetch(url) {
        Ok(v) => {
            if let Ok(mut c) = CACHE.lock() {
                c.get_or_insert_with(HashMap::new)
                    .insert(url.to_string(), (Instant::now(), v.clone()));
            }
            Ok(v)
        }
        // Yesterday's list beats no list: the trackers in it did not stop
        // existing because GitHub did not answer.
        Err(e) => cached(url).ok_or(e),
    }
}

pub(crate) fn fetch(url: &str) -> Result<Vec<String>, String> {
    fetch_via(url, &[], typhon_engine::webseed::is_internal_ip)
}

/// `fetch` with `hosts` names resolved to fixed addresses instead of by DNS,
/// and `is_internal` deciding which addresses count as internal (the seam a
/// test uses to give a name a local address).
pub(crate) fn fetch_via(
    url: &str,
    hosts: &[(&str, std::net::IpAddr)],
    is_internal: fn(std::net::IpAddr) -> bool,
) -> Result<Vec<String>, String> {
    // The daemon's own way out (`[proxy]`, `[daemon] bind_interface`), not
    // the default route: the list host would otherwise see the home address
    // of a node whose engines all sit in a tunnel. The URL is the
    // operator's and may name an internal host; a list host out on the
    // internet may not send the fetch on to an internal address, however the
    // name is written (`egress::blocking_client_resolving`).
    let guard = std::sync::Arc::new(typhon_engine::webseed::StartGuard::for_url(url));
    let route = typhon_engine::egress::route();
    let client =
        typhon_engine::egress::blocking_client_resolving(&route, hosts, guard, is_internal)
            .map_err(|e| format!("tracker list {url}: {e}"))?;
    fetch_with(&client, url)
}

fn fetch_with(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<String>, String> {
    let resp = client
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .map_err(|e| format!("tracker list {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "tracker list {url}: HTTP {}",
            resp.status().as_u16()
        ));
    }
    let body = resp
        .bytes()
        .map_err(|e| format!("tracker list {url}: {e}"))?;
    if body.len() > MAX_BYTES {
        return Err(format!(
            "tracker list {url}: {} bytes, not a tracker list",
            body.len()
        ));
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
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A server on `ip` answering every request with `reply`, noting it was asked.
    fn serve_raw(ip: &str, reply: String) -> (u16, Arc<AtomicBool>) {
        let l = std::net::TcpListener::bind((ip, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let hit = Arc::new(AtomicBool::new(false));
        let seen = hit.clone();
        std::thread::spawn(move || {
            while let Ok((mut sock, _)) = l.accept() {
                seen.store(true, Ordering::SeqCst);
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let _ = sock.write_all(reply.as_bytes());
            }
        });
        (port, hit)
    }

    fn ok_reply(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn redirect_to(url: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    const PUBLIC: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1));
    const INNER: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 2));

    fn only_inner(ip: std::net::IpAddr) -> bool {
        ip == INNER
    }

    const LIST: &str = "udp://tracker.example:1337/announce\n";

    /// A list host out on the internet cannot send the fetch on to an
    /// internal address, written out or reached by a name.
    #[test]
    fn a_list_redirect_from_a_public_host_to_an_internal_one_is_not_followed() {
        let hosts = [("lists.example", PUBLIC), ("rebind.example", INNER)];
        for target in ["127.0.0.2", "rebind.example"] {
            let (inner, hit) = serve_raw("127.0.0.2", ok_reply(LIST));
            let (outer, _) = serve_raw(
                "127.0.0.1",
                redirect_to(&format!("http://{target}:{inner}/l")),
            );
            let r = fetch_via(
                &format!("http://lists.example:{outer}/best.txt"),
                &hosts,
                only_inner,
            );
            assert!(r.is_err(), "{target}: {r:?}");
            assert!(
                !hit.load(Ordering::SeqCst),
                "{target}: the internal address was asked"
            );
        }
    }

    /// A list on the operator's own network, and a public one that moves to
    /// another public host, are fetched as before.
    #[test]
    fn a_list_that_starts_internal_or_stays_public_is_fetched() {
        let hosts = [
            ("svc.example", INNER),
            ("lists.example", PUBLIC),
            ("mirror.example", PUBLIC),
        ];
        let (port, _) = serve_raw("127.0.0.2", ok_reply(LIST));
        let r = fetch_via(&format!("http://svc.example:{port}/l"), &hosts, only_inner);
        assert_eq!(r.unwrap(), vec!["udp://tracker.example:1337/announce"]);

        let (inner, _) = serve_raw("127.0.0.1", ok_reply(LIST));
        let (outer, _) = serve_raw(
            "127.0.0.1",
            redirect_to(&format!("http://mirror.example:{inner}/l")),
        );
        let r = fetch_via(
            &format!("http://lists.example:{outer}/l"),
            &hosts,
            only_inner,
        );
        assert_eq!(r.unwrap(), vec!["udp://tracker.example:1337/announce"]);
    }

    #[test]
    fn a_list_is_read_one_announce_url_per_line() {
        let body = "udp://tracker.opentrackr.org:1337/announce\n\n# comment\nhttp://t.example/announce\nnot a url\nudp://tracker.opentrackr.org:1337/announce\nftp://x/y\n";
        assert_eq!(
            parse(body),
            vec![
                "udp://tracker.opentrackr.org:1337/announce",
                "http://t.example/announce"
            ]
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
