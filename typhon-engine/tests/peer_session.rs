//! A real peer session, driven over a loopback TCP pair.
//!
//! `peer/session.rs` is the loop every connection lives in, and nothing had
//! ever run it in a test: it takes a framed stream, so the only way to exercise
//! it is to be the peer on the other end. That is what this does -- our session
//! on one socket, a hand-written peer speaking the same codec on the other.
//!
//! What it asserts is the opening of a BitTorrent conversation, which is
//! precisely the part a port can lose without anything failing to compile: a
//! seeding client says what it has and then that it is willing to give it.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::Framed;

use typhon_engine::crypto::stream::CryptoStream;
use typhon_engine::disk::DiskManager;
use typhon_engine::peer::message::Message;
use typhon_engine::peer::session;
use typhon_engine::peer::transport::PeerTransport;
use typhon_engine::torrent::meta::{TorrentMeta, TorrentState, TorrentStatus};
use typhon_engine::wire::codec::BtCodec;

const OUR_ID: [u8; 20] = *b"-TY4R00-aaaaaaaaaaaa";
const THEIR_ID: [u8; 20] = *b"-qB5220-bbbbbbbbbbbb";

fn meta(num_pieces: u32) -> TorrentMeta {
    TorrentMeta {
        info_hash: [0x5A; 20],
        name: "session-under-test".into(),
        num_pieces,
        piece_length: 16384,
        total_size: num_pieces as u64 * 16384,
        files: Vec::new(),
        trackers: Vec::new(),
        url_list: Vec::new(),
        private: false,
        multi_file: false,
        info_dict_len: 0,
        v2: false,
    }
}

/// A torrent we hold in full: `seed_mode` skips the picker, which is what a
/// seeding torrent looks like in production.
fn seeding(num_pieces: u32) -> Arc<TorrentState> {
    let t = Arc::new(TorrentState::new(meta(num_pieces), PathBuf::from("/tmp"), true));
    t.status
        .store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
    t
}

/// A connected pair: the session's end, and ours.
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let dial = tokio::spawn(async move { TcpStream::connect(addr).await.expect("connect") });
    let (server, _) = listener.accept().await.expect("accept");
    (server, dial.await.expect("dialled"))
}

fn framed(s: TcpStream) -> Framed<CryptoStream, BtCodec> {
    Framed::new(CryptoStream::plain(PeerTransport::Tcp(s)), BtCodec::new())
}

/// Start a session on one end and hand back the other, as a peer would hold it.
fn start(torrent: Arc<TorrentState>, fast_ext: bool, ours: TcpStream, theirs: TcpStream)
    -> Framed<CryptoStream, BtCodec>
{
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    tokio::spawn(session::run(
        framed(ours),
        addr,
        torrent,
        Arc::new(DiskManager::new(16)),
        OUR_ID,
        THEIR_ID,
        false, // not encrypted
        fast_ext,
        false, // no BEP 10: this is about the BEP 3 opening
        None,  // no uTP socket
        0,     // listen port unknown, which disables the extension handshake
    ));
    framed(theirs)
}

/// Read messages until one matches, or give up. A session sends what it has to
/// say in its own order and may interleave keepalives.
async fn wait_for<F: Fn(&Message) -> bool>(
    peer: &mut Framed<CryptoStream, BtCodec>,
    what: &str,
    pred: F,
) -> Message {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            panic!("no {what} in {seen:?}");
        }
        match tokio::time::timeout(left, peer.next()).await {
            Ok(Some(Ok(m))) => {
                if pred(&m) {
                    return m;
                }
                seen.push(format!("{m:?}"));
            }
            Ok(Some(Err(e))) => panic!("the session sent something unreadable: {e}"),
            Ok(None) => panic!("the session hung up before sending {what}; saw {seen:?}"),
            Err(_) => panic!("no {what} within five seconds; saw {seen:?}"),
        }
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// BEP 6: a peer that negotiated the fast extension and holds everything says
/// `have all` -- one byte instead of a bitfield that, on a 300k-piece torrent,
/// is tens of kilobytes per connection.
#[test]
fn a_complete_seed_opens_with_have_all_when_fast_is_on() {
    rt().block_on(async {
        let (ours, theirs) = pair().await;
        let mut peer = start(seeding(64), true, ours, theirs);
        wait_for(&mut peer, "have all", |m| matches!(m, Message::HaveAll)).await;
    });
}

