//! Conformance of the peer-to-peer half of the protocol.
//!
//! The tracker half lives in `bep_conformance.rs`. This file covers what two
//! clients say to each other: the handshake, the message set, the framing that
//! carries it, and the one rule a private tracker will ban an account over --
//! BEP 27, a private torrent looks for peers nowhere but its own trackers.
//!
//! References:
//!   BEP 3  -- the peer wire protocol: handshake, message ids, framing
//!   BEP 6  -- the Fast Extension
//!   BEP 10 -- the extension protocol
//!   BEP 27 -- private torrents
//!
//! Assertions are on bytes and on offsets taken from the specifications, not
//! on our own constants: a test that reads `MSG_HAVE` to check that `have` is
//! `MSG_HAVE` cannot fail, and would not have caught the id being wrong.

use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use typhon_engine::peer::handshake::{build_handshake, parse_handshake};
use typhon_engine::peer::message::Message;
use typhon_engine::torrent::metainfo::parse_torrent_bytes;
use typhon_engine::wire::codec::BtCodec;

const IH: [u8; 20] = [0xAB; 20];
const PID: [u8; 20] = *b"-TY4R00-abcdefghijkl";

// ---------------------------------------------------------------------------
// BEP 3 -- the handshake
// ---------------------------------------------------------------------------

/// BEP 3: the handshake is `<pstrlen><pstr><reserved><info_hash><peer_id>`,
/// which for pstr = "BitTorrent protocol" is exactly 68 bytes. The far side
/// reads that many and no more, so a handshake of any other length leaves both
/// ends waiting on each other.
#[test]
fn bep3_the_handshake_is_sixty_eight_bytes() {
    assert_eq!(build_handshake(&IH, &PID).len(), 68);
}

/// BEP 3: pstrlen is 19 and pstr is the ASCII string "BitTorrent protocol".
#[test]
fn bep3_the_protocol_string_is_the_one_the_spec_names() {
    let h = build_handshake(&IH, &PID);
    assert_eq!(h[0], 19, "pstrlen");
    assert_eq!(&h[1..20], b"BitTorrent protocol");
}

/// BEP 3 gives the offsets: 8 reserved bytes at 20, the info hash at 28, the
/// peer id at 48. Everything downstream indexes from these.
#[test]
fn bep3_the_info_hash_and_peer_id_sit_where_the_spec_puts_them() {
    let h = build_handshake(&IH, &PID);
    assert_eq!(&h[28..48], &IH[..], "info hash at offset 28");
    assert_eq!(&h[48..68], &PID[..], "peer id at offset 48");
}

/// BEP 6 claims reserved[7] & 0x04, BEP 10 claims reserved[5] & 0x10. Nothing
/// else is claimed: advertising an extension we do not implement makes a peer
/// wait for messages that never come.
#[test]
fn the_reserved_bytes_claim_fast_and_extended_and_nothing_more() {
    let h = build_handshake(&IH, &PID);
    let reserved = &h[20..28];
    assert_eq!(reserved[7], 0x04, "BEP 6 Fast Extension");
    assert_eq!(reserved[5], 0x10, "BEP 10 Extension Protocol");
    for (i, b) in reserved.iter().enumerate() {
        if i != 5 && i != 7 {
            assert_eq!(*b, 0, "reserved[{i}] claims an extension we do not implement");
        }
    }
}

/// What we write, we can read.
#[test]
fn bep3_a_handshake_round_trips() {
    let r = parse_handshake(&build_handshake(&IH, &PID)).expect("our own handshake parses");
    assert_eq!(r.info_hash, IH);
    assert_eq!(r.peer_id, PID);
    assert!(r.fast_extension, "we claimed BEP 6");
    assert!(r.extended_protocol, "we claimed BEP 10");
}

/// BEP 3: a handshake that does not open with the protocol string is not a
/// BitTorrent handshake. Reading on anyway is how a client talks to whatever
/// answered the port.
#[test]
fn bep3_a_foreign_protocol_string_is_refused() {
    let mut h = build_handshake(&IH, &PID);
    h[1..20].copy_from_slice(b"NotTorrent protoco!");
    assert!(parse_handshake(&h).is_err());

    let mut h = build_handshake(&IH, &PID);
    h[0] = 20; // wrong pstrlen
    assert!(parse_handshake(&h).is_err());
}

