//! The two pieces of the peer wire that only a connection can exercise: the
//! BEP 3 handshake exchange (who we refuse, and what we say before we refuse)
//! and BEP 9 metadata resolution, which turns a bare info hash into an info
//! dict by asking strangers -- strangers whose answer is checked against the
//! hash and against the size they claimed, byte by byte.
//!
//! The peer on the other end is written here, by hand, speaking the same codec
//! as the engine; what it receives is what the engine actually put on the wire.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::Framed;

use typhon_engine::crypto::stream::CryptoStream;
use typhon_engine::netpin::Egress;
use typhon_engine::peer::extension::{self, PeerPolicy, METADATA_BLOCK, OUR_UT_METADATA_ID};
use typhon_engine::peer::handshake::{build_handshake, incoming, outgoing};
use typhon_engine::peer::message::Message;
use typhon_engine::peer::metadata::{self, MAX_METADATA_SIZE};
use typhon_engine::peer::transport::PeerTransport;
use typhon_engine::wire::codec::BtCodec;

const OURS: [u8; 20] = *b"-TY4R00-aaaaaaaaaaaa";
const THEIRS: [u8; 20] = *b"-qB5220-bbbbbbbbbbbb";
const HASH: [u8; 20] = [0x5A; 20];

async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let dial = tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
    let (server, _) = listener.accept().await.expect("accept");
    (server, dial.await.expect("dialled"))
}

// ---------------------------------------------------------------- handshake

/// The side that dials says its piece first, and says exactly the 68 bytes of
/// BEP 3: what the far side reads is `build_handshake`, nothing added.
#[tokio::test]
async fn an_outgoing_handshake_sends_the_sixty_eight_bytes_and_reads_the_answer() {
    let (ours, mut theirs) = pair().await;
    let mut ours = PeerTransport::Tcp(ours);
    let peer = tokio::spawn(async move {
        let mut got = [0u8; 68];
        theirs.read_exact(&mut got).await.unwrap();
        theirs
            .write_all(&build_handshake(&HASH, &THEIRS))
            .await
            .unwrap();
        got
    });
    let r = outgoing(&mut ours, &HASH, &OURS)
        .await
        .expect("a well-formed answer is accepted");
    assert_eq!(
        peer.await.unwrap(),
        build_handshake(&HASH, &OURS),
        "what we sent"
    );
    assert_eq!((r.peer_id, r.info_hash), (THEIRS, HASH));
    assert!(
        r.fast_extension && r.extended_protocol,
        "the answer claimed both extensions"
    );
}

/// A peer that answers about another torrent is not the peer we dialled for.
#[tokio::test]
async fn an_outgoing_handshake_refuses_an_answer_about_another_torrent() {
    let (ours, mut theirs) = pair().await;
    let mut ours = PeerTransport::Tcp(ours);
    tokio::spawn(async move {
        let mut got = [0u8; 68];
        theirs.read_exact(&mut got).await.unwrap();
        theirs
            .write_all(&build_handshake(&[0x77; 20], &THEIRS))
            .await
            .unwrap();
        theirs
    });
    let err = outgoing(&mut ours, &HASH, &OURS)
        .await
        .err()
        .expect("must be refused");
    assert_eq!(err, "info_hash mismatch");
}

/// The tracker handed us our own listener (a stale self-address list, a
/// loop through a relay): the answer carries OUR peer id, and the session must
/// never start.
#[tokio::test]
async fn an_outgoing_handshake_refuses_to_talk_to_itself() {
    let (ours, mut theirs) = pair().await;
    let mut ours = PeerTransport::Tcp(ours);
    tokio::spawn(async move {
        let mut got = [0u8; 68];
        theirs.read_exact(&mut got).await.unwrap();
        theirs
            .write_all(&build_handshake(&HASH, &OURS))
            .await
            .unwrap();
        theirs
    });
    let err = outgoing(&mut ours, &HASH, &OURS)
        .await
        .err()
        .expect("must be refused");
    assert!(err.starts_with("self-connection"), "{err}");
}

