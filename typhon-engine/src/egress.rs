//! The daemon's own way out: every request that belongs to no engine.
//!
//! The engines pin their sockets (`netpin`) and proxy their announces, but the
//! daemon itself also talks to the internet: tracker lists, ipfilter lists,
//! the update check, webhooks, `.torrent` URLs. Each of those built its own
//! reqwest client on the default route, so a node whose engines were carefully
//! kept inside a tunnel still showed the home address to GitHub, to a list
//! host and to every webhook target. This is the one place such a client is
//! built, from `[proxy]` (SOCKS5, `socks5h`: the name is resolved by the
//! proxy) and `[daemon] bind_interface`.
//!
//! `[daemon] kill_switch` turns "the configured way" into "the only way": with
//! neither a proxy nor an interface there is no way at all and every request
//! is refused, and an interface that is not there refuses rather than leaving
//! by the default route. Without it, an interface that is missing still fails
//! the request -- reqwest binds the socket to it -- but no route at all means
//! the default one, as before.
//!
//! The route is process-wide and replaced whenever the configuration is: the
//! clients built from it are cached by generation, so a change takes effect
//! at the next request without a restart.

use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// Where the daemon's own requests leave by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Route {
    /// `socks5h://...`, credentials escaped. Empty = no proxy.
    pub proxy: String,
    /// Interface the sockets (to the target, or to the proxy) are bound to.
    pub interface: String,
    /// Nothing leaves outside the route above; no route = nothing leaves.
    pub kill_switch: bool,
}

/// The refusal when the kill switch is on and nothing says where to go.
pub const NO_ROUTE: &str = "kill switch: no proxy ([proxy] socks5_host) and no interface ([daemon] bind_interface) \
     for the daemon's own traffic, so it is not sent at all (never by the default route)";

impl Route {
    /// From the `[proxy]` keys 3.x already had and the `[daemon]` ones.
    pub fn new(socks5_host: &str, socks5_port: u16, user: &str, pass: &str, interface: &str, kill_switch: bool) -> Route {
        let host = socks5_host.trim();
        let proxy = if host.is_empty() {
            String::new()
        } else {
            let auth = if user.is_empty() {
                String::new()
            } else {
                format!(
                    "{}:{}@",
                    crate::config::userinfo_escape(user),
                    crate::config::userinfo_escape(pass)
                )
            };
            let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
            let port = if socks5_port == 0 { 1080 } else { socks5_port };
            format!("socks5h://{auth}{host}:{port}")
        };
        Route { proxy, interface: interface.trim().to_string(), kill_switch }
    }

    /// No proxy, no interface: the default route.
    pub fn is_direct(&self) -> bool {
        self.proxy.is_empty() && self.interface.is_empty()
    }

    /// One line for a log or the Network tab, credentials removed.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.proxy.is_empty() {
            parts.push(format!("proxy {}", crate::tracker::http::redact_proxy(&self.proxy)));
        }
        if !self.interface.is_empty() {
            parts.push(format!("interface {}", self.interface));
        }
        if parts.is_empty() {
            parts.push("the default route".into());
        }
        let mut s = parts.join(", via ");
        if self.kill_switch {
            s.push_str(" (kill switch on)");
        }
        s
    }

    /// Whether a request may leave by this route right now; why not otherwise.
    ///
    /// Checked before every client is handed out, not only when it is built:
    /// a tunnel can go away under a cached client, and the error then names
    /// the reason instead of a bare "connection refused".
    pub fn preflight(&self) -> Result<(), String> {
        if self.kill_switch && self.is_direct() {
            return Err(NO_ROUTE.into());
        }
        if !self.interface.is_empty() {
            interface_usable(&self.interface)?;
        }
        if !self.proxy.is_empty() {
            reqwest::Proxy::all(&self.proxy).map_err(|e| {
                format!("proxy {} is not usable: {e}", crate::tracker::http::redact_proxy(&self.proxy))
            })?;
        }
        Ok(())
    }
}

