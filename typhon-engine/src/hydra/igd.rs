//! Asking a home router to forward a port (UPnP IGD).
//!
//! The companion of `portfwd`, which speaks NAT-PMP. The two solve the same
//! problem for different equipment: NAT-PMP is what a VPN gateway answers --
//! Proton's, notably -- and almost no consumer router does. A home box speaks
//! IGD, over SSDP and SOAP. Supporting only one of them means half our users
//! get no mapping at all and cannot seed to anyone who is not already dialling
//! them.
//!
//! Done by hand rather than with a crate, for the same reason `portfwd` is:
//! the protocol is three requests, and a public repository is a worse place to
//! add a dependency than a private one.
//!
//! The sequence:
//!   1. SSDP -- a UDP multicast asking who is an InternetGatewayDevice. The
//!      answer carries a LOCATION, the URL of a description document.
//!   2. That document lists services; the one that maps ports is
//!      `WANIPConnection` or, on older ADSL boxes, `WANPPPConnection`.
//!   3. SOAP `AddPortMapping` on that service's control URL, renewed for as
//!      long as we want the mapping to live.

use std::net::SocketAddr;
use std::time::Duration;

/// Where SSDP discovery is shouted.
const SSDP_ADDR: &str = "239.255.255.250:1900";
/// How long to listen for answers. Routers reply within a second or so; the
/// spec has them stagger replies up to the MX we ask for.
const DISCOVERY_WINDOW: Duration = Duration::from_secs(3);
/// The MX we ask for, in seconds. Kept low so discovery does not hold up boot.
const MX: u8 = 2;
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// The service types that can map a port, most modern first.
const MAPPING_SERVICES: [&str; 3] = [
    "urn:schemas-upnp-org:service:WANIPConnection:2",
    "urn:schemas-upnp-org:service:WANIPConnection:1",
    "urn:schemas-upnp-org:service:WANPPPConnection:1",
];

/// A gateway we can talk to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gateway {
    /// Absolute URL of the service that maps ports.
    pub control_url: String,
    /// The service type, needed verbatim in every SOAP action.
    pub service_type: String,
}

/// The SSDP search datagram.
///
/// `MAN` really is a quoted string with a colon in it, and `ST` has to match a
/// device type the router advertises. Routers are strict about both: a
/// malformed search is answered with silence, which is indistinguishable from
/// having no router at all.
pub fn search_request() -> String {
    format!(
        "M-SEARCH * HTTP/1.1\r\n\
         HOST: {SSDP_ADDR}\r\n\
         MAN: \"ssdp:discover\"\r\n\
         MX: {MX}\r\n\
         ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
    )
}

