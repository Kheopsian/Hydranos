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