/// The interface exists here, and requests can be bound to it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn interface_usable(dev: &str) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if !std::path::Path::new("/sys/class/net").join(dev).exists() {
        return Err(format!("interface {dev} is not there; the daemon's traffic is not sent by another one"));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn interface_usable(dev: &str) -> Result<(), String> {
    Err(format!(
        "[daemon] bind_interface {dev:?} cannot be applied on this platform; set a proxy instead \
         (the request is refused rather than sent by the default route)"
    ))
}

/// The live route and its generation.
fn current() -> &'static RwLock<(u64, Arc<Route>)> {
    static R: OnceLock<RwLock<(u64, Arc<Route>)>> = OnceLock::new();
    R.get_or_init(|| RwLock::new((0, Arc::new(Route::default()))))
}

/// Install a route. True when it differs from the one in force.
pub fn set(route: Route) -> bool {
    let mut cur = current().write().unwrap_or_else(|p| p.into_inner());
    if *cur.1 == route {
        return false;
    }
    let generation = cur.0 + 1;
    *cur = (generation, Arc::new(route));
    true
}

/// The route in force.
pub fn route() -> Arc<Route> {
    current().read().unwrap_or_else(|p| p.into_inner()).1.clone()
}

/// The same settings on reqwest's async and blocking builders, which share
/// method names but no trait.
macro_rules! apply_route {
    ($builder:expr, $route:expr) => {{
        let route: &Route = $route;
        let mut b = $builder.user_agent(crate::config::user_agent());
        if !route.proxy.is_empty() {
            // `no_proxy` first: an HTTP_PROXY in the environment is a second
            // way out the operator did not write here.
            let p = reqwest::Proxy::all(&route.proxy).map_err(|e| {
                format!("proxy {} is not usable: {e}", crate::tracker::http::redact_proxy(&route.proxy))
            })?;
            b = b.no_proxy().proxy(p);
        } else if route.kill_switch {
            b = b.no_proxy();
        }
        if !route.interface.is_empty() {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                b = b.interface(&route.interface);
            }
            // With a proxy the name is the proxy's to resolve. Without one,
            // a managed tunnel's names go to the tunnel's DNS server.
            if route.proxy.is_empty() {
                b = b.dns_resolver(crate::tunneldns::Resolver::new(&route.interface));
            }
        }
        b
    }};
}

/// An async client for `route`, checked first. Callers set their own timeout
/// per request: a webhook and an ipfilter download do not wait alike.
pub fn client_for(route: &Route) -> Result<reqwest::Client, String> {
    route.preflight()?;
    let b = apply_route!(reqwest::Client::builder(), route);
    b.build().map_err(|e| format!("http client: {e}"))
}

/// The blocking twin, for the callers that run on a plain thread.
pub fn blocking_client_for(route: &Route) -> Result<reqwest::blocking::Client, String> {
    route.preflight()?;
    let b = apply_route!(reqwest::blocking::Client::builder(), route);
    b.build().map_err(|e| format!("http client: {e}"))
}

/// The daemon's async client, on the route in force.
pub fn client() -> Result<reqwest::Client, String> {
    static CACHE: Mutex<Option<(u64, reqwest::Client)>> = Mutex::new(None);
    let (generation, route) = current().read().unwrap_or_else(|p| p.into_inner()).clone();
    route.preflight()?;
    let mut slot = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((g, c)) = slot.as_ref() {
        if *g == generation {
            return Ok(c.clone());
        }
    }
    let c = client_for(&route)?;
    *slot = Some((generation, c.clone()));
    Ok(c)
}

