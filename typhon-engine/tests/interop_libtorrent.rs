//! Interoperability with libtorrent, through a real qBittorrent.
//!
//! `two_engines.rs` proves Hydranos agrees with itself. These prove it agrees
//! with the client most swarms are made of: qBittorrent downloads a torrent
//! from Hydranos and Hydranos downloads one from qBittorrent, both verified
//! piece by piece by the receiving side's own SHA-1 check -- and the same
//! again with encryption required, which only an MSE handshake gets through.
//!
//! `#[ignore]`d: they need the qBittorrent that `tools/interop/run.sh` starts,
//! on a Docker network shared with this test, with a volume both can read.
//!
//! Environment (set by the script):
//!   HYDRANOS_INTEROP_QBIT         the WebUI, e.g. http://qbit:8080
//!   HYDRANOS_INTEROP_QBIT_PEER    qBittorrent's peer address, host:port
//!   HYDRANOS_INTEROP_SHARED       a directory mounted in both containers
//!   HYDRANOS_INTEROP_QBIT_SHARED  the same directory as qBittorrent sees it

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha1::{Digest, Sha1};
use typhon_engine::config::ResolvedBinding;
use typhon_engine::disk::DiskManager;
use typhon_engine::netpin::Egress;
use typhon_engine::torrent::TorrentManager;

/// Several blocks per piece, several pieces: the request pipeline, not a
/// single message, is what gets exercised.
const PIECE_LEN: usize = 65536;
const PIECES: usize = 32;
const TOTAL: usize = PIECE_LEN * PIECES;

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is not set -- run tools/interop/run.sh"))
}

/// Content that differs per test and per piece.
fn content(seed: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(TOTAL);
    let mut x: u32 = seed;
    for _ in 0..TOTAL {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        out.push((x >> 24) as u8);
    }
    out
}

fn build_torrent(name: &str, data: &[u8], private: bool) -> Vec<u8> {
    let mut pieces = Vec::with_capacity(PIECES * 20);
    for chunk in data.chunks(PIECE_LEN) {
        let mut h = Sha1::new();
        h.update(chunk);
        pieces.extend_from_slice(&h.finalize());
    }
    let mut info = Vec::new();
    info.push(b'd');
    info.extend_from_slice(format!("6:lengthi{}e", data.len()).as_bytes());
    info.extend_from_slice(format!("4:name{}:{name}", name.len()).as_bytes());
    info.extend_from_slice(format!("12:piece lengthi{PIECE_LEN}e").as_bytes());
    info.extend_from_slice(format!("6:pieces{}:", pieces.len()).as_bytes());
    info.extend_from_slice(&pieces);
    if private {
        info.extend_from_slice(b"7:privatei1e");
    }
    info.push(b'e');
    let announce = "http://tracker.invalid/announce";
    let mut out = format!("d8:announce{}:{announce}4:info", announce.len()).into_bytes();
    out.extend_from_slice(&info);
    out.push(b'e');
    out
}

fn info_hash_hex(torrent: &[u8]) -> String {
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(torrent).expect("parses");
    typhon_engine::torrent::hex_encode(&meta.info_hash)
}

/// Our own address on the shared network, as qBittorrent will dial it: the
/// source address the kernel picks to reach qBittorrent.
fn our_address_towards(peer: &str) -> std::net::IpAddr {
    let target: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(peer).unwrap().next().unwrap();
    let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect(target).unwrap();
    s.local_addr().unwrap().ip()
}

fn peer_id() -> [u8; 20] {
    let cfg: typhon_engine::config::EngineConfig = serde_json::from_str("{}").unwrap();
    typhon_engine::config::set_version("4.2.4");
    cfg.peer_id()
}