/// The `LOCATION` of a description document, from an SSDP reply.
///
/// Header names are case-insensitive and routers disagree about the case they
/// use, so the comparison is too.
pub fn location_of(reply: &str) -> Option<String> {
    for line in reply.lines() {
        // `?` here would give up on the status line, which carries no colon at
        // all -- and the status line is always first.
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("location") {
            let v = value.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Resolve a control URL, which the description document may give relative.
///
/// Routers give all three forms: absolute, root-relative, and bare. Getting
/// this wrong sends the SOAP call to the wrong host and the mapping silently
/// never happens.
pub fn resolve_url(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        return url.to_string();
    }
    // The scheme and authority of the base: everything before the third slash.
    let origin = match base.find("://") {
        Some(i) => match base[i + 3..].find('/') {
            Some(j) => &base[..i + 3 + j],
            None => base,
        },
        None => base,
    };
    if let Some(rest) = url.strip_prefix('/') {
        format!("{origin}/{rest}")
    } else {
        format!("{origin}/{url}")
    }
}

/// Find the control URL of the first service that can map a port.
///
/// The document is XML, walked by hand: it is a fixed shape and pulling in a
/// parser to read two tags out of it is not a trade worth making. What matters
/// is that `controlURL` is taken from inside the SAME `<service>` block as the
/// `serviceType` that matched -- a document lists several services, and taking
/// the first `controlURL` in the file maps ports on whichever came first.
pub fn find_service(xml: &str, base: &str) -> Option<Gateway> {
    for wanted in MAPPING_SERVICES {
        let mut rest = xml;
        while let Some(i) = rest.find(wanted) {
            // The block containing this service type, bounded by </service>.
            let after = &rest[i..];
            let block_end = after.find("</service>").unwrap_or(after.len());
            let block = &after[..block_end];
            if let Some(url) = tag_value(block, "controlURL") {
                return Some(Gateway {
                    control_url: resolve_url(base, &url),
                    service_type: wanted.to_string(),
                });
            }
            rest = &after[wanted.len()..];
        }
    }
    None
}

/// The text of the first `<tag>` in this fragment.
fn tag_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    let v = xml[start..end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// The SOAP body asking for a mapping.
///
/// `NewLeaseDuration` of zero means "forever" to some routers and is rejected
/// by others, so a finite lease is always asked for and renewed. A permanent
/// mapping also outlives the process that wanted it, which is how a router
/// ends up with a table full of entries nobody can explain.
pub fn add_mapping_body(
    service_type: &str,
    internal_ip: &str,
    internal_port: u16,
    external_port: u16,
    protocol: &str,
    lease: Duration,
) -> String {
    format!(
        "<?xml version=\"1.0\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:AddPortMapping xmlns:u=\"{service_type}\">\
         <NewRemoteHost></NewRemoteHost>\
         <NewExternalPort>{external_port}</NewExternalPort>\
         <NewProtocol>{protocol}</NewProtocol>\
         <NewInternalPort>{internal_port}</NewInternalPort>\
         <NewInternalClient>{internal_ip}</NewInternalClient>\
         <NewEnabled>1</NewEnabled>\
         <NewPortMappingDescription>Hydranos</NewPortMappingDescription>\
         <NewLeaseDuration>{}</NewLeaseDuration>\
         </u:AddPortMapping></s:Body></s:Envelope>",
        lease.as_secs()
    )
}

/// A router's refusal, in its own words.
///
/// A SOAP fault comes back as HTTP 500 with the reason in the body. Reporting
/// "HTTP 500" alone leaves an operator with nothing; 718 and 725 in particular
/// are routine and mean something they can act on.
pub fn parse_fault(xml: &str) -> Option<(u16, String)> {
    let code: u16 = tag_value(xml, "errorCode")?.parse().ok()?;
    Some((code, fault_text(code).to_string()))
}

pub fn fault_text(code: u16) -> &'static str {
    match code {
        402 => "the router rejected the request as malformed",
        501 => "the router failed to act on the request",
        606 => "the router refuses to take orders from us",
        714 => "no such mapping to remove",
        715 => "the router will not map for a wildcard source",
        716 => "the router will not map a wildcard external port",
        718 => "that external port is already mapped to another machine",
        724 => "the router only maps a port to the same port number",
        725 => "the router only grants permanent leases",
        727 => "the router only maps to the same internal port",
        _ => "the router refused, without saying why",
    }
}

/// Shout on the local network and take the first gateway that answers.
pub async fn discover() -> Result<Gateway, String> {
    let target: SocketAddr = SSDP_ADDR
        .parse()
        .map_err(|e| format!("bad SSDP address: {e}"))?;
    discover_at(target).await
}

/// The same, asking a chosen address.
///
/// Split out so the search can be pointed at a socket on loopback: the
/// multicast group is the one part of this that a test cannot join, and
/// leaving it hard-coded would mean the whole discovery path ships unrun.
pub async fn discover_at(target: SocketAddr) -> Result<Gateway, String> {
    let socket = tokio::net::UdpSocket::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| format!("cannot open a socket to search: {e}"))?;
    socket
        .send_to(search_request().as_bytes(), target)
        .await
        .map_err(|e| format!("cannot send the search: {e}"))?;

    let deadline = tokio::time::Instant::now() + DISCOVERY_WINDOW;
    let mut buf = [0u8; 2048];
    let mut last = String::from("no router answered the search");

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(last);
        }
        let n = match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => n,
            // ⚠ Windows surfaces the ICMP port-unreachable from our own
            // search as WSAECONNRESET on the NEXT recv of the same UDP
            // socket; Linux swallows it. Treating it as fatal turned "no
            // router answered" -- the ordinary case on a machine with no
            // UPnP gateway -- into a hard error on every Windows host. Keep
            // listening until the deadline; other devices may still answer.
            Ok(Err(e)) if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionRefused
            ) => continue,
            Ok(Err(e)) => return Err(format!("cannot read the answer: {e}")),
            Err(_) => return Err(last),
        };
        let reply = String::from_utf8_lossy(&buf[..n]);
        let Some(location) = location_of(&reply) else {
            continue;
        };
        // Several devices may answer, and not all of them can map a port.
        // Keep asking until one describes a service that can.
        match describe(&location).await {
            Ok(g) => return Ok(g),
            Err(e) => last = e,
        }
    }
}