/// The daemon's blocking client, on the route in force.
///
/// ⚠ Only from a blocking thread: a superseded client is dropped by the
/// caller that replaces it, and reqwest panics when a blocking client is
/// dropped inside the async runtime.
pub fn blocking_client() -> Result<reqwest::blocking::Client, String> {
    static CACHE: Mutex<Option<(u64, reqwest::blocking::Client)>> = Mutex::new(None);
    let (generation, route) = current().read().unwrap_or_else(|p| p.into_inner()).clone();
    route.preflight()?;
    let mut slot = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((g, c)) = slot.as_ref() {
        if *g == generation {
            return Ok(c.clone());
        }
    }
    let c = blocking_client_for(&route)?;
    *slot = Some((generation, c.clone()));
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A SOCKS5 server that records the host it is asked for and answers
    /// every request with `200 ok` itself.
    async fn fake_socks5() -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut b = [0u8; 2];
                    s.read_exact(&mut b).await?;
                    let mut m = vec![0u8; b[1] as usize];
                    s.read_exact(&mut m).await?;
                    s.write_all(&[5, 0]).await?;
                    let mut h = [0u8; 4];
                    s.read_exact(&mut h).await?;
                    let host = match h[3] {
                        3 => {
                            let mut n = [0u8; 1];
                            s.read_exact(&mut n).await?;
                            let mut name = vec![0u8; n[0] as usize];
                            s.read_exact(&mut name).await?;
                            String::from_utf8_lossy(&name).into_owned()
                        }
                        1 => {
                            let mut a = [0u8; 4];
                            s.read_exact(&mut a).await?;
                            std::net::Ipv4Addr::from(a).to_string()
                        }
                        _ => return Ok::<(), std::io::Error>(()),
                    };
                    let mut p = [0u8; 2];
                    s.read_exact(&mut p).await?;
                    log.lock().unwrap().push(host);
                    s.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                    let mut req = [0u8; 1024];
                    let _ = s.read(&mut req).await?;
                    s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await?;
                    Ok(())
                });
            }
        });
        (port, seen)
    }

    #[tokio::test]
    async fn the_proxy_is_handed_the_name_not_an_address() {
        let (port, seen) = fake_socks5().await;
        let route = Route::new("127.0.0.1", port, "", "", "", true);
        let body = client_for(&route)
            .unwrap()
            .get("http://lists.hydranos.invalid/trackers.txt")
            .send()
            .await
            .expect("through the proxy")
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
        // socks5h: the name went to the proxy, so no DNS query left from here
        // (`.invalid` would not have resolved anyway).
        assert_eq!(*seen.lock().unwrap(), vec!["lists.hydranos.invalid".to_string()]);
    }

    /// The test the kill switch exists for: the proxy is not there, and the
    /// target, which IS reachable directly, must not be contacted.
    #[tokio::test]
    async fn an_unreachable_proxy_refuses_and_never_goes_direct() {
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = target.accept().await {
                h.fetch_add(1, Ordering::SeqCst);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
            }
        });
        // A port nobody listens on.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let route = Route::new("127.0.0.1", dead_port, "u", "p@ss", "", true);
        let r = client_for(&route).unwrap().get(format!("http://127.0.0.1:{target_port}/")).send().await;
        assert!(r.is_err(), "a dead proxy is a failed request");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no direct connection to the target");
    }

    #[test]
    fn the_kill_switch_with_no_route_refuses_every_request() {
        let r = Route::new("", 0, "", "", "", true);
        assert_eq!(client_for(&r).err().as_deref(), Some(NO_ROUTE));
        assert_eq!(blocking_client_for(&r).err().as_deref(), Some(NO_ROUTE));
        // Without it, no route is the default route.
        assert!(client_for(&Route::new("", 0, "", "", "", false)).is_ok());
    }

    #[test]
    fn a_missing_interface_refuses_with_its_name() {
        let r = Route::new("", 0, "", "", "hydranos-nope0", false);
        let e = client_for(&r).err().expect("refused");
        assert!(e.contains("hydranos-nope0"), "{e}");
    }

    #[test]
    fn the_route_url_escapes_credentials_and_brackets_v6() {
        let r = Route::new("::1", 0, "us er", "p@ss:w", "", false);
        assert_eq!(r.proxy, "socks5h://us%20er:p%40ss%3Aw@[::1]:1080");
        assert!(!r.describe().contains("p%40ss"), "{}", r.describe());
    }

    #[test]
    fn a_new_route_is_a_new_generation() {
        let before = route();
        let other = Route { interface: "lo".into(), ..(*before).clone() };
        assert!(set(other.clone()));
        assert!(!set(other), "the same route twice is no change");
        set((*before).clone());
    }
}
