//! Who is on the other end, read from the 20-byte peer id.
//!
//! There is no registry and no standard: BEP 20 describes a convention, most
//! clients follow it, and the rest each invented their own. This module holds
//! what a peer id can look like in one place, because the alternative is what
//! this codebase had -- two identical copies of a six-entry table, one of them
//! dead, and every peer that was not one of those six reported as raw bytes.
//!
//! Used for the Client column, for spotting seedboxes in `intel`, and to check
//! our OWN spoof: a client that claims to be qBittorrent should decode as one.

/// Azureus style: `-XX1234-` followed by 12 bytes of anything.
///
/// The two letters are the client and the four characters are its version. The
/// list is what actually shows up in a swarm, not everything ever written.
const AZUREUS: &[(&str, &str)] = &[
    ("7T", "aTorrent"),
    ("AB", "AnyEvent::BitTorrent"),
    ("AG", "Ares"),
    ("AT", "Artemis"),
    ("AX", "BitPump"),
    ("AZ", "Azureus"),
    ("BC", "BitComet"),
    ("BE", "BitTorrent SDK"),
    ("BF", "BitFlu"),
    ("BG", "BTG"),
    ("BL", "BitBlinder"),
    ("BP", "BitTorrent Pro"),
    ("BR", "BitRocket"),
    ("BT", "BitTorrent"),
    ("BW", "BitWombat"),
    ("BX", "BittorrentX"),
    ("CD", "Enhanced CTorrent"),
    ("CT", "CTorrent"),
    ("DE", "Deluge"),
    ("DP", "Propagate Data Client"),
    ("EB", "EBit"),
    ("ES", "electric sheep"),
    ("FC", "FileCroc"),
    ("FD", "Free Download Manager"),
    ("FT", "FoxTorrent"),
    ("FX", "Freebox BitTorrent"),
    ("GS", "GSTorrent"),
    ("HK", "Hekate"),
    ("HL", "Halite"),
    ("HN", "Hydranode"),
    ("IL", "iLivid"),
    ("KG", "KGet"),
    ("KT", "KTorrent"),
    ("LC", "LeechCraft"),
    ("LH", "LH-ABC"),
    ("LT", "libtorrent"),
    ("LW", "LimeWire"),
    ("MO", "MonoTorrent"),
    ("MP", "MooPolice"),
    ("MR", "Miro"),
    ("MT", "MoonlightTorrent"),
    ("NE", "BT Next Evolution"),
    ("NX", "Net Transport"),
    ("OS", "OneSwarm"),
    ("OT", "OmegaTorrent"),
    ("PB", "Protocol::BitTorrent"),
    ("PD", "Pando"),
    ("PI", "PicoTorrent"),
    ("PT", "PHPTracker"),
    ("qB", "qBittorrent"),
    ("QD", "QQDownload"),
    ("RT", "Retriever"),
    ("RZ", "RezTorrent"),
    ("SD", "Xunlei"),
    ("SM", "SoMud"),
    ("SP", "BitSpirit"),
    ("SS", "SwarmScope"),
    ("ST", "SymTorrent"),
    ("st", "sharktorrent"),
    ("SZ", "Shareaza"),
    ("TB", "Torch"),
    ("TE", "terasaur Seed Bank"),
    ("TL", "Tribler"),
    ("TN", "TorrentDotNET"),
    ("TR", "Transmission"),
    ("TS", "Torrentstorm"),
    ("TT", "TuoTu"),
    ("UL", "uLeecher!"),
    ("UM", "uTorrent Mac"),
    ("UT", "uTorrent"),
    ("UW", "uTorrent Web"),
    ("VG", "Vagaa"),
    ("WD", "WebTorrent Desktop"),
    ("WT", "BitLet"),
    ("WW", "WebTorrent"),
    ("WY", "FireTorrent"),
    ("XF", "Xfplay"),
    ("XL", "Xunlei"),
    ("XS", "XSwifter"),
    ("XT", "XanTorrent"),
    ("XX", "Xtorrent"),
    ("ZT", "ZipTorrent"),
    ("BI", "BiglyBT"),
    ("TX", "Tixati"),
    ("HY", "Hydranos"),
    ("lt", "libTorrent"),
];