/// Without the fast extension there is no `have all`, so the same thing has to
/// be said as a bitfield. Saying nothing leaves the peer believing we hold
/// nothing, and it never asks.
#[test]
fn the_same_seed_opens_with_a_bitfield_when_fast_is_off() {
    rt().block_on(async {
        let (ours, theirs) = pair().await;
        let mut peer = start(seeding(64), false, ours, theirs);
        let m = wait_for(&mut peer, "bitfield", |m| matches!(m, Message::Bitfield { .. })).await;
        match m {
            Message::Bitfield { data } => {
                assert_eq!(data.len(), 8, "sixty-four pieces fit in eight bytes");
                assert!(
                    data.iter().all(|b| *b == 0xFF),
                    "a complete torrent has every bit set: {data:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    });
}

/// And then it unchokes. A seed that announces what it has and never lets
/// anybody take it uploads nothing, which is the failure that looks like
/// working: the connection is up, the peer is there, no bytes move.
#[test]
fn a_seed_unchokes_so_the_peer_may_actually_ask() {
    rt().block_on(async {
        let (ours, theirs) = pair().await;
        let mut peer = start(seeding(64), true, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
    });
}

/// The session registers itself on the torrent for as long as it lives. That
/// count is what the choking engine ranks and what the interface shows, and a
/// session that never registered is invisible to both.
#[test]
fn a_live_session_is_counted_on_the_torrent() {
    rt().block_on(async {
        let torrent = seeding(64);
        let (ours, theirs) = pair().await;
        let mut peer = start(torrent.clone(), true, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;

        assert_eq!(torrent.peers_connected.load(Ordering::Relaxed), 1);
        assert!(
            torrent.peer_stats.iter().next().is_some(),
            "and it is in the peer table the choking engine reads"
        );
    });
}

/// A peer that hangs up is released. The guard is RAII precisely because the
/// session has many ways out, and a leaked entry holds a piece-sized buffer
/// that nothing frees -- measured at 459 MB of growth in half an hour.
#[test]
fn a_peer_that_hangs_up_is_released() {
    rt().block_on(async {
        let torrent = seeding(64);
        let (ours, theirs) = pair().await;
        let mut peer = start(torrent.clone(), true, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
        assert_eq!(torrent.peers_connected.load(Ordering::Relaxed), 1);

        drop(peer); // the peer goes away

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while torrent.peers_connected.load(Ordering::Relaxed) != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the session did not release its registration"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(torrent.peer_stats.is_empty(), "and the table is empty again");
    });
}

/// Interest is recorded: the choking engine only ranks peers that asked, so a
/// lost `interested` means a peer that is never unchoked on merit.
#[test]
fn an_interested_peer_is_recorded_as_interested() {
    rt().block_on(async {
        let torrent = seeding(64);
        let (ours, theirs) = pair().await;
        let mut peer = start(torrent.clone(), true, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;

        peer.send(Message::Interested).await.expect("sent");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let interested = torrent
                .peer_stats
                .iter()
                .any(|e| e.value().interested.load(Ordering::Relaxed));
            if interested {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the session never recorded the interest"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

/// A session with the extension protocol on, as production runs it.
fn start_extended(torrent: Arc<TorrentState>, ours: TcpStream, theirs: TcpStream)
    -> Framed<CryptoStream, BtCodec>
{
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    tokio::spawn(session::run(
        framed(ours),
        addr,
        torrent,
        Arc::new(DiskManager::new(16)),
        OUR_ID,
        THEIR_ID,
        false,
        true,
        true,  // BEP 10 negotiated
        None,
        16171, // a listen port: the extension handshake is on
    ));
    framed(theirs)
}

fn private_seed(num_pieces: u32) -> Arc<TorrentState> {
    let mut m = meta(num_pieces);
    m.private = true;
    let t = Arc::new(TorrentState::new(m, PathBuf::from("/tmp"), true));
    t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
    t
}

/// Send one PEX message naming a public peer, and report how many peers the
/// torrent says it learned from PEX.
async fn pex_learned(torrent: Arc<TorrentState>) -> u64 {
    use typhon_engine::peer::extension::{build_pex_message, OUR_UT_PEX_ID};
    let (ours, theirs) = pair().await;
    let mut peer = start_extended(torrent.clone(), ours, theirs);
    wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
    let added: SocketAddr = "93.184.216.34:6881".parse().unwrap();
    peer.send(Message::Extended {
        ext_id: OUR_UT_PEX_ID,
        payload: bytes::Bytes::from(build_pex_message(&[added], &[])),
    })
    .await
    .expect("sent");
    // A keepalive behind it: once the session has read that, it has read the
    // PEX message too -- messages on one connection are handled in order.
    peer.send(Message::KeepAlive).await.expect("sent");
    tokio::time::sleep(Duration::from_millis(300)).await;
    torrent.pex_peers_discovered.load(Ordering::Relaxed)
}

/// ⭐⭐ BEP 27: a private torrent learns nothing from PEX, even from a peer
/// that sends it unasked. The public control proves the probe can see a
/// learned peer at all -- a probe that always reads zero proves nothing.
#[test]
fn bep27_a_private_torrent_learns_no_peer_from_pex_even_unasked() {
    rt().block_on(async {
        assert!(pex_learned(seeding(8)).await > 0, "control: a public torrent does learn from PEX");
        assert_eq!(pex_learned(private_seed(8)).await, 0, "a private torrent takes peers from its tracker only");
    });
}

/// ⭐⭐ BEP 27 / BEP 55: a hole-punch `connect` names a peer to dial. On a
/// private torrent that is a peer from outside the tracker, and it is not
/// dialled. The public control shows the same message IS acted on there.
#[test]
fn bep27_a_private_torrent_dials_no_peer_a_hole_punch_names() {
    use typhon_engine::peer::holepunch::{Punch, OUR_UT_HOLEPUNCH_ID};
    use typhon_engine::tracker::{DIAL_ENQUEUED, DIAL_ENQUEUE_DROPPED};
    fn dials() -> u64 {
        DIAL_ENQUEUED.load(Ordering::Relaxed) + DIAL_ENQUEUE_DROPPED.load(Ordering::Relaxed)
    }
    async fn punched(torrent: Arc<TorrentState>) -> u64 {
        let (ours, theirs) = pair().await;
        let mut peer = start_extended(torrent, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
        let before = dials();
        let target: SocketAddr = "93.184.216.34:6881".parse().unwrap();
        peer.send(Message::Extended {
            ext_id: OUR_UT_HOLEPUNCH_ID,
            payload: bytes::Bytes::from(Punch::Connect(target).encode()),
        })
        .await
        .expect("sent");
        peer.send(Message::KeepAlive).await.expect("sent");
        tokio::time::sleep(Duration::from_millis(300)).await;
        dials() - before
    }
    rt().block_on(async {
        assert!(punched(seeding(8)).await > 0, "control: a public torrent acts on the connect");
        assert_eq!(punched(private_seed(8)).await, 0, "a private torrent dials nobody a peer names");
    });
}

// ---------------------------------------------------------------------------
// Upload rate cap
// ---------------------------------------------------------------------------

/// A seeding torrent with a real payload on disk, so requests are actually
/// served: `pieces` blocks of 16 KiB in one file.
fn seeding_with_payload(tag: &str, pieces: u32) -> (Arc<TorrentState>, PathBuf) {
    let dir = std::env::temp_dir().join(format!("peer-session-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let len = pieces as u64 * 16384;
    std::fs::write(dir.join("payload.bin"), vec![0x5Au8; len as usize]).unwrap();
    let mut m = meta(pieces);
    m.name = "payload.bin".into();
    m.files = vec![typhon_engine::torrent::meta::FileEntry {
        path: PathBuf::from("payload.bin"),
        offset: 0,
        length: len,
    }];
    let t = Arc::new(TorrentState::new(m, dir.clone(), true));
    t.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
    (t, dir)
}

/// Ask for every block, then count the payload bytes that arrive within
/// `window`, and the Rejects.
async fn pull(peer: &mut Framed<CryptoStream, BtCodec>, pieces: u32, window: Duration) -> (u64, u32) {
    wait_for(peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
    peer.send(Message::Interested).await.expect("sent");
    for i in 0..pieces {
        peer.feed(Message::Request { index: i, begin: 0, length: 16384 }).await.expect("fed");
    }
    peer.flush().await.expect("flushed");
    let deadline = tokio::time::Instant::now() + window;
    let (mut bytes, mut rejects) = (0u64, 0u32);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, peer.next()).await {
            Ok(Some(Ok(Message::Piece { data, .. }))) => {
                bytes += data.len() as u64;
                if bytes >= pieces as u64 * 16384 {
                    break;
                }
            }
            Ok(Some(Ok(Message::Reject { .. }))) => rejects += 1,
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("unreadable: {e}"),
            Ok(None) => panic!("the session hung up"),
            Err(_) => break,
        }
    }
    (bytes, rejects)
}

/// ⭐⭐ The cap is MEASURED on the wire: a torrent capped at 256 KiB/s sends,
/// over three seconds, 768 KiB give or take the burst -- not the 2 MiB the
/// peer asked for, which the uncapped control delivers in well under that.
/// Before 4.4 the setting existed and capped nothing.
#[test]
fn an_upload_cap_holds_on_the_wire() {
    rt().block_on(async {
        const PIECES: u32 = 128; // 2 MiB
        // Control: uncapped, everything arrives at once.
        let (t, dir) = seeding_with_payload("uncapped", PIECES);
        let (ours, theirs) = pair().await;
        let mut peer = start(t, true, ours, theirs);
        let (bytes, _) = pull(&mut peer, PIECES, Duration::from_secs(3)).await;
        assert_eq!(bytes, PIECES as u64 * 16384, "control: uncapped, the whole 2 MiB arrives");
        let _ = std::fs::remove_dir_all(dir);

        // Capped at 256 KiB/s.
        let (t, dir) = seeding_with_payload("capped", PIECES);
        t.set_rate_limits(Some(256 * 1024), None);
        let (ours, theirs) = pair().await;
        let mut peer = start(t, true, ours, theirs);
        let (bytes, rejects) = pull(&mut peer, PIECES, Duration::from_secs(3)).await;
        let kib = bytes / 1024;
        // 3 s x 256 KiB/s = 768 KiB, plus the 64 KiB burst and the block in
        // hand. -15% / +15% around that.
        assert!((650..=900).contains(&kib), "256 KiB/s over 3 s sent {kib} KiB");
        assert_eq!(rejects, 0, "a capped request is deferred, not refused");
        let _ = std::fs::remove_dir_all(dir);
    });
}

/// ⭐ A capped session keeps READING: a Cancel sent while its requests wait
/// on the cap is acted on at once (BEP 6: answered with a Reject), instead of
/// sitting in the socket until the queue ahead of it has drained.
#[test]
fn a_capped_session_still_hears_the_peer() {
    rt().block_on(async {
        const PIECES: u32 = 32;
        let (t, dir) = seeding_with_payload("cancel", PIECES);
        t.set_rate_limits(Some(16 * 1024), None); // one block a second
        let (ours, theirs) = pair().await;
        let mut peer = start(t, true, ours, theirs);
        wait_for(&mut peer, "unchoke", |m| matches!(m, Message::Unchoke)).await;
        peer.send(Message::Interested).await.expect("sent");
        for i in 0..PIECES {
            peer.feed(Message::Request { index: i, begin: 0, length: 16384 }).await.expect("fed");
        }
        peer.flush().await.expect("flushed");
        let last = PIECES - 1;
        peer.send(Message::Cancel { index: last, begin: 0, length: 16384 }).await.expect("sent");
        // At 16 KiB/s the last block is ~30 s away; its Reject must come now.
        let started = tokio::time::Instant::now();
        let m = wait_for(&mut peer, "reject of the cancelled block", |m| {
            matches!(m, Message::Reject { index, .. } if *index == last)
        })
        .await;
        assert!(matches!(m, Message::Reject { .. }));
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        let _ = std::fs::remove_dir_all(dir);
    });
}
