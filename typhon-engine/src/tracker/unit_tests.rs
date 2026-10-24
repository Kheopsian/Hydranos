//! Decisions in `tracker` that were only ever exercised through a whole
//! announce or a whole dial: which URLs hold a secret in the clear, which
//! binding a peer is dialled from, what a full dial queue does, and the proxy
//! helpers whose failure mode is announcing without the proxy.

use super::*;
use crate::config::ResolvedBinding;
use std::net::SocketAddr;

mod key_in_clear {
    use super::*;

    /// A passkey in the path or in a query value is a secret on a plain-text
    /// connection. The query parameter's NAME is not what decides it.
    #[test]
    fn a_key_in_the_path_or_a_query_value_of_an_http_url_is_seen() {
        for url in [
            "http://tracker.example/a1b2c3d4e5f6a7b8c9d0/announce",
            "http://tracker.example/announce/a1b2c3d4e5f6a7b8c9d0",
            "http://tracker.example/announce?passkey=a1b2c3d4e5f6a7b8c9d0", // leak-ok: a placeholder key
            "http://tracker.example/announce?x=1&whatever=a1b2c3d4e5f6a7b8c9d0",
            "HTTP://tracker.example/announce?k=a1b2c3d4e5f6a7b8c9d0",
        ] {
            assert!(
                carries_key_in_clear(url),
                "{url} carries a key over plain http"
            );
        }
    }

    /// Over TLS the same URL is fine; and a plain announce with nothing that
    /// looks like a key is fine too.
    #[test]
    fn only_a_plain_http_url_can_carry_a_key_in_clear() {
        let key = "a1b2c3d4e5f6a7b8c9d0";
        assert!(!carries_key_in_clear(&format!(
            "https://tracker.example/{key}/announce"
        )));
        assert!(!carries_key_in_clear(&format!(
            "udp://tracker.example:80/{key}"
        )));
        assert!(
            !carries_key_in_clear(&format!("tracker.example/{key}")),
            "no scheme, no http"
        );
        for url in [
            "http://tracker.example/announce",
            "http://tracker.example/announce.php",
            "http://tracker.example/public-tracker-announce",
            "http://tracker.example",
            "http://tracker.example/announce?info=1&compact=1",
            "",
            "http:/",
        ] {
            assert!(!carries_key_in_clear(url), "{url:?} holds no key");
        }
    }

    /// A key is 16 characters or more, with a digit and a letter. 15 is a word,
    /// all digits is a number, all letters is a word: none of them is a
    /// passkey, and flagging them would refuse honest trackers.
    #[test]
    fn a_key_needs_sixteen_characters_a_digit_and_a_letter() {
        let url = |seg: &str| format!("http://tracker.example/{seg}/announce");
        assert!(
            carries_key_in_clear(&url("abcdefgh12345678")),
            "16 characters is a key"
        );
        assert!(
            !carries_key_in_clear(&url("abcdefgh1234567")),
            "15 characters is not"
        );
        assert!(
            !carries_key_in_clear(&url("1234567890123456")),
            "digits only is not a key"
        );
        assert!(
            !carries_key_in_clear(&url("abcdefghijklmnop")),
            "letters only is not a key"
        );
        assert!(
            carries_key_in_clear(&url("abcdefgh-234_678")),
            "dash and underscore belong to a key"
        );
        assert!(
            !carries_key_in_clear(&url("abcdefgh.2345678")),
            "a dot does not"
        );
    }

    /// The host is not a path segment: a tracker whose name happens to look
    /// like a key is not leaking one.
    #[test]
    fn a_host_that_looks_like_a_key_is_not_a_key() {
        assert!(!carries_key_in_clear("http://abcdefgh12345678/announce"));
        assert!(!carries_key_in_clear("http://abcdefgh12345678"));
    }
}

mod dial_binding {
    use super::*;

    fn binding(id: u32) -> ResolvedBinding {
        ResolvedBinding {
            id,
            addr: "0.0.0.0:6881".parse().unwrap(),
            peer_id: [id as u8; 20],
            egress: Default::default(),
            advertised_port: 6881,
            only_v6: false,
        }
    }