/// The side that is dialled reads first and answers only for a torrent it
/// has: a stranger guessing info hashes gets silence, not a handshake that
/// confirms the torrent exists or not.
#[tokio::test]
async fn an_incoming_handshake_for_an_unknown_torrent_is_closed_unanswered() {
    let (ours, mut theirs) = pair().await;
    let mut ours = PeerTransport::Tcp(ours);
    theirs
        .write_all(&build_handshake(&HASH, &THEIRS))
        .await
        .unwrap();
    let err = incoming(&mut ours, &OURS, |_| false)
        .await
        .err()
        .expect("unknown torrent");
    assert_eq!(err, "unknown info_hash");
    drop(ours);
    let mut rest = Vec::new();
    theirs.read_to_end(&mut rest).await.unwrap();
    assert!(
        rest.is_empty(),
        "{} bytes were sent to a stranger",
        rest.len()
    );
}

/// For a torrent it has, it asks the lookup about the hash the peer named,
/// then answers with its own peer id on that same hash.
#[tokio::test]
async fn an_incoming_handshake_for_a_known_torrent_is_answered_with_our_id() {
    let (ours, mut theirs) = pair().await;
    let mut ours = PeerTransport::Tcp(ours);
    theirs
        .write_all(&build_handshake(&HASH, &THEIRS))
        .await
        .unwrap();
    let asked = std::sync::Mutex::new(Vec::new());
    let r = incoming(&mut ours, &OURS, |ih| {
        asked.lock().unwrap().push(*ih);
        ih == &HASH
    })
    .await
    .expect("known torrent");
    assert_eq!((r.peer_id, r.info_hash), (THEIRS, HASH));
    assert_eq!(
        *asked.lock().unwrap(),
        vec![HASH],
        "the lookup was asked about the peer's hash"
    );
    let mut answer = [0u8; 68];
    theirs.read_exact(&mut answer).await.unwrap();
    assert_eq!(answer, build_handshake(&HASH, &OURS));
}

// ----------------------------------------------------------------- metadata

/// How the fake peer behaves once the extension handshake is exchanged.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    /// Serves every block as asked.
    Good,
    /// Serves a dict that does not hash to the info hash.
    Tampered,
    /// The last block is one byte short of what its size says.
    ShortBlock,
    /// Rejects every request.
    Reject,
    /// Advertises BEP 10 but not ut_metadata.
    NoMetadata,
    /// Offers ut_metadata without saying how big the dict is.
    NoSize,
    /// Claims a dict one byte over the cap.
    TooBig,
    /// Does not claim the extension protocol in the BEP 3 handshake.
    NoExtensionProtocol,
    /// Serves a block of a piece that does not exist, and every block twice,
    /// the second time with garbage.
    Noisy,
}

/// The peer's own id for ut_metadata. Not the engine's (`OUR_UT_METADATA_ID`):
/// a request sent with the wrong id is a request the peer never sees.
const THEIR_UT_METADATA: u8 = 9;