/// Shadow style: one letter, then three version characters, then `---`.
/// Predates the Azureus convention and a few clients never moved.
const SHADOW: &[(u8, &str)] = &[
    (b'A', "ABC"),
    (b'O', "Osprey Permaseed"),
    (b'Q', "BTQueue"),
    (b'R', "Tribler"),
    (b'S', "Shadow"),
    (b'T', "BitTornado"),
    (b'U', "UPnP NAT Bit Torrent"),
];

/// How a family writes its four version characters.
enum Scheme {
    /// `5220` -> `5.2.2`. One decimal digit per component, trailing zeros cut.
    Digits,
    /// `0D80` -> `0.13.8`. libtorrent writes each component in base 16.
    Hex,
    /// `3000` -> `3.00`. Transmission's major, then a two-digit minor.
    Transmission,
    /// Base 36 per component, which is how a component above nine fits in one
    /// character. Ours, and anyone else who outgrew a single digit.
    Base36,
}

fn scheme_for(code: &str) -> Scheme {
    match code {
        "LT" | "lt" => Scheme::Hex,
        "TR" => Scheme::Transmission,
        "HY" => Scheme::Base36,
        _ => Scheme::Digits,
    }
}

fn digit(c: u8, radix: u32) -> Option<u32> {
    (c as char).to_digit(radix)
}

/// The version part, rendered the way its own family writes it. Falls back to
/// the raw characters rather than inventing something when they do not parse.
fn version(code: &str, raw: &str) -> String {
    let b = raw.as_bytes();
    if b.len() != 4 {
        return raw.to_string();
    }
    let out = match scheme_for(code) {
        Scheme::Hex => (0..4)
            .map(|i| digit(b[i], 16).map(|v| v.to_string()))
            .collect::<Option<Vec<_>>>(),
        Scheme::Base36 => (0..4)
            .map(|i| digit(b[i], 36).map(|v| v.to_string()))
            .collect::<Option<Vec<_>>>(),
        Scheme::Transmission => digit(b[0], 10).and_then(|maj| {
            let min: String = raw[1..3].to_string();
            min.parse::<u32>().ok().map(|_| vec![maj.to_string(), min])
        }),
        Scheme::Digits => (0..4)
            .map(|i| digit(b[i], 10).map(|v| v.to_string()))
            .collect::<Option<Vec<_>>>(),
    };
    let Some(mut parts) = out else { return raw.to_string() };
    // Trailing zeros carry no information: 5.2.2.0 is 5.2.2.
    while parts.len() > 2 && parts.last().map(|s| s == "0").unwrap_or(false) {
        parts.pop();
    }
    parts.join(".")
}