    fn peer(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().unwrap(), port)
    }

    /// With no binding at all the dial still has something to leave from: a
    /// wildcard with a zero peer id, which the caller has already warned about.
    #[test]
    fn no_binding_gives_the_wildcard_one() {
        let b = pick_binding_for_dial(&[], peer("192.0.2.1", 1));
        assert_eq!((b.id, b.peer_id, b.advertised_port), (0, [0u8; 20], 0));
        assert!(
            b.addr.ip().is_unspecified() && b.addr.port() == 0,
            "{}",
            b.addr
        );
    }

    /// One binding is every peer's binding, whatever its address.
    #[test]
    fn a_single_binding_serves_every_peer() {
        let only = [binding(5)];
        for ip in ["192.0.2.1", "198.51.100.200", "2001:db8::1"] {
            assert_eq!(pick_binding_for_dial(&only, peer(ip, 6881)).id, 5);
        }
    }

    /// A peer is always dialled from the same binding, so it always sees the
    /// same peer id from us -- and the port does not matter, only the host.
    #[test]
    fn a_peer_keeps_its_binding_whatever_its_port() {
        let all = [binding(1), binding(2), binding(3)];
        for ip in ["192.0.2.1", "198.51.100.7", "2001:db8::42"] {
            let first = pick_binding_for_dial(&all, peer(ip, 6881)).id;
            for port in [1, 6881, 51413, 65535] {
                assert_eq!(
                    pick_binding_for_dial(&all, peer(ip, port)).id,
                    first,
                    "{ip}:{port}"
                );
            }
        }
    }

    /// Different peers spread over the bindings; hashing nothing would put them
    /// all on one tunnel.
    #[test]
    fn peers_spread_over_every_binding() {
        let all = [binding(1), binding(2), binding(3)];
        let mut used = std::collections::HashSet::new();
        for n in 0..=255u8 {
            used.insert(pick_binding_for_dial(&all, peer(&format!("192.0.2.{n}"), 6881)).id);
        }
        assert_eq!(used.len(), 3, "bindings used: {used:?}");
    }

    /// Pinned values: the choice is part of what a peer remembers about us, so
    /// a change of hash is a change of identity for every peer already
    /// connected. FNV-1a over the address octets, modulo the binding count.
    #[test]
    fn the_choice_is_fnv_1a_of_the_address() {
        let all = [binding(0), binding(1), binding(2), binding(3)];
        let fnv = |octets: &[u8]| {
            let mut h: u32 = 0x811c9dc5;
            for b in octets {
                h ^= *b as u32;
                h = h.wrapping_mul(0x01000193);
            }
            h
        };
        for n in [1u8, 2, 3, 77, 200] {
            let want = fnv(&[192, 0, 2, n]) as usize % 4;
            assert_eq!(
                pick_binding_for_dial(&all, peer(&format!("192.0.2.{n}"), 9)).id as usize,
                want
            );
        }
        let v6: std::net::Ipv6Addr = "2001:db8::42".parse().unwrap();
        let want = fnv(&v6.octets()) as usize % 4;
        assert_eq!(
            pick_binding_for_dial(&all, SocketAddr::new(v6.into(), 9)).id as usize,
            want
        );
    }
}

mod dial_queue {
    use super::*;
    use crate::torrent::meta::TorrentMeta;

    fn torrent_on(limiter: &Arc<dial_limiter::DialLimiter>, n: u8) -> Arc<TorrentState> {
        let t = Arc::new(TorrentState::new(
            TorrentMeta {
                info_hash: [n; 20],
                name: "t".into(),
                num_pieces: 1,
                piece_length: 16384,
                total_size: 16384,
                files: Vec::new(),
                trackers: Vec::new(),
                url_list: Vec::new(),
                private: false,
                multi_file: false,
                info_dict_len: 0,
                v2: false,
                renames: Default::default(),
            },
            std::path::PathBuf::from("/tmp"),
            false,
        ));
        let _ = t.limiter.set(limiter.clone());
        t
    }