/// A peer claiming no extensions is read as claiming none -- we must not
/// assume our own reserved bits came back.
#[test]
fn a_peer_claiming_no_extension_is_read_as_claiming_none() {
    let mut h = build_handshake(&IH, &PID);
    h[20..28].fill(0);
    let r = parse_handshake(&h).expect("a bare handshake is still valid");
    assert!(!r.fast_extension);
    assert!(!r.extended_protocol);
}

// ---------------------------------------------------------------------------
// BEP 3 -- the message set
// ---------------------------------------------------------------------------

/// BEP 3 numbers the core messages 0 to 8, in this order. These are wire
/// constants: a client using different numbers is talking to nobody.
#[test]
fn bep3_the_core_message_ids_are_the_numbers_the_spec_gives() {
    let expected: [(u8, Message); 5] = [
        (0, Message::Choke),
        (1, Message::Unchoke),
        (2, Message::Interested),
        (3, Message::NotInterested),
        (4, Message::Have { piece: 7 }),
    ];
    for (id, msg) in expected {
        assert_eq!(msg.encode_payload()[0], id, "{msg:?} is message id {id}");
    }
    assert_eq!(Message::Bitfield { data: Bytes::new() }.encode_payload()[0], 5);
    assert_eq!(Message::Request { index: 0, begin: 0, length: 0 }.encode_payload()[0], 6);
    assert_eq!(Message::Piece { index: 0, begin: 0, data: Bytes::new() }.encode_payload()[0], 7);
    assert_eq!(Message::Cancel { index: 0, begin: 0, length: 0 }.encode_payload()[0], 8);
}

/// BEP 3: `have` carries a 4-byte big-endian piece index; `request` and
/// `cancel` carry index, begin and length, in that order, 4 bytes each.
#[test]
fn bep3_the_core_messages_round_trip_through_the_wire() {
    for msg in [
        Message::Choke,
        Message::Unchoke,
        Message::Interested,
        Message::NotInterested,
        Message::Have { piece: 0x0102_0304 },
        Message::Request { index: 1, begin: 16384, length: 16384 },
        Message::Cancel { index: 9, begin: 32768, length: 16384 },
    ] {
        let back = round_trip(&msg);
        assert_eq!(format!("{back:?}"), format!("{msg:?}"), "{msg:?} did not survive");
    }
}

/// BEP 3: a 4-byte index is big-endian. Sending it little-endian asks for a
/// piece that does not exist, and the peer answers nothing.
#[test]
fn bep3_a_piece_index_is_big_endian() {
    let payload = Message::Have { piece: 1 }.encode_payload();
    assert_eq!(&payload[1..5], &[0, 0, 0, 1], "big-endian, high byte first");
}

/// BEP 3: `piece` is index, begin, then the block. The block is whatever
/// remains -- its length is the frame's, not a field.
#[test]
fn bep3_a_piece_carries_its_index_begin_and_block() {
    let block = Bytes::from_static(b"sixteen kib, honest");
    let back = round_trip(&Message::Piece { index: 3, begin: 16384, data: block.clone() });
    match back {
        Message::Piece { index, begin, data } => {
            assert_eq!((index, begin), (3, 16384));
            assert_eq!(data, block, "the block arrives byte for byte");
        }
        other => panic!("a piece decoded as {other:?}"),
    }
}