/// Best-effort identification. Empty when the peer id follows no convention we
/// know -- which is a real answer, not a failure: a client is free to send 20
/// random bytes, and many do.
pub fn identify(pid: &[u8; 20]) -> String {
    // Azureus: -XX1234-
    if pid[0] == b'-' && pid[7] == b'-' {
        if let (Ok(code), Ok(raw)) = (
            std::str::from_utf8(&pid[1..3]),
            std::str::from_utf8(&pid[3..7]),
        ) {
            let name = AZUREUS
                .iter()
                .find(|(c, _)| *c == code)
                .map(|(_, n)| *n)
                .unwrap_or(code);
            return format!("{} {}", name, version(code, raw));
        }
    }
    // Shadow: one letter, three version characters, then dashes.
    if pid[4] == b'-' && pid[5] == b'-' && pid[6] == b'-' {
        if let Some((_, name)) = SHADOW.iter().find(|(c, _)| *c == pid[0]) {
            let ver: Vec<String> = pid[1..4]
                .iter()
                .filter_map(|c| digit(*c, 36).map(|v| v.to_string()))
                .collect();
            if ver.len() == 3 {
                return format!("{} {}", name, ver.join("."));
            }
            return (*name).to_string();
        }
    }
    // Mainline: `M4-20-8--`, dashes between the components themselves.
    if pid[0] == b'M' && pid[2] == b'-' {
        if let Ok(s) = std::str::from_utf8(&pid[1..8]) {
            let ver: Vec<&str> = s.split('-').filter(|p| !p.is_empty()).collect();
            if !ver.is_empty() {
                return format!("Mainline {}", ver.join("."));
            }
        }
    }
    // The handful that ignore every convention and put their name up front.
    for (magic, name) in [
        (&b"exbc"[..], "BitComet"),
        (&b"FUTB"[..], "BitComet Mod"),
        (&b"xUTB"[..], "BitComet Mod"),
        (&b"OP"[..], "Opera"),
        (&b"XBT"[..], "XBT Client"),
        (&b"-BOW"[..], "Bits on Wheels"),
        (&b"Plus"[..], "Plus!"),
        (&b"LIME"[..], "LimeWire"),
        (&b"btpd"[..], "BT Protocol Daemon"),
        (&b"QVOD"[..], "QVOD"),
    ] {
        if pid.starts_with(magic) {
            return name.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(s: &str) -> [u8; 20] {
        let mut out = [b'0'; 20];
        let b = s.as_bytes();
        out[..b.len().min(20)].copy_from_slice(&b[..b.len().min(20)]);
        out
    }

    /// The six the old table knew, still right.
    #[test]
    fn the_common_clients_still_decode() {
        assert_eq!(identify(&pid("-qB5220-abcdefghijkl")), "qBittorrent 5.2.2");
        assert_eq!(identify(&pid("-UT3550-abcdefghijkl")), "uTorrent 3.5.5");
        assert_eq!(identify(&pid("-DE2200-abcdefghijkl")), "Deluge 2.2");
    }

    /// The ones it did not: every peer outside its six showed raw bytes.
    #[test]
    fn the_clients_the_old_table_missed() {
        assert_eq!(identify(&pid("-BI3400-abcdefghijkl")), "BiglyBT 3.4");
        assert_eq!(identify(&pid("-TX2600-abcdefghijkl")), "Tixati 2.6");
        assert_eq!(identify(&pid("-PI1000-abcdefghijkl")), "PicoTorrent 1.0");
        assert_eq!(identify(&pid("-WW0100-abcdefghijkl")), "WebTorrent 0.1");
        assert_eq!(identify(&pid("-KT3000-abcdefghijkl")), "KTorrent 3.0");
    }

    /// Transmission writes a two-digit minor: 3000 is 3.00, not 3.0.0.0.
    #[test]
    fn transmission_has_its_own_scheme() {
        assert_eq!(identify(&pid("-TR3000-abcdefghijkl")), "Transmission 3.00");
    }

    /// libtorrent writes each component in base 16. The real peer id this
    /// production node saw was `lt 0D80`, which is 0.13.8 -- not "0D80".
    #[test]
    fn libtorrent_is_hexadecimal() {
        assert_eq!(identify(&pid("-lt0D80-abcdefghijkl")), "libTorrent 0.13.8");
        assert_eq!(identify(&pid("-LT0D60-abcdefghijkl")), "libtorrent 0.13.6");
    }

    /// Ours, base 36, so a minor above nine still fits in one character.
    #[test]
    fn our_own_id_decodes_to_our_own_version() {
        assert_eq!(identify(&pid("-HY4Q00-abcdefghijkl")), "Hydranos 4.26");
    }

    /// Older conventions the previous table ignored completely.
    #[test]
    fn the_pre_azureus_conventions_are_read() {
        assert_eq!(identify(&pid("T03C---abcdefghijklm")), "BitTornado 0.3.12");
        assert!(identify(&pid("M4-20-8--abcdefghijk")).starts_with("Mainline 4.20"));
        assert_eq!(identify(&pid("exbc0100abcdefghijkl")), "BitComet");
    }

    /// An unknown two-letter code still says something useful rather than
    /// nothing: the code itself, which is what a human would look up.
    #[test]
    fn an_unknown_code_falls_back_to_the_code() {
        assert_eq!(identify(&pid("-ZZ1234-abcdefghijkl")), "ZZ 1.2.3.4");
    }

    /// A peer id following no convention is not an error. Many clients send
    /// 20 random bytes, and claiming to recognise them would be worse.
    #[test]
    fn random_bytes_identify_as_nothing() {
        assert_eq!(identify(&[0x7f; 20]), "");
    }
}