    /// An engine whose dial consumer is gone loses its dials quietly: the
    /// caller (a tracker, PEX or DHT task) is not hurt, the dial does not
    /// fall through to another engine's queue, and the next engine goes on.
    #[tokio::test]
    async fn a_dead_consumer_loses_the_dial_and_nothing_else() {
        let (dead, other) = (
            Arc::new(dial_limiter::DialLimiter::default()),
            Arc::new(dial_limiter::DialLimiter::default()),
        );
        let (tx_dead, rx_dead) = tokio::sync::mpsc::channel(4);
        let (tx_other, mut rx_other) = tokio::sync::mpsc::channel(4);
        {
            let mut map = ENGINE_DIAL_TX.get_or_init(Default::default).lock().unwrap();
            map.insert(Arc::as_ptr(&dead) as usize, tx_dead);
            map.insert(Arc::as_ptr(&other) as usize, tx_other);
        }
        drop(rx_dead);
        let peer: SocketAddr = "192.0.2.12:6881".parse().unwrap();
        enqueue_dial(peer, torrent_on(&dead, 32));
        assert!(
            rx_other.try_recv().is_err(),
            "a dial of the dead engine reached another engine's queue"
        );
        enqueue_dial(peer, torrent_on(&other, 33));
        assert_eq!(
            rx_other.try_recv().unwrap().1.info_hash,
            [33u8; 20],
            "the live engine still dials"
        );
    }
}

mod proxy_helpers {
    use super::super::http::{announces_proxied, effective_proxy, proxy_for, redact_proxy};

    /// A proxy URL goes into logs and errors; its credentials never do.
    #[test]
    fn a_proxy_url_loses_its_credentials_when_shown() {
        assert_eq!(
            redact_proxy("socks5://user:secret@10.0.0.1:1080"),
            "socks5://***@10.0.0.1:1080"
        );
        assert_eq!(
            redact_proxy("http://tok@proxy.example:3128"),
            "http://***@proxy.example:3128"
        );
        // An '@' inside the password: everything before the LAST one is hidden.
        assert_eq!(
            redact_proxy("http://u:p@ss@proxy.example:3128"),
            "http://***@proxy.example:3128"
        );
        // Nothing to hide, nothing changed.
        assert_eq!(
            redact_proxy("http://proxy.example:3128"),
            "http://proxy.example:3128"
        );
        assert_eq!(redact_proxy("not a url"), "not a url");
    }

    /// What the engine was configured with wins, trimmed; an empty or blank
    /// setting is "not configured" and never becomes an empty proxy URL.
    #[test]
    fn a_configured_proxy_wins_and_is_trimmed() {
        assert_eq!(
            effective_proxy("  http://proxy.example:3128 \n").as_deref(),
            Some("http://proxy.example:3128")
        );
        assert!(announces_proxied("http://proxy.example:3128"));
        // With nothing configured the answer is the environment's, which the
        // test process leaves unset; blank must be treated as nothing.
        assert_eq!(effective_proxy("   "), effective_proxy(""));
        assert_eq!(announces_proxied("  "), announces_proxied(""));
    }

    /// A proxy URL that does not parse is an error naming the proxy (without
    /// its password), never "no proxy": announcing directly instead would
    /// publish the address the operator set the proxy up to hide.
    #[test]
    fn an_unusable_proxy_is_an_error_not_a_direct_announce() {
        let err = proxy_for("http://user:hunter2@[::1")
            .expect_err("a broken proxy URL must not become no proxy");
        assert!(err.contains("announce proxy"), "{err}");
        assert!(
            !err.contains("hunter2"),
            "the password is in the error: {err}"
        );
        assert!(err.contains("***@"), "{err}");
        assert!(proxy_for("http://proxy.example:3128").unwrap().is_some());
    }
}

mod char_boundary {
    use super::super::udp::is_udp;

    /// A tracker URL is judged by its first six bytes without cutting a
    /// character in two: a non-ASCII character can straddle the offset.
    #[test]
    fn a_non_ascii_url_is_not_udp_and_is_judged_whole() {
        for u in [
            "abcde\u{e9}://x",
            "udp\u{e9}://x",
            "ud\u{e9}p://x",
            "\u{e9}\u{e9}\u{e9}\u{e9}",
        ] {
            assert!(!is_udp(u), "{u:?}");
        }
        assert!(
            is_udp("udp://t.example:80") && is_udp("UDP://t.example:80"),
            "the ascii cases are unchanged"
        );
        assert!(!is_udp("udp://"), "the bare scheme is no tracker");
    }
}