/// Fetch a description document and find the mapping service in it.
pub async fn describe(location: &str) -> Result<Gateway, String> {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let body = client
        .get(location)
        .send()
        .await
        .map_err(|e| format!("cannot fetch {location}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("cannot read {location}: {e}"))?;
    find_service(&body, location)
        .ok_or_else(|| format!("{location} describes no port mapping service"))
}

/// Ask for one mapping.
pub async fn add_mapping(
    gateway: &Gateway,
    internal_ip: &str,
    internal_port: u16,
    external_port: u16,
    lease: Duration,
) -> Result<(), String> {
    let body = add_mapping_body(
        &gateway.service_type,
        internal_ip,
        internal_port,
        external_port,
        "TCP",
        lease,
    );
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let resp = client
        .post(&gateway.control_url)
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        // ⚠️ This goes out as `soapaction:`, lowercased by the `http` crate.
        // HTTP/1.1 says header names are case-insensitive and a correct router
        // does not care -- but some cheap firmwares string-match `SOAPAction`
        // literally and answer 500 to anything else. That is why miniupnpc
        // builds its request by hand on a raw socket.
        //
        // Left as it is until a router is seen refusing it: writing our own
        // HTTP client to control one capital letter is a trade worth making
        // only against evidence, and the failure is loud -- the mapping is
        // refused, the reason is logged, and `map_once` falls through.
        .header(
            "SOAPAction",
            format!("\"{}#AddPortMapping\"", gateway.service_type),
        )
        .body(body)
        .send()
        .await
        .map_err(|e| format!("cannot reach the router: {e}"))?;

    if resp.status().is_success() {
        return Ok(());
    }
    let text = resp.text().await.unwrap_or_default();
    match parse_fault(&text) {
        Some((code, why)) => Err(format!("{why} (UPnP error {code})")),
        None => Err("the router refused the mapping".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = r#"
<root><device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <serviceList>
    <service>
      <serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType>
      <controlURL>/ctl/L3F</controlURL>
    </service>
  </serviceList>
  <deviceList><device><serviceList>
    <service>
      <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
      <controlURL>/ctl/IPConn</controlURL>
    </service>
  </serviceList></device></deviceList>
</device></root>"#;

    /// Routers answer a malformed search with silence, which reads exactly like
    /// having no router. Every field here is one a router checks.
    #[test]
    fn the_search_carries_what_a_router_checks() {
        let r = search_request();
        assert!(r.starts_with("M-SEARCH * HTTP/1.1\r\n"));
        assert!(r.contains("MAN: \"ssdp:discover\""), "quoted, with the colon: {r}");
        assert!(r.contains("HOST: 239.255.255.250:1900"));
        assert!(r.contains("ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1"));
        assert!(r.ends_with("\r\n\r\n"), "a blank line ends the request");
    }

    /// Header case is the router's choice, not ours.
    #[test]
    fn the_location_is_found_whatever_its_case() {
        let reply = "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nlocation: http://192.168.99.1:5000/rootDesc.xml\r\n\r\n";
        assert_eq!(
            location_of(reply).as_deref(),
            Some("http://192.168.99.1:5000/rootDesc.xml")
        );
        let upper = "HTTP/1.1 200 OK\r\nLOCATION: http://10.0.0.1/d.xml\r\n\r\n";
        assert_eq!(location_of(upper).as_deref(), Some("http://10.0.0.1/d.xml"));
    }

    #[test]
    fn a_reply_without_a_location_is_not_a_gateway() {
        assert_eq!(location_of("HTTP/1.1 200 OK\r\nSERVER: x\r\n\r\n"), None);
    }

    /// The control URL comes from the same `<service>` block as the matching
    /// service type. Taking the first one in the document would post to
    /// Layer3Forwarding, which maps nothing and fails obscurely.
    #[test]
    fn the_control_url_comes_from_the_matching_service() {
        let g = find_service(DESC, "http://192.168.99.1:5000/rootDesc.xml").unwrap();
        assert_eq!(g.control_url, "http://192.168.99.1:5000/ctl/IPConn");
        assert_eq!(g.service_type, "urn:schemas-upnp-org:service:WANIPConnection:1");
    }

    /// Older ADSL boxes only have the PPP flavour.
    #[test]
    fn the_ppp_service_is_accepted_too() {
        let xml = DESC.replace("WANIPConnection:1", "WANPPPConnection:1");
        let g = find_service(&xml, "http://192.168.99.1:5000/d.xml").unwrap();
        assert_eq!(g.service_type, "urn:schemas-upnp-org:service:WANPPPConnection:1");
    }

    #[test]
    fn a_device_that_maps_nothing_yields_no_gateway() {
        let xml = "<root><service><serviceType>urn:schemas-upnp-org:service:WANCommonInterfaceConfig:1</serviceType><controlURL>/x</controlURL></service></root>";
        assert!(find_service(xml, "http://192.168.99.1/d.xml").is_none());
    }

    /// Routers give all three forms, and posting to the wrong host means the
    /// mapping never happens and nothing says so.
    #[test]
    fn a_control_url_is_resolved_in_all_three_forms() {
        let base = "http://192.168.99.1:5000/rootDesc.xml";
        assert_eq!(resolve_url(base, "http://10.0.0.1/ctl"), "http://10.0.0.1/ctl");
        assert_eq!(resolve_url(base, "/ctl/IPConn"), "http://192.168.99.1:5000/ctl/IPConn");
        assert_eq!(resolve_url(base, "ctl/IPConn"), "http://192.168.99.1:5000/ctl/IPConn");
    }

    #[test]
    fn the_mapping_request_says_which_ports_and_for_how_long() {
        let b = add_mapping_body(
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            "192.168.99.50",
            16171,
            16171,
            "TCP",
            Duration::from_secs(3600),
        );
        assert!(b.contains("<NewInternalPort>16171</NewInternalPort>"));
        assert!(b.contains("<NewExternalPort>16171</NewExternalPort>"));
        assert!(b.contains("<NewInternalClient>192.168.99.50</NewInternalClient>"));
        assert!(b.contains("<NewProtocol>TCP</NewProtocol>"));
        assert!(b.contains("<NewLeaseDuration>3600</NewLeaseDuration>"));
        assert!(b.contains("<NewEnabled>1</NewEnabled>"));
        // Named, so an operator looking at the router's table knows what it is.
        assert!(b.contains("<NewPortMappingDescription>Hydranos</NewPortMappingDescription>"));
    }

    /// A fault is an HTTP 500 with the reason in the body. "HTTP 500" alone
    /// leaves an operator with nothing to act on; 718 tells them exactly what
    /// to change.
    #[test]
    fn a_refusal_is_read_out_of_the_fault_body() {
        let xml = "<s:Envelope><s:Body><s:Fault><detail><UPnPError>\
                   <errorCode>718</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>";
        let (code, why) = parse_fault(xml).unwrap();
        assert_eq!(code, 718);
        assert!(why.contains("already mapped"), "{why}");
    }

    #[test]
    fn a_body_that_is_not_a_fault_yields_nothing() {
        assert!(parse_fault("<html>gateway timeout</html>").is_none());
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const DESC_DOC: &str = r#"<?xml version="1.0"?>
<root><device>
  <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
  <serviceList><service>
    <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
    <controlURL>/ctl/IPConn</controlURL>
  </service></serviceList>
</device></root>"#;

    const FAULT: &str = r#"<?xml version="1.0"?>
<s:Envelope><s:Body><s:Fault><detail><UPnPError>
<errorCode>718</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    /// A router that answers one request with `status` and `body`, then stops.
    /// Returns its base URL and the request it received.
    fn router(status: &'static str, body: &'static str) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut buf = Vec::new();
            let mut byte = [0u8; 1];
            // Head only, then whatever body is already buffered: enough to see
            // the SOAPAction header and the action name.
            while !buf.ends_with(b"\r\n\r\n") {
                match sock.read(&mut byte) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(byte[0]),
                }
            }
            let mut rest = vec![0u8; 4096];
            sock.set_read_timeout(Some(std::time::Duration::from_millis(200))).ok();
            if let Ok(n) = sock.read(&mut rest) {
                buf.extend_from_slice(&rest[..n]);
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(body.as_bytes());
            let _ = sock.flush();
        });
        (format!("http://127.0.0.1:{port}/rootDesc.xml"), rx)
    }

    /// The real fetch-and-parse path, against a router that answers. Only the
    /// parsing of this was covered before; the request itself never ran.
    #[test]
    fn a_description_document_is_fetched_and_read() {
        let (url, _rx) = router("200 OK", DESC_DOC);
        let g = rt().block_on(describe(&url)).expect("the router described itself");
        assert_eq!(g.service_type, "urn:schemas-upnp-org:service:WANIPConnection:1");
        // Resolved against the document's own URL, not guessed.
        assert!(g.control_url.ends_with("/ctl/IPConn"), "{}", g.control_url);
        assert!(g.control_url.starts_with("http://127.0.0.1:"));
    }

    /// A device that describes no mapping service is not a gateway. Answering
    /// otherwise would send the SOAP call somewhere that maps nothing.
    #[test]
    fn a_document_without_a_mapping_service_is_refused() {
        let (url, _rx) = router("200 OK", "<root><device></device></root>");
        let err = rt().block_on(describe(&url)).expect_err("no service, no gateway");
        assert!(err.contains("no port mapping service"), "{err}");
    }

    /// The mapping request itself: it has to carry the SOAPAction header, and
    /// the action name has to be in the body. A router rejects either mistake
    /// with a fault that says nothing useful.
    #[test]
    fn a_mapping_request_carries_the_soap_action_and_the_body() {
        let (url, rx) = router("200 OK", "<ok/>");
        let g = Gateway {
            control_url: url,
            service_type: "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
        };
        rt().block_on(add_mapping(&g, "192.168.99.50", 16171, 16171, Duration::from_secs(3600)))
            .expect("the router accepted");

        let req = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("a request arrived");
        assert!(req.starts_with("POST "), "{req}");
        // Lowercased, because the `http` crate normalises header names and
        // HTTP/1.1 says they are case-insensitive. Asserted as it actually
        // goes out rather than as one might wish: see the note on the header
        // in `add_mapping`.
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains(
                "soapaction: \"urn:schemas-upnp-org:service:wanipconnection:1#addportmapping\""
            ),
            "the action header is what a router dispatches on: {req}"
        );
        assert!(req.contains("<u:AddPortMapping"), "{req}");
        assert!(req.contains("<NewInternalPort>16171</NewInternalPort>"), "{req}");
    }

    /// A refusal arrives as HTTP 500 with the reason in the body. Reporting the
    /// status alone leaves an operator with nothing to change; 718 tells them
    /// the port is taken.
    #[test]
    fn a_soap_fault_becomes_the_routers_own_reason() {
        let (url, _rx) = router("500 Internal Server Error", FAULT);
        let g = Gateway {
            control_url: url,
            service_type: "urn:schemas-upnp-org:service:WANIPConnection:1".into(),
        };
        let err = rt()
            .block_on(add_mapping(&g, "192.168.99.50", 16171, 16171, Duration::from_secs(3600)))
            .expect_err("718 is a refusal");
        assert!(err.contains("already mapped"), "{err}");
        assert!(err.contains("718"), "the number is there for searching: {err}");
    }

    /// The whole discovery path, end to end: a search goes out, an answer comes
    /// back with a LOCATION, the document behind it is fetched and read. None
    /// of this ran before -- only the string parsing did.
    #[test]
    fn discovery_finds_a_gateway_that_answers() {
        let (desc_url, _rx) = router("200 OK", DESC_DOC);

        // A router listening for the search, on loopback rather than the
        // multicast group a test cannot join.
        let ssdp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let ssdp_addr = ssdp.local_addr().unwrap();
        let reply_url = desc_url.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let Ok((n, from)) = ssdp.recv_from(&mut buf) else {
                return;
            };
            let search = String::from_utf8_lossy(&buf[..n]).into_owned();
            // Only answer a search that is actually one: a router that is sent
            // a malformed M-SEARCH stays silent, and so does this.
            if !search.starts_with("M-SEARCH * HTTP/1.1") || !search.contains("ssdp:discover") {
                return;
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nLOCATION: {reply_url}\r\n\
                 ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
            );
            let _ = ssdp.send_to(reply.as_bytes(), from);
        });

        let g = rt().block_on(discover_at(ssdp_addr)).expect("a gateway answered");
        assert_eq!(g.service_type, "urn:schemas-upnp-org:service:WANIPConnection:1");
        assert!(g.control_url.ends_with("/ctl/IPConn"));
    }

    /// Nothing on the network is not an error to shout about, and it must not
    /// hang: a machine with no router is the ordinary case.
    #[test]
    fn a_silent_network_gives_up_rather_than_waiting() {
        let quiet = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let addr = quiet.local_addr().unwrap();
        drop(quiet); // nothing is listening there now

        let started = std::time::Instant::now();
        let err = rt().block_on(discover_at(addr)).expect_err("nobody answered");
        assert!(err.contains("no router"), "{err}");
        assert!(
            started.elapsed() < DISCOVERY_WINDOW + Duration::from_secs(2),
            "gave up in {:?}, which is longer than the window",
            started.elapsed()
        );
    }
}