struct Engine {
    mgr: Arc<TorrentManager>,
    disk: Arc<DiskManager>,
    root: std::path::PathBuf,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn engine(tag: &str, torrent: Vec<u8>) -> Engine {
    let root = std::path::PathBuf::from(env("HYDRANOS_INTEROP_SHARED"))
        .join(format!("hydranos-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("resume")).unwrap();
    let disk = Arc::new(DiskManager::new(64));
    let mgr = Arc::new(TorrentManager::new(
        root.join("data").to_string_lossy().into_owned(),
        root.join("resume").to_string_lossy().into_owned(),
        disk.clone(),
    ));
    let blob = torrent.clone();
    mgr.set_blob_source(Arc::new(move |_h: &str| Some(blob.clone())));
    Engine { mgr, disk, root }
}

// --- qBittorrent's WebUI ----------------------------------------------------

struct Qbit {
    base: String,
    http: reqwest::Client,
}

impl Qbit {
    fn new() -> Qbit {
        Qbit { base: env("HYDRANOS_INTEROP_QBIT"), http: reqwest::Client::new() }
    }

    async fn set_prefs(&self, json: &str) {
        let r = self
            .http
            .post(format!("{}/api/v2/app/setPreferences", self.base))
            .form(&[("json", json)])
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "setPreferences: {}", r.status());
    }

    async fn add(&self, torrent: &[u8], save_path: &str) {
        let part = reqwest::multipart::Part::bytes(torrent.to_vec()).file_name("t.torrent");
        let form = reqwest::multipart::Form::new()
            .part("torrents", part)
            .text("savepath", save_path.to_string())
            .text("autoTMM", "false");
        let r = self
            .http
            .post(format!("{}/api/v2/torrents/add", self.base))
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "torrents/add: {}", r.status());
    }

    async fn add_peer(&self, hash: &str, peer: SocketAddr) {
        let r = self
            .http
            .post(format!("{}/api/v2/torrents/addPeers", self.base))
            .form(&[("hashes", hash.to_string()), ("peers", peer.to_string())])
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "addPeers: {}", r.status());
    }