fn dict_of(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn sha1_of(b: &[u8]) -> [u8; 20] {
    Sha1::digest(b).into()
}

async fn serve(listener: TcpListener, ih: [u8; 20], dict: Vec<u8>, mode: Mode) {
    loop {
        let Ok((s, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(peer(s, ih, dict.clone(), mode));
    }
}

async fn peer(mut s: TcpStream, ih: [u8; 20], dict: Vec<u8>, mode: Mode) {
    let mut hs = [0u8; 68];
    if s.read_exact(&mut hs).await.is_err() {
        return;
    }
    let mut reply = build_handshake(&ih, &THEIRS);
    if mode == Mode::NoExtensionProtocol {
        reply[25] &= !0x10;
    }
    if s.write_all(&reply).await.is_err() {
        return;
    }
    let mut f = Framed::new(CryptoStream::plain(PeerTransport::Tcp(s)), BtCodec::new());
    let size = if mode == Mode::TooBig {
        MAX_METADATA_SIZE + 1
    } else {
        dict.len()
    };
    loop {
        // An idle peer hangs up, so a client that asked the wrong way fails
        // with "connection ended" instead of waiting its thirty seconds.
        let Ok(Some(Ok(msg))) = tokio::time::timeout(Duration::from_millis(1500), f.next()).await
        else {
            return;
        };
        let Message::Extended { ext_id, payload } = msg else {
            continue;
        };
        if ext_id == 0 {
            let hs = match mode {
                Mode::NoMetadata => "d1:md6:ut_pexi1eee".to_string(),
                Mode::NoSize => format!("d1:md11:ut_metadatai{THEIR_UT_METADATA}eee"),
                _ => format!("d1:md11:ut_metadatai{THEIR_UT_METADATA}ee13:metadata_sizei{size}ee"),
            };
            let _ = f
                .send(Message::Extended {
                    ext_id: 0,
                    payload: Bytes::from(hs.into_bytes()),
                })
                .await;
        } else if ext_id == THEIR_UT_METADATA {
            let Some(req) = extension::parse_metadata_message(&payload) else {
                return;
            };
            let start = req.piece as usize * METADATA_BLOCK;
            let end = (start + METADATA_BLOCK).min(dict.len());
            let mut block = dict[start..end].to_vec();
            let mut out: Vec<Vec<u8>> = Vec::new();
            match mode {
                Mode::Reject => out.push(extension::build_metadata_reject(req.piece)),
                Mode::Tampered => {
                    block[0] ^= 0xFF;
                    out.push(extension::build_metadata_data(req.piece, size, &block));
                }
                Mode::ShortBlock if end == dict.len() => {
                    block.pop();
                    out.push(extension::build_metadata_data(req.piece, size, &block));
                }
                Mode::Noisy => {
                    out.push(extension::build_metadata_data(99, size, &block));
                    out.push(extension::build_metadata_data(req.piece, size, &block));
                    out.push(extension::build_metadata_data(
                        req.piece,
                        size,
                        &vec![0xEE; block.len()],
                    ));
                }
                _ => out.push(extension::build_metadata_data(req.piece, size, &block)),
            }
            for p in out {
                if f.send(Message::Extended {
                    ext_id: OUR_UT_METADATA_ID,
                    payload: Bytes::from(p),
                })
                .await
                .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn peer_in_mode(dict: &[u8], mode: Mode) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, sha1_of(dict), dict.to_vec(), mode));
    addr
}

async fn ask(addr: SocketAddr, ih: [u8; 20]) -> Result<Vec<u8>, String> {
    tokio::time::timeout(
        Duration::from_secs(10),
        metadata::fetch_from_peer(
            addr,
            ih,
            OURS,
            None,
            6881,
            Egress::default(),
            Arc::new(PeerPolicy::default()),
        ),
    )
    .await
    .expect("fetch_from_peer hung")
}

/// A dict of three blocks (two full, one short) comes back whole: each block
/// is requested with the id the PEER gave ut_metadata, placed at its own
/// offset, and the last one is the length the size says.
#[tokio::test]
async fn a_dict_of_several_blocks_is_assembled_and_verified() {
    let dict = dict_of(2 * METADATA_BLOCK + 1234);
    let addr = peer_in_mode(&dict, Mode::Good).await;
    let got = ask(addr, sha1_of(&dict))
        .await
        .expect("a good peer serves the dict");
    assert_eq!(got.len(), dict.len());
    assert!(
        got == dict,
        "the assembled dict differs from the served one"
    );
}

/// Bytes that do not hash to the info hash are worthless -- a broken or
/// hostile peer -- and must not become a torrent.
#[tokio::test]
async fn a_dict_that_does_not_hash_to_the_info_hash_is_refused() {
    let dict = dict_of(20_000);
    let addr = peer_in_mode(&dict, Mode::Tampered).await;
    let err = ask(addr, sha1_of(&dict))
        .await
        .expect_err("tampered bytes must not pass");
    assert!(err.contains("does not hash"), "{err}");
}

/// The answer must be for the hash we asked: a peer whose dict is genuine for
/// another torrent is refused as well.
#[tokio::test]
async fn a_genuine_dict_of_another_torrent_is_refused() {
    let dict = dict_of(20_000);
    let addr = peer_in_mode(&dict, Mode::Good).await;
    let err = ask(addr, [0x11; 20])
        .await
        .expect_err("another torrent's dict");
    // The fake peer's handshake answers with the hash it was built for; the
    // dial fails on the mismatch before any metadata moves.
    assert_eq!(err, "no connection");
}

/// A block shorter than its size says is an error naming the block, not a
/// dict with a hole in it.
#[tokio::test]
async fn a_block_of_the_wrong_length_is_an_error() {
    let dict = dict_of(METADATA_BLOCK + 500);
    let addr = peer_in_mode(&dict, Mode::ShortBlock).await;
    let err = ask(addr, sha1_of(&dict))
        .await
        .expect_err("a short last block");
    assert_eq!(err, "block 1 is 499 bytes, expected 500");
}

/// A peer that says it cannot serve a block ends the attempt, saying which.
#[tokio::test]
async fn a_rejected_block_ends_the_attempt() {
    let dict = dict_of(20_000);
    let addr = peer_in_mode(&dict, Mode::Reject).await;
    let err = ask(addr, sha1_of(&dict)).await.expect_err("rejected");
    assert!(err.starts_with("peer rejected block"), "{err}");
}

/// Peers that cannot serve metadata are told apart by why.
#[tokio::test]
async fn a_peer_that_cannot_serve_metadata_says_why() {
    let dict = dict_of(20_000);
    for (mode, want) in [
        (Mode::NoMetadata, "peer does not offer ut_metadata"),
        (Mode::NoSize, "peer offers ut_metadata but no metadata_size"),
        (Mode::NoExtensionProtocol, "peer does not speak BEP 10"),
    ] {
        let addr = peer_in_mode(&dict, mode).await;
        let err = ask(addr, sha1_of(&dict))
            .await
            .err()
            .unwrap_or_else(|| panic!("{mode:?} must fail"));
        assert_eq!(err, want, "{mode:?}");
    }
}

/// The size comes from an unauthenticated peer and we allocate on its word
/// alone: one byte over the cap is refused before anything is allocated or
/// requested.
#[tokio::test]
async fn a_claimed_size_over_the_cap_is_refused_before_allocating() {
    let dict = dict_of(20_000);
    let addr = peer_in_mode(&dict, Mode::TooBig).await;
    let err = ask(addr, sha1_of(&dict)).await.expect_err("over the cap");
    assert_eq!(
        err,
        format!(
            "metadata_size {} over the {} cap",
            MAX_METADATA_SIZE + 1,
            MAX_METADATA_SIZE
        )
    );
}

/// A block for a piece that does not exist is ignored, and so is a second copy
/// of a block already held -- the first one stands, the garbage after it does
/// not overwrite it and is not counted towards completion.
#[tokio::test]
async fn a_repeated_or_out_of_range_block_changes_nothing() {
    let dict = dict_of(2 * METADATA_BLOCK + 10);
    let addr = peer_in_mode(&dict, Mode::Noisy).await;
    let got = ask(addr, sha1_of(&dict))
        .await
        .expect("the noise is ignored");
    assert!(got == dict, "a repeated block overwrote a good one");
}

/// Racing peers: the first verified answer wins, a dead peer costs only itself,
/// and with a width of one the next wave is tried after the failed one.
#[tokio::test]
async fn the_race_skips_a_dead_peer_and_takes_the_next_that_serves() {
    let dict = dict_of(20_000);
    let dead = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap()
    };
    let good = peer_in_mode(&dict, Mode::Good).await;
    for width in [0, 1, 8] {
        let got = tokio::time::timeout(
            Duration::from_secs(20),
            metadata::fetch(
                &[dead, good],
                sha1_of(&dict),
                OURS,
                None,
                6881,
                Egress::default(),
                Arc::new(PeerPolicy::default()),
                width,
            ),
        )
        .await
        .expect("fetch hung")
        .unwrap_or_else(|e| panic!("width {width}: {e}"));
        assert!(got == dict, "width {width}");
    }
}

#[tokio::test]
async fn a_race_with_no_peer_that_serves_says_so() {
    let policy = Arc::new(PeerPolicy::default());
    let none = metadata::fetch(
        &[],
        HASH,
        OURS,
        None,
        6881,
        Egress::default(),
        policy.clone(),
        8,
    )
    .await;
    assert_eq!(none.err().as_deref(), Some("no peers to ask"));

    let dict = dict_of(20_000);
    let bad = peer_in_mode(&dict, Mode::Reject).await;
    let lost = metadata::fetch(
        &[bad],
        sha1_of(&dict),
        OURS,
        None,
        6881,
        Egress::default(),
        policy,
        8,
    )
    .await;
    assert_eq!(lost.err().as_deref(), Some("no peer served the metadata"));
}
