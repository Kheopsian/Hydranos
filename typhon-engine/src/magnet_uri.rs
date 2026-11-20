//! Reading a `magnet:?` link.
//!
//! Lives in the engine crate, next to `magnet` (which resolves one), so the
//! fuzz crate can reach it; `hydra/magnets.rs` re-exports it.

use std::net::SocketAddr;

/// A magnet link, read.
#[derive(Debug, Clone, PartialEq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: String,
    pub trackers: Vec<String>,
    pub peers: Vec<SocketAddr>,
}

fn hex_nibble(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `%XX` and `+`, as a query string encodes them.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex_nibble(b[i + 1]), hex_nibble(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// RFC 4648 base32, which a `btih` may use instead of hex (32 characters).
fn base32_20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 20];
    let mut acc: u64 = 0;
    let mut bits = 0;
    let mut n = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
            acc &= (1 << bits) - 1;
        }
    }
    (n == 20).then_some(out)
}

fn hex_20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, p) in s.as_bytes().chunks(2).enumerate() {
        out[i] = hex_nibble(p[0])? * 16 + hex_nibble(p[1])?;
    }
    Some(out)
}

/// Read a `magnet:?` link.
///
/// Needs a v1 info hash (`xt=urn:btih:`). A magnet carrying only a v2 one
/// (`urn:btmh:`) is refused by name: resolving by a v2 hash needs the v2
/// metadata exchange, and answering "added" to something that will never
/// resolve would be worse than saying so. A hybrid magnet has both and is fine.
pub fn parse(uri: &str) -> Result<Magnet, String> {
    let uri = uri.trim();
    let query = match uri.get(..8) {
        Some(p) if p.eq_ignore_ascii_case("magnet:?") => &uri[8..],
        _ => return Err("not a magnet link (magnet:?...)".into()),
    };
    let mut info_hash = None;
    let mut v2_only = false;
    let mut m = Magnet {
        info_hash: [0; 20],
        name: String::new(),
        trackers: Vec::new(),
        peers: Vec::new(),
    };
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        // `tr.1`, `xt.2`: numbered repeats, same meaning.
        let key = k.split('.').next().unwrap_or(k).to_ascii_lowercase();
        let v = percent_decode(v);
        match key.as_str() {
            "xt" => {
                let lower = v.to_ascii_lowercase();
                if let Some(h) = lower.strip_prefix("urn:btih:") {
                    let raw = &v[9..];
                    info_hash = hex_20(h).or_else(|| base32_20(raw));
                    if info_hash.is_none() {
                        return Err(format!("{v:?} is not a 40-hex or 32-base32 info hash"));
                    }
                } else if lower.starts_with("urn:btmh:") {
                    v2_only = true;
                }
            }
            "dn" => m.name = v,
            "tr" => {
                if !v.is_empty() && !m.trackers.contains(&v) {
                    m.trackers.push(v);
                }
            }
            "x" if k.eq_ignore_ascii_case("x.pe") => {
                if let Ok(a) = v.parse() {
                    m.peers.push(a);
                }
            }
            _ => {}
        }
    }
    match info_hash {
        Some(h) => m.info_hash = h,
        None if v2_only => {
            return Err(
                "this magnet has only a v2 info hash (btmh); add the .torrent file instead".into(),
            )
        }
        None => return Err("the magnet has no info hash (xt=urn:btih:...)".into()),
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::magnet::hex;

    const H: &str = "c9e15763f722f23e98a29decdfae341b98d53056";

    #[test]
    fn a_magnet_gives_its_hash_name_trackers_and_peers() {
        let m = parse(&format!(
            "magnet:?xt=urn:btih:{H}&dn=Some+Book%20%28EPUB%29&tr=udp%3A%2F%2Ft.example%3A1337%2Fannounce\
             &tr.1=https%3A%2F%2Ft2.example%2Fannounce&tr=udp%3A%2F%2Ft.example%3A1337%2Fannounce&x.pe=10.0.0.1:6881"
        ))
        .unwrap();
        assert_eq!(hex(&m.info_hash), H);
        assert_eq!(m.name, "Some Book (EPUB)");
        assert_eq!(
            m.trackers,
            vec![
                "udp://t.example:1337/announce",
                "https://t2.example/announce"
            ],
            "deduplicated, in order"
        );
        assert_eq!(m.peers, vec!["10.0.0.1:6881".parse().unwrap()]);
    }

    /// Base32 is the other spelling of the same 20 bytes.
    #[test]
    fn a_base32_info_hash_is_the_same_hash() {
        let b32 = "ZHQVOY7XELZD5GFCTXWN7LRUDOMNKMCW";
        assert_eq!(
            hex(&parse(&format!("magnet:?xt=urn:btih:{b32}"))
                .unwrap()
                .info_hash),
            H
        );
        assert_eq!(
            hex(&parse(&format!("MAGNET:?xt=urn:btih:{}", H.to_uppercase()))
                .unwrap()
                .info_hash),
            H,
            "case does not matter"
        );
    }

    #[test]
    fn what_is_not_a_usable_magnet_is_refused_by_name() {
        assert!(parse("http://example/x.torrent")
            .unwrap_err()
            .contains("not a magnet"));
        assert!(parse("magnet:?dn=nothing")
            .unwrap_err()
            .contains("no info hash"));
        assert!(parse("magnet:?xt=urn:btih:1234")
            .unwrap_err()
            .contains("not a 40-hex"));
        let v2 = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        assert!(parse(v2).unwrap_err().contains("v2"));
        let hybrid = format!("magnet:?xt=urn:btih:{H}&xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e");
        assert_eq!(
            hex(&parse(&hybrid).unwrap().info_hash),
            H,
            "a hybrid resolves by its v1 hash"
        );
    }
}