    async fn info(&self, hash: &str) -> serde_json::Value {
        let v: serde_json::Value = self
            .http
            .get(format!("{}/api/v2/torrents/info?hashes={hash}", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        v[0].clone()
    }

    async fn peers(&self, hash: &str) -> serde_json::Value {
        self.http
            .get(format!("{}/api/v2/sync/torrentPeers?hash={hash}&rid=0", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn delete(&self, hash: &str) {
        let _ = self
            .http
            .post(format!("{}/api/v2/torrents/delete", self.base))
            .form(&[("hashes", hash), ("deleteFiles", "true")])
            .send()
            .await;
    }

    /// Wait until qBittorrent holds every piece -- each one having passed its
    /// own SHA-1 check -- AND is seeding them: libtorrent turns incoming
    /// connections away while it is still checking, and progress reads 1.0
    /// before the check has formally ended.
    async fn wait_complete(&self, hash: &str, what: &str) -> serde_json::Value {
        for _ in 0..600 {
            let i = self.info(hash).await;
            let seeding = matches!(
                i["state"].as_str(),
                Some("uploading" | "stalledUP" | "forcedUP" | "queuedUP")
            );
            if i["progress"].as_f64() == Some(1.0) && seeding {
                return i;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{what}: qBittorrent never completed; last state {}", self.info(hash).await);
    }
}

fn listen(e: &Engine, port: u16, pid: [u8; 20]) -> Arc<AtomicBool> {
    let listening = Arc::new(AtomicBool::new(false));
    let mgr = e.mgr.clone();
    let disk = e.disk.clone();
    let l = listening.clone();
    tokio::spawn(async move {
        let _ = typhon_engine::peer::listen(
            vec![ResolvedBinding {
                id: 0,
                addr: format!("0.0.0.0:{port}").parse().unwrap(),
                peer_id: pid,
                egress: Egress::default(),
                advertised_port: port,
                only_v6: false,
            }],
            port,
            mgr,
            disk,
            None,
            l,
        )
        .await;
    });
    listening
}

async fn wait_listening(l: &AtomicBool) {
    for _ in 0..200 {
        if l.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the listener never came up");
}

/// qBittorrent LEECHES from Hydranos. Every piece passes libtorrent's hash
/// check, and Hydranos counts exactly the payload it served.
async fn libtorrent_downloads_from_us(tag: &str, private: bool, seed: u32) {
    let data = content(seed);
    let name = format!("from-hydranos-{tag}.bin");
    let torrent = build_torrent(&name, &data, private);
    let hash = info_hash_hex(&torrent);
    let q = Qbit::new();

    let us = engine(tag, torrent.clone());
    std::fs::write(us.root.join("data").join(&name), &data).unwrap();
    let (ih, _) = us
        .mgr
        .add_torrent_bytes(&torrent, &us.root.join("data").to_string_lossy(), false, true)
        .expect("we seed it");
    let port = 16000 + (seed % 1000) as u16;
    let pid = peer_id();
    wait_listening(&listen(&us, port, pid)).await;

    let peer = env("HYDRANOS_INTEROP_QBIT_PEER");
    let me = SocketAddr::new(our_address_towards(&peer), port);
    q.add(&torrent, &format!("{}/qbit-{tag}", env("HYDRANOS_INTEROP_QBIT_SHARED"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    q.add_peer(&hash, me).await;

    let info = q.wait_complete(&hash, tag).await;
    assert_eq!(info["size"].as_u64(), Some(TOTAL as u64), "{info}");

    let t = us.mgr.get(&ih).unwrap();
    assert_eq!(
        t.total_uploaded.load(Ordering::Relaxed),
        TOTAL as u64,
        "we count exactly the payload we served -- no more, no less"
    );
    q.delete(&hash).await;
}

/// Hydranos LEECHES from qBittorrent: the bytes on our disk are the bytes it
/// holds, verified piece by piece by our own hash check.
async fn we_download_from_libtorrent(tag: &str, seed: u32) {
    let data = content(seed);
    let name = format!("from-libtorrent-{tag}.bin");
    let torrent = build_torrent(&name, &data, false);
    let hash = info_hash_hex(&torrent);
    let q = Qbit::new();

    // qBittorrent's copy, on the shared volume, then checked by qBittorrent.
    let shared = std::path::PathBuf::from(env("HYDRANOS_INTEROP_SHARED")).join(format!("qseed-{tag}"));
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join(&name), &data).unwrap();
    q.add(&torrent, &format!("{}/qseed-{tag}", env("HYDRANOS_INTEROP_QBIT_SHARED"))).await;
    q.wait_complete(&hash, "qBittorrent's own recheck").await;

    let us = engine(tag, torrent.clone());
    let (ih, _) = us
        .mgr
        .add_torrent_bytes(&torrent, &us.root.join("data").to_string_lossy(), false, false)
        .expect("we leech it");
    let t = us.mgr.get(&ih).unwrap();
    let peer: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(&env("HYDRANOS_INTEROP_QBIT_PEER").as_str())
        .unwrap()
        .next()
        .unwrap();
    let pid = peer_id();

    // Dial, and dial again while nothing is connected -- what a tracker's next
    // answer does in production. `dial_peer` skips a peer already connected,
    // so a redial never opens a second session.
    let target = us.root.join("data").join(&name);
    let mut got = Vec::new();
    let mut seen_client = String::new();
    for tick in 0..600 {
        if tick % 20 == 0 && t.peers_connected.load(Ordering::Relaxed) == 0 {
            let t = t.clone();
            let disk = us.disk.clone();
            tokio::spawn(async move {
                typhon_engine::tracker::dial_peer(peer, t, disk, pid, None, 16999, &Egress::default()).await;
            });
        }
        if seen_client.is_empty() {
            if let Some(p) = t.peer_stats.iter().next() {
                seen_client = p.value().client.clone();
            }
        }
        if let Ok(b) = std::fs::read(&target) {
            if b.len() == data.len() && b == data {
                got = b;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !seen_client.is_empty() {
        eprintln!("[interop] {tag}: we saw libtorrent as {seen_client:?}");
    }
    if got != data {
        use typhon_engine::tracker as tr;
        eprintln!(
            "[interop] {tag}: dials attempted={} plain ok/fail={}/{} mse ok/fail={}/{} tcp ok/fail={}/{} hs ok/fail={}/{}",
            tr::DIAL_ATTEMPTED.load(Ordering::Relaxed),
            tr::DIAL_PLAIN_OK.load(Ordering::Relaxed),
            tr::DIAL_PLAIN_FAIL.load(Ordering::Relaxed),
            tr::DIAL_MSE_OK.load(Ordering::Relaxed),
            tr::DIAL_MSE_FAIL.load(Ordering::Relaxed),
            tr::DIAL_TCP_OK.load(Ordering::Relaxed),
            tr::DIAL_TCP_FAIL.load(Ordering::Relaxed),
            tr::DIAL_HANDSHAKE_OK.load(Ordering::Relaxed),
            tr::DIAL_HANDSHAKE_FAIL.load(Ordering::Relaxed),
        );
        eprintln!("[interop] {tag}: qBittorrent says {}", q.info(&hash).await);
        eprintln!("[interop] {tag}: qBittorrent peers {}", q.peers(&hash).await);
        eprintln!(
            "[interop] {tag}: our status={} peers_connected={} got={}",
            t.status.load(Ordering::Relaxed),
            t.peers_connected.load(Ordering::Relaxed),
            typhon_engine::tracker::BT_GOT_PIECE.load(Ordering::Relaxed)
        );
    }
    assert!(got == data, "{tag}: the file never matched (downloaded {} bytes)", t.total_downloaded.load(Ordering::Relaxed));
    assert_eq!(t.total_downloaded.load(Ordering::Relaxed), TOTAL as u64, "verified payload, counted once");
    q.delete(&hash).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_libtorrent_downloads_a_torrent_from_hydranos() {
    Qbit::new().set_prefs(r#"{"encryption":0}"#).await;
    libtorrent_downloads_from_us("plain", false, 0x1111_1111).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_hydranos_downloads_a_torrent_from_libtorrent() {
    Qbit::new().set_prefs(r#"{"encryption":0}"#).await;
    we_download_from_libtorrent("plain", 0x2222_2222).await;
}

/// libtorrent set to REQUIRE encryption: nothing but an MSE handshake gets a
/// byte through, in either direction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_mse_both_ways_with_libtorrent_requiring_encryption() {
    let q = Qbit::new();
    q.set_prefs(r#"{"encryption":1}"#).await;
    we_download_from_libtorrent("mse", 0x3333_3333).await;
    libtorrent_downloads_from_us("mse", false, 0x4444_4444).await;
    q.set_prefs(r#"{"encryption":0}"#).await;
}

/// A private torrent (BEP 27) transfers like any other between the two: the
/// flag changes where peers come from, not how pieces move.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_libtorrent_downloads_a_private_torrent_from_hydranos() {
    Qbit::new().set_prefs(r#"{"encryption":0}"#).await;
    libtorrent_downloads_from_us("private", true, 0x5555_5555).await;
}

/// ⭐ A magnet resolved against libtorrent: we know only the info hash and
/// qBittorrent's address, fetch the info dict over BEP 9, and it hashes to
/// the magnet's info hash -- the step a magnet add stands on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_hydranos_resolves_a_magnet_from_libtorrent() {
    let tag = "magnet";
    let data = content(77);
    let name = format!("magnet-{tag}.bin");
    let torrent = build_torrent(&name, &data, false);
    let hash = info_hash_hex(&torrent);
    let q = Qbit::new();
    let shared = std::path::PathBuf::from(env("HYDRANOS_INTEROP_SHARED")).join(format!("qseed-{tag}"));
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join(&name), &data).unwrap();
    q.add(&torrent, &format!("{}/qseed-{tag}", env("HYDRANOS_INTEROP_QBIT_SHARED"))).await;
    q.wait_complete(&hash, "qBittorrent's own recheck").await;

    // An engine that has never seen the torrent: no blob, no .torrent.
    let us = engine(tag, Vec::new());
    let peer: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(&env("HYDRANOS_INTEROP_QBIT_PEER").as_str())
        .unwrap()
        .next()
        .unwrap();
    let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
    let cfg: typhon_engine::config::EngineConfig = serde_json::from_str("{}").unwrap();
    // libtorrent refuses connections to a torrent it is still checking. A
    // resolution asks again every few seconds within its budget; and a
    // resolution that fails is started again, as `magnets::drive` does.
    let mut dict = None;
    let mut last = String::new();
    'attempts: for _ in 0..15 {
        assert!(us.mgr.magnet().start(ih, Vec::new(), vec![peer], &cfg, None, None), "a resolution starts");
        for _ in 0..400 {
            match us.mgr.magnet().state_of(&ih) {
                Some(typhon_engine::magnet::JobState::Done(d)) => {
                    dict = Some(d);
                    break 'attempts;
                }
                Some(typhon_engine::magnet::JobState::Failed(e)) => {
                    last = e;
                    us.mgr.magnet().forget(&ih);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue 'attempts;
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }
    if dict.is_none() {
        let b = &cfg.resolved_bindings()[0];
        let direct = typhon_engine::peer::metadata::fetch_from_peer(peer, ih, b.peer_id, None, b.advertised_port, b.egress.clone()).await;
        eprintln!("[interop] magnet: last failure {last:?}; asked directly: {:?}; qBittorrent says {}", direct.map(|d| d.len()), q.info(&hash).await);
    }
    let dict = dict.expect("the metadata arrived within a minute");
    let got: [u8; 20] = Sha1::digest(&dict).into();
    assert_eq!(got, ih, "the dict libtorrent sent is the torrent's");
    q.delete(&hash).await;
}

/// ⭐ The IP filter against a real peer, three ways: a libtorrent already
/// connected is cut off when it becomes blocked, it cannot connect back in,
/// and we do not dial it out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs qBittorrent: tools/interop/run.sh"]
async fn interop_a_blocked_libtorrent_is_cut_off_and_kept_out() {
    let tag = "ipfilter";
    let data = content(91);
    let name = format!("filtered-{tag}.bin");
    let torrent = build_torrent(&name, &data, false);
    let hash = info_hash_hex(&torrent);
    let q = Qbit::new();

    // We seed, qBittorrent downloads: a connection to cut.
    let us = engine(tag, torrent.clone());
    std::fs::write(us.root.join("data").join(&name), &data).unwrap();
    let (ih, _) = us.mgr.add_torrent_bytes(&torrent, &us.root.join("data").to_string_lossy(), false, true).unwrap();
    let port = 16911;
    wait_listening(&listen(&us, port, peer_id())).await;
    let peer_env = env("HYDRANOS_INTEROP_QBIT_PEER");
    let qaddr: SocketAddr = std::net::ToSocketAddrs::to_socket_addrs(&peer_env.as_str()).unwrap().next().unwrap();
    let me = SocketAddr::new(our_address_towards(&peer_env), port);
    q.add(&torrent, &format!("{}/qbit-{tag}", env("HYDRANOS_INTEROP_QBIT_SHARED"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Slowed to 32 KiB/s, so the connection is still downloading when the
    // ban lands: finished, libtorrent would close a seed-to-seed link itself
    // and the test would prove nothing.
    q.http
        .post(format!("{}/api/v2/torrents/setDownloadLimit", q.base))
        .form(&[("hashes", hash.as_str()), ("limit", "32768")])
        .send()
        .await
        .unwrap();
    q.add_peer(&hash, me).await;
    let t = us.mgr.get(&ih).unwrap();
    for _ in 0..200 {
        if t.peers_connected.load(Ordering::Relaxed) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(t.peers_connected.load(Ordering::Relaxed) > 0, "libtorrent is connected before the ban");

    // 1. Cut off: blocked while connected, woken, gone.
    use typhon_engine::ipfilter as f;
    f::install(Some(f::IpFilter::parse(&qaddr.ip().to_string()).0));
    assert!(us.mgr.wake_filtered_peers() > 0, "its session is woken");
    for _ in 0..100 {
        if t.peers_connected.load(Ordering::Relaxed) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(t.peers_connected.load(Ordering::Relaxed), 0, "the blocked peer is disconnected");
    assert!(f::DROPPED.load(Ordering::Relaxed) >= 1);

    // 2. Kept out: a fresh copy on its side -- libtorrent backs off a peer
    // that dropped it, a new torrent has no such memory -- told about us.
    let before_in = f::BLOCKED_IN.load(Ordering::Relaxed);
    q.delete(&hash).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    q.add(&torrent, &format!("{}/qbit-{tag}-2", env("HYDRANOS_INTEROP_QBIT_SHARED"))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    q.add_peer(&hash, me).await;
    for _ in 0..100 {
        if f::BLOCKED_IN.load(Ordering::Relaxed) > before_in {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(f::BLOCKED_IN.load(Ordering::Relaxed) > before_in, "its inbound attempt is refused");
    assert_eq!(t.peers_connected.load(Ordering::Relaxed), 0);

    // 3. Not dialled: we do not open a connection to it either.
    let before_out = f::BLOCKED_OUT.load(Ordering::Relaxed);
    typhon_engine::tracker::dial_peer(qaddr, t.clone(), us.disk.clone(), peer_id(), None, port, &Egress::default()).await;
    assert!(f::BLOCKED_OUT.load(Ordering::Relaxed) > before_out, "the dial is refused before connecting");
    assert_eq!(t.peers_connected.load(Ordering::Relaxed), 0);

    f::install(None);
    q.delete(&hash).await;
}
