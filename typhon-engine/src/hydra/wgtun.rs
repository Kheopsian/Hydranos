//! Reading a WireGuard configuration.
//!
//! The wg-quick-only keys (Address, DNS, MTU, Table, PreUp...) are kept apart
//! from the ones `wg setconf` understands, because handing wg a file that
//! contains `Address =` makes it refuse the whole file. Splitting them here is
//! what lets a provider's .conf be used exactly as it was handed out.
//!
//! `Table` is parsed only so we can say out loud that we ignore it: we never
//! install a default route.

/// One `[Peer]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peer {
    pub public_key: String,
    pub preshared_key: String,
    pub endpoint: String,
    pub allowed_ips: Vec<String>,
    pub keepalive: u32,
}

/// A parsed wg-quick style configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Conf {
    pub private_key: String,
    pub addresses: Vec<String>,
    pub listen_port: u16,
    pub fwmark: u32,
    pub mtu: u32,
    pub dns: Vec<String>,
    pub peers: Vec<Peer>,
    /// Kept, never applied.
    pub table: String,
}

/// Parse a configuration.
///
/// Strict about the two things that would otherwise fail later and silently --
/// a missing private key, and a peer with no public key -- and lax about keys
/// it does not use. Provider configs carry all sorts of extras, and refusing a
/// file over a line nobody reads would just send the operator back to
/// wg-quick.
pub fn parse(text: &str) -> Result<Conf, String> {
    let mut conf = Conf::default();
    let mut section = String::new();
    let mut peer: Option<Peer> = None;

    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            if section.eq_ignore_ascii_case("peer") {
                if let Some(p) = peer.take() {
                    conf.peers.push(p);
                }
            }
            section = line[1..line.len() - 1].to_lowercase();
            if section == "peer" {
                peer = Some(Peer::default());
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", n + 1));
        };
        let key = key.trim().to_lowercase();
        let value = value.trim().to_string();

        match section.as_str() {
            "interface" => match key.as_str() {
                "privatekey" => conf.private_key = value,
                "address" => conf.addresses = split_list(&value),
                "listenport" => conf.listen_port = value.parse().unwrap_or(0),
                "fwmark" => conf.fwmark = parse_fwmark(&value),
                "mtu" => conf.mtu = value.parse().unwrap_or(0),
                "dns" => conf.dns = split_list(&value),
                "table" => conf.table = value,
                _ => {}
            },
            "peer" => {
                if let Some(p) = peer.as_mut() {
                    match key.as_str() {
                        "publickey" => p.public_key = value,
                        "presharedkey" => p.preshared_key = value,
                        "endpoint" => p.endpoint = value,
                        "allowedips" => p.allowed_ips = split_list(&value),
                        "persistentkeepalive" => p.keepalive = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(p) = peer.take() {
        conf.peers.push(p);
    }

    if conf.private_key.is_empty() {
        return Err("no PrivateKey: the tunnel could not come up".into());
    }
    if conf.peers.iter().any(|p| p.public_key.is_empty()) {
        return Err("a [Peer] has no PublicKey".into());
    }
    Ok(conf)
}

/// `wg setconf` input: only the keys wg itself understands.
pub fn setconf_text(conf: &Conf) -> String {
    let mut out = String::from("[Interface]\n");
    out.push_str(&format!("PrivateKey = {}\n", conf.private_key));
    if conf.listen_port > 0 {
        out.push_str(&format!("ListenPort = {}\n", conf.listen_port));
    }
    if conf.fwmark > 0 {
        out.push_str(&format!("FwMark = {}\n", conf.fwmark));
    }
    for p in &conf.peers {
        out.push_str("\n[Peer]\n");
        out.push_str(&format!("PublicKey = {}\n", p.public_key));
        if !p.preshared_key.is_empty() {
            out.push_str(&format!("PresharedKey = {}\n", p.preshared_key));
        }
        if !p.endpoint.is_empty() {
            out.push_str(&format!("Endpoint = {}\n", p.endpoint));
        }
        if !p.allowed_ips.is_empty() {
            out.push_str(&format!("AllowedIPs = {}\n", p.allowed_ips.join(", ")));
        }
        if p.keepalive > 0 {
            out.push_str(&format!("PersistentKeepalive = {}\n", p.keepalive));
        }
    }
    out
}

/// The same configuration with every secret removed.
///
/// For anything that leaves this process: an API answer, a log line, a support
/// paste. A WireGuard private key is the tunnel; publishing one hands it over.
pub fn redacted(conf: &Conf) -> Conf {
    let mut out = conf.clone();
    if !out.private_key.is_empty() {
        out.private_key = "[redacted]".into();
    }
    for p in out.peers.iter_mut() {
        if !p.preshared_key.is_empty() {
            p.preshared_key = "[redacted]".into();
        }
    }
    out
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// FwMark accepts decimal and 0x-prefixed hex, as wg-quick does.
fn parse_fwmark(v: &str) -> u32 {
    let v = v.trim();
    if let Some(hex) = v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).unwrap_or(0)
    } else {
        v.parse().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[Interface]
PrivateKey = aPrivateKeyValue=
Address = 10.2.0.2/32, fd00::2/128
DNS = 10.2.0.1
MTU = 1420
Table = off
FwMark = 0x1234

[Peer]
PublicKey = aPublicKeyValue=
PresharedKey = aPresharedKeyValue=
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = 1.2.3.4:51820
PersistentKeepalive = 25
";

    #[test]
    fn a_provider_config_is_read_as_handed_out() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.addresses, ["10.2.0.2/32", "fd00::2/128"]);
        assert_eq!(c.fwmark, 0x1234, "FwMark accepts hex, as wg-quick does");
        assert_eq!(c.table, "off", "read, and deliberately not applied");
        assert_eq!(c.peers.len(), 1);
        assert_eq!(c.peers[0].keepalive, 25);
    }

    /// Handing wg a file containing `Address =` makes it refuse the whole
    /// thing, so the wg-quick-only keys must not reach it.
    #[test]
    fn setconf_carries_only_what_wg_understands() {
        let out = setconf_text(&parse(SAMPLE).unwrap());
        for rejected in ["Address", "DNS", "MTU", "Table"] {
            assert!(!out.contains(rejected), "{rejected} would make wg refuse the file:\n{out}");
        }
        assert!(out.contains("PrivateKey = "));
        assert!(out.contains("AllowedIPs = 0.0.0.0/0, ::/0"));
    }

    /// A private key is the tunnel. Anything leaving the process must not
    /// carry one.
    #[test]
    fn redaction_removes_every_secret_and_keeps_the_rest() {
        let r = redacted(&parse(SAMPLE).unwrap());
        assert_eq!(r.private_key, "[redacted]");
        assert_eq!(r.peers[0].preshared_key, "[redacted]");
        assert_eq!(r.peers[0].public_key, "aPublicKeyValue=", "a public key is public");
        assert_eq!(r.peers[0].endpoint, "1.2.3.4:51820");
    }

    #[test]
    fn the_two_failures_that_would_be_silent_are_refused_early() {
        // No private key: the tunnel simply never comes up, with nothing
        // saying why.
        assert!(parse("[Interface]\nAddress = 10.0.0.1/32\n").is_err());
        // A peer with no public key: wg rejects it later, at setconf.
        let no_pub = "[Interface]\nPrivateKey = k\n\n[Peer]\nEndpoint = 1.2.3.4:1\n";
        assert!(parse(no_pub).is_err());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let c = parse("# a comment\n[Interface]\nPrivateKey = k # trailing\n\n").unwrap();
        assert_eq!(c.private_key, "k");
    }
}