/// BEP 3: a bitfield is opaque bytes, high bit of the first byte being piece 0.
/// Nothing in the codec may reinterpret it.
#[test]
fn bep3_a_bitfield_is_passed_through_untouched() {
    let bits = Bytes::from_static(&[0b1010_0000, 0x00, 0xFF]);
    match round_trip(&Message::Bitfield { data: bits.clone() }) {
        Message::Bitfield { data } => assert_eq!(data, bits),
        other => panic!("a bitfield decoded as {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// BEP 6 -- the Fast Extension
// ---------------------------------------------------------------------------

/// BEP 6 numbers its messages: suggest 13, have all 14, have none 15,
/// reject 16, allowed fast 17.
#[test]
fn bep6_the_fast_extension_ids_are_the_numbers_the_spec_gives() {
    assert_eq!(Message::Suggest { piece: 0 }.encode_payload()[0], 13);
    assert_eq!(Message::HaveAll.encode_payload()[0], 14);
    assert_eq!(Message::HaveNone.encode_payload()[0], 15);
    assert_eq!(Message::Reject { index: 0, begin: 0, length: 0 }.encode_payload()[0], 16);
    assert_eq!(Message::AllowedFast { piece: 0 }.encode_payload()[0], 17);
}

/// A reject echoes the request it refuses, field for field, so the requester
/// can match it to what it asked for.
#[test]
fn bep6_a_reject_echoes_the_request_it_refuses() {
    match round_trip(&Message::Reject { index: 4, begin: 16384, length: 16384 }) {
        Message::Reject { index, begin, length } => {
            assert_eq!((index, begin, length), (4, 16384, 16384));
        }
        other => panic!("a reject decoded as {other:?}"),
    }
}

/// `have all` and `have none` carry no payload: the id is the whole message.
#[test]
fn bep6_have_all_and_have_none_carry_no_payload() {
    assert_eq!(Message::HaveAll.encode_payload().len(), 1);
    assert_eq!(Message::HaveNone.encode_payload().len(), 1);
}

// ---------------------------------------------------------------------------
// BEP 10 -- the extension protocol
// ---------------------------------------------------------------------------

/// BEP 10: message id 20, then one byte of extended id, then the bencoded
/// payload. Extended id 0 is the extension handshake.
#[test]
fn bep10_an_extended_message_is_id_twenty_then_the_sub_id() {
    let payload = Bytes::from_static(b"d1:md6:ut_pexi1eee");
    let encoded = Message::Extended { ext_id: 0, payload: payload.clone() }.encode_payload();
    assert_eq!(encoded[0], 20, "BEP 10 rides on message id 20");
    assert_eq!(encoded[1], 0, "extended id 0 is the handshake");
    assert_eq!(&encoded[2..], &payload[..]);

    match round_trip(&Message::Extended { ext_id: 3, payload: payload.clone() }) {
        Message::Extended { ext_id, payload: back } => {
            assert_eq!(ext_id, 3);
            assert_eq!(back, payload);
        }
        other => panic!("an extended message decoded as {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// BEP 3 -- framing
// ---------------------------------------------------------------------------

/// BEP 3: every message is a 4-byte big-endian length prefix then the payload.
#[test]
fn bep3_the_length_prefix_is_four_bytes_big_endian() {
    let mut codec = BtCodec::new();
    let mut out = BytesMut::new();
    codec
        .encode(Message::Request { index: 1, begin: 2, length: 3 }, &mut out)
        .expect("encode");
    // 1 byte of id + 12 of payload
    assert_eq!(&out[0..4], &[0, 0, 0, 13], "length 13, high byte first");
    assert_eq!(out.len(), 17);
}

/// BEP 3: a keepalive is a length of zero and nothing else. It is how a peer
/// says it is still there without saying anything.
#[test]
fn bep3_a_keepalive_is_a_length_of_zero_and_no_payload() {
    let mut codec = BtCodec::new();
    let mut out = BytesMut::new();
    codec.encode(Message::KeepAlive, &mut out).expect("encode");
    assert_eq!(&out[..], &[0, 0, 0, 0]);

    let mut buf = BytesMut::from(&[0u8, 0, 0, 0][..]);
    assert!(matches!(
        codec.decode(&mut buf).expect("decode"),
        Some(Message::KeepAlive)
    ));
    assert!(buf.is_empty(), "the keepalive is consumed");
}

/// A length prefix is four bytes a peer controls. Trusting it means letting a
/// stranger ask for a four-gigabyte allocation; the frame is refused on its
/// header, before anything is reserved.
#[test]
fn an_oversized_frame_is_refused_on_its_header() {
    let mut codec = BtCodec::new();
    let mut buf = BytesMut::from(&[0xFFu8, 0xFF, 0xFF, 0xFF][..]);
    let err = codec.decode(&mut buf).expect_err("a 4 GiB frame is refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(buf.capacity() < 1 << 20, "nothing was reserved for it");
}

/// A frame that has not fully arrived yields nothing, and keeps its bytes. TCP
/// splits where it likes; a decoder that guessed here would lose a message.
#[test]
fn a_partial_frame_waits_instead_of_guessing() {
    let mut codec = BtCodec::new();
    // Announces 13 bytes, delivers 4.
    let mut buf = BytesMut::from(&[0u8, 0, 0, 13, 6, 0, 0, 0][..]);
    assert!(codec.decode(&mut buf).expect("decode").is_none());
    assert_eq!(buf.len(), 8, "the bytes are kept for the next read");
}

/// Two messages arriving in one read are two messages.
#[test]
fn two_messages_in_one_read_decode_separately() {
    let mut codec = BtCodec::new();
    let mut buf = BytesMut::new();
    codec.encode(Message::Interested, &mut buf).expect("encode");
    codec.encode(Message::Have { piece: 42 }, &mut buf).expect("encode");

    assert!(matches!(codec.decode(&mut buf).expect("decode"), Some(Message::Interested)));
    match codec.decode(&mut buf).expect("decode") {
        Some(Message::Have { piece }) => assert_eq!(piece, 42),
        other => panic!("expected a have, got {other:?}"),
    }
    assert!(codec.decode(&mut buf).expect("decode").is_none(), "and nothing more");
}

/// An id we do not know is carried, not fatal. BEP 3 tells a client to ignore
/// what it does not understand; dropping the connection instead would break
/// against every client that gains a message before we do.
#[test]
fn an_unknown_message_id_is_ignored_not_fatal() {
    let mut codec = BtCodec::new();
    let mut buf = BytesMut::from(&[0u8, 0, 0, 2, 200, 1][..]);
    match codec.decode(&mut buf).expect("an unknown id is not an error") {
        Some(Message::Unknown { id, .. }) => assert_eq!(id, 200),
        other => panic!("expected an unknown message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// BEP 27 -- private torrents
// ---------------------------------------------------------------------------

/// BEP 27: `private = 1` in the info dict means this torrent may use no peer
/// source but its own trackers.
#[test]
fn bep27_a_private_torrent_is_parsed_as_private() {
    let meta = parse_torrent_bytes(&torrent_bytes(Some(1))).expect("parses");
    assert!(meta.private, "private = 1 means private");
}

/// A torrent with no `private` key is public. Defaulting the other way would
/// quietly cut the DHT off for every public torrent.
#[test]
fn bep27_a_torrent_without_the_key_is_public() {
    let meta = parse_torrent_bytes(&torrent_bytes(None)).expect("parses");
    assert!(!meta.private, "absent means public");
}

/// BEP 27 defines the flag as `private = 1`. Any other value, zero included,
/// is not the flag.
#[test]
fn bep27_private_zero_is_not_private() {
    let meta = parse_torrent_bytes(&torrent_bytes(Some(0))).expect("parses");
    assert!(!meta.private);
}

/// The rule itself, and the reason this file exists: a private torrent must
/// look for peers nowhere but its trackers -- no DHT, no PEX, no local
/// discovery. Both the DHT registration and the peer session ask this one
/// question, so this test is what holds the rule for both.
#[test]
fn bep27_a_private_torrent_allows_no_peer_discovery() {
    let meta = parse_torrent_bytes(&torrent_bytes(Some(1))).expect("parses");
    assert!(
        !meta.allows_peer_discovery(),
        "a private torrent announcing itself to the DHT is what gets an account banned"
    );
}

/// And the converse, so the guard cannot be satisfied by refusing everyone.
#[test]
fn bep27_a_public_torrent_allows_peer_discovery() {
    let meta = parse_torrent_bytes(&torrent_bytes(None)).expect("parses");
    assert!(meta.allows_peer_discovery());
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Encode a message and decode it back through the real codec.
fn round_trip(msg: &Message) -> Message {
    let mut codec = BtCodec::new();
    let mut buf = BytesMut::new();
    codec.encode(msg.clone(), &mut buf).expect("encode");
    codec
        .decode(&mut buf)
        .expect("decode")
        .expect("a whole frame decodes to a message")
}

/// A minimal single-file torrent, optionally carrying `private`.
///
/// Built by hand rather than loaded from a fixture so the bytes under test are
/// visible here: keys are in the order bencode requires, which is what a
/// decoder is entitled to assume.
fn torrent_bytes(private: Option<i64>) -> Vec<u8> {
    let mut info = Vec::new();
    info.extend_from_slice(b"d6:lengthi1024e4:name4:test12:piece lengthi16384e6:pieces20:");
    info.extend_from_slice(&[0xCD; 20]);
    if let Some(p) = private {
        info.extend_from_slice(format!("7:privatei{p}e").as_bytes());
    }
    info.push(b'e');

    let mut out = Vec::new();
    out.extend_from_slice(b"d8:announce24:http://tracker.example/a4:info");
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}
