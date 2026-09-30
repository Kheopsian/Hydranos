//! Interoperability with real tracker software.
//!
//! Everything else in the suite talks to trackers we wrote for the purpose,
//! which proves the client agrees with our reading of the BEPs. These talk to
//! trackers somebody else wrote -- opentracker, the most deployed public
//! tracker, and Torrust in private mode, with keys -- and read back from THEIR
//! state what they understood: the scrape for opentracker, the peer table of
//! the REST API for Torrust.
//!
//! `#[ignore]`d because they need those trackers running. `tools/interop/run.sh`
//! starts them, runs these with `--ignored`, and stops them:
//!
//! ```sh
//! tools/interop/run.sh
//! ```
//!
//! Every test drives the real `announce_one` -- the book, the policy, the URL
//! builder, the HTTP client -- on a real torrent in a real `TorrentManager`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use typhon_engine::torrent::meta::{TorrentState, TorrentStatus, ANNOUNCE_EVENT_COMPLETED};
use typhon_engine::torrent::TorrentManager;

use super::breaker::Breaker;
use super::cache::Cache;
use super::policy::Policy;
use super::runner::{announce_one, Mode};
use super::scheduler::Job;

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("{key} is not set -- run tools/interop/run.sh"))
}

struct Fixture {
    mgr: Arc<TorrentManager>,
    root: std::path::PathBuf,
    policy: Policy,
    breaker: Breaker,
    cache: Cache,
    peer_id: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A manager, and the identity production would present: the real
/// fingerprint for this version, the real User-Agent.
fn fixture(tag: &str) -> Fixture {
    typhon_engine::config::set_version(crate::api::HYDRANOS_VERSION);
    let root = std::env::temp_dir().join(format!("hydra-interop-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("resume")).unwrap();
    let mgr = Arc::new(TorrentManager::new(
        root.join("data").to_string_lossy().into_owned(),
        root.join("resume").to_string_lossy().into_owned(),
        Arc::new(typhon_engine::disk::DiskManager::new(16)),
    ));
    let cfg: typhon_engine::config::EngineConfig = serde_json::from_str("{}").unwrap();
    let peer_id = String::from_utf8(cfg.peer_id().to_vec()).unwrap();
    let policy = super::policy_from_config(&crate::config::Config::default(), peer_id.clone(), String::new());
    Fixture { mgr, root, policy, breaker: Breaker::default(), cache: Cache::default(), peer_id }
}

/// A one-piece torrent with a name nobody else will have used, so every run
/// gets a fresh info hash and a tracker never mixes two runs.
fn add(fx: &Fixture, name: &str, tracker: &str, seed: bool) -> (String, Arc<TorrentState>) {
    let unique = format!(
        "{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    );
    let mut info = Vec::new();
    info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{unique}", unique.len()).as_bytes());
    info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
    info.extend_from_slice(&[0x5Au8; 20]);
    info.push(b'e');
    let mut bytes = format!("d8:announce{}:{tracker}4:info", tracker.len()).into_bytes();
    bytes.extend_from_slice(&info);
    bytes.push(b'e');
    let (ih, _) = fx.mgr.add_torrent_bytes(&bytes, "/tmp", false, seed).expect("fixture parses");
    let st = fx.mgr.get(&ih).unwrap();
    *st.live_trackers.write() = vec![vec![tracker.to_string()]];
    st.status.store(
        if seed { TorrentStatus::Seeding } else { TorrentStatus::Downloading } as u8,
        Ordering::Relaxed,
    );
    (typhon_engine::torrent::hex_encode(&ih), st)
}

async fn announce(fx: &Fixture, hash: &str, mode: Mode) {
    announce_one(
        &fx.mgr, &fx.policy, &fx.breaker, &fx.cache, 16371, mode,
        Job { info_hash: hash.to_string(), first: false },
    )
    .await;
}

/// What the download path does when the last piece verifies: the piece is
/// held, the torrent seeds, and `completed` is owed.
fn finish(st: &TorrentState) {
    if let Some(p) = st.picker.get() {
        p.lock().unwrap().set_have(0);
    }
    st.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
    st.pending_announce_event.fetch_or(ANNOUNCE_EVENT_COMPLETED, Ordering::Relaxed);
}

/// Time passing, as far as the floor is concerned. The tests would otherwise
/// wait out a real `min interval` -- 13 minutes on opentracker, 15 here.
fn let_the_interval_pass(st: &TorrentState) {
    for slot in st.announce_book.lock().unwrap().iter_mut() {
        slot.not_before = 0;
    }
}

fn raw_hash(hex: &str) -> String {
    hex.as_bytes()
        .chunks(2)
        .map(|p| format!("%{}", std::str::from_utf8(p).unwrap().to_ascii_uppercase()))
        .collect()
}

// ---------------------------------------------------------------------------
// opentracker: read back through its scrape
// ---------------------------------------------------------------------------

/// `(complete, incomplete, downloaded)` as opentracker reports them.
///
/// Read off the bytes rather than through our bencode reader, whose
/// dictionaries key on UTF-8 text: the scrape's `files` dictionary is keyed by
/// the raw 20-byte hash. One torrent per scrape, so each count appears once.
async fn scrape(announce_url: &str, hash: &str) -> (i64, i64, i64) {
    let url = format!("{}?info_hash={}", announce_url.replace("/announce", "/scrape"), raw_hash(hash));
    let body = reqwest::get(&url).await.expect("scrape answered").bytes().await.unwrap();
    let text = String::from_utf8_lossy(&body);
    let n = |key: &str| -> i64 {
        let tag = format!("{}:{}i", key.len(), key);
        text.find(&tag)
            .map(|i| &text[i + tag.len()..])
            .and_then(|rest| rest.split('e').next())
            .and_then(|d| d.parse().ok())
            .unwrap_or(0)
    };
    (n("complete"), n("incomplete"), n("downloaded"))
}

/// ⭐⭐⭐ A download's whole life as opentracker records it: a leecher
/// arrives, completes -- one snatch -- is stopped and leaves, resumes and
/// comes back as a seed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs opentracker: tools/interop/run.sh"]
async fn interop_opentracker_records_arrival_snatch_departure_and_return() {
    let url = env("HYDRANOS_INTEROP_OPENTRACKER");
    let fx = fixture("ot");
    let (hash, st) = add(&fx, "ot-life", &url, false);

    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(scrape(&url, &hash).await, (0, 1, 0), "one leecher after `started`");

    finish(&st);
    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(scrape(&url, &hash).await, (1, 0, 1), "a seed, and one snatch, after `completed`");

    fx.mgr.stop_torrent(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(scrape(&url, &hash).await, (0, 0, 1), "gone after `stopped`; the snatch stays");

    fx.mgr.start_torrent(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(scrape(&url, &hash).await, (1, 0, 1), "back as a seed, and no second snatch");
}

/// A torrent added complete is a seed from its first announce and is never
/// counted as a snatch -- a cross-seed must not inflate the tracker's count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs opentracker: tools/interop/run.sh"]
async fn interop_opentracker_a_cross_seed_is_never_a_snatch() {
    let url = env("HYDRANOS_INTEROP_OPENTRACKER");
    let fx = fixture("ot-xseed");
    let (hash, st) = add(&fx, "ot-xseed", &url, true);
    announce(&fx, &hash, Mode::Hoard).await;
    let_the_interval_pass(&st);
    announce(&fx, &hash, Mode::Hoard).await;
    assert_eq!(scrape(&url, &hash).await, (1, 0, 0));
}

/// Its `min interval` holds: asked again at once, we send nothing, and the
/// tracker's view does not move.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs opentracker: tools/interop/run.sh"]
async fn interop_opentracker_min_interval_is_honoured() {
    let url = env("HYDRANOS_INTEROP_OPENTRACKER");
    let fx = fixture("ot-floor");
    let (hash, st) = add(&fx, "ot-floor", &url, true);
    announce(&fx, &hash, Mode::Race).await;
    let floor = st.announce_book.lock().unwrap()[0].not_before - typhon_engine::torrent::meta::now_secs();
    assert!(floor > 60, "opentracker states a min interval, and we keep it: {floor}s");
    let before = fx.cache.outcomes();
    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(fx.cache.outcomes(), before, "no request inside the floor, race or not");
}

// ---------------------------------------------------------------------------
// Torrust, private mode: read back through its REST API
// ---------------------------------------------------------------------------

struct Torrust {
    announce_base: String,
    api: String,
    token: String,
}

fn torrust() -> Torrust {
    Torrust {
        announce_base: env("HYDRANOS_INTEROP_TORRUST"),
        api: env("HYDRANOS_INTEROP_TORRUST_API"),
        token: env("HYDRANOS_INTEROP_TORRUST_TOKEN"),
    }
}

impl Torrust {
    /// A fresh user key: the private tracker's passkey.
    async fn key(&self) -> String {
        let url = format!("{}/api/v1/key/3600?token={}", self.api, self.token);
        let v: serde_json::Value = reqwest::Client::new().post(&url).send().await.unwrap().json().await.unwrap();
        v["key"].as_str().expect("a key").to_string()
    }

    async fn torrent(&self, hash: &str) -> serde_json::Value {
        let url = format!("{}/api/v1/torrent/{}?token={}", self.api, hash, self.token);
        let resp = reqwest::get(&url).await.unwrap();
        if !resp.status().is_success() {
            return serde_json::json!({ "peers": [], "completed": 0 });
        }
        resp.json().await.unwrap()
    }
}

fn peer_id_hex(id: &str) -> String {
    let mut s = String::from("0x");
    for b in id.bytes() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// ⭐⭐⭐ What a private tracker stores about us is exactly what we meant:
/// our peer id, `started` with zero counters, then the session's upload, then
/// nothing at all inside its `min interval`, then gone on `stopped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Torrust: tools/interop/run.sh"]
async fn interop_torrust_private_tracker_stores_exactly_what_we_report() {
    let t = torrust();
    let url = format!("{}/{}", t.announce_base, t.key().await);
    let fx = fixture("torrust");
    let (hash, st) = add(&fx, "tr-session", &url, true);
    // A lifetime total from earlier sessions: must never reach the tracker.
    st.total_uploaded.store(900_000_000_000, Ordering::Relaxed);
    st.begin_announce_session();

    announce(&fx, &hash, Mode::Hoard).await;
    let v = t.torrent(&hash).await;
    let peer = &v["peers"][0];
    assert_eq!(peer["peer_id"]["id"], peer_id_hex(&fx.peer_id), "{v}");
    assert_eq!(peer["event"], "Started", "{v}");
    assert_eq!(peer["uploaded"], 0, "a new session reports from zero: {v}");
    assert_eq!(peer["left"], 0, "{v}");

    st.total_uploaded.fetch_add(12_345, Ordering::Relaxed);
    let_the_interval_pass(&st);
    announce(&fx, &hash, Mode::Hoard).await;
    let v = t.torrent(&hash).await;
    assert_eq!(v["peers"][0]["uploaded"], 12_345, "{v}");
    let stamp = v["peers"][0]["updated"].clone();

    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(t.torrent(&hash).await["peers"][0]["updated"], stamp, "inside min interval, the tracker hears nothing");

    fx.mgr.stop_torrent(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
    announce(&fx, &hash, Mode::Hoard).await;
    let v = t.torrent(&hash).await;
    assert_eq!(v["peers"].as_array().map(|a| a.len()).unwrap_or(0), 0, "gone after stopped: {v}");
}

/// A download completed on a private tracker is counted once -- the snatch
/// the tracker's ratio system is built on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Torrust: tools/interop/run.sh"]
async fn interop_torrust_counts_the_snatch_once() {
    let t = torrust();
    let url = format!("{}/{}", t.announce_base, t.key().await);
    let fx = fixture("torrust-snatch");
    let (hash, st) = add(&fx, "tr-snatch", &url, false);

    announce(&fx, &hash, Mode::Race).await;
    let v = t.torrent(&hash).await;
    assert_eq!(v["peers"][0]["left"], 16384, "a leecher reports what it needs: {v}");

    st.total_downloaded.fetch_add(16384, Ordering::Relaxed);
    finish(&st);
    announce(&fx, &hash, Mode::Race).await;
    let v = t.torrent(&hash).await;
    assert_eq!(v["completed"], 1, "{v}");
    assert_eq!(v["peers"][0]["downloaded"], 16384, "{v}");
    assert_eq!(v["peers"][0]["left"], 0, "{v}");

    let_the_interval_pass(&st);
    announce(&fx, &hash, Mode::Race).await;
    assert_eq!(t.torrent(&hash).await["completed"], 1, "said once, counted once");
}

/// Without a valid key a private tracker refuses us, we say so to the
/// operator, and nothing is registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs Torrust: tools/interop/run.sh"]
async fn interop_torrust_refuses_an_announce_without_a_key_and_we_report_it() {
    let t = torrust();
    let fx = fixture("torrust-nokey");
    let (hash, st) = add(&fx, "tr-nokey", &t.announce_base, true);
    announce(&fx, &hash, Mode::Hoard).await;
    assert!(!st.last_announce_ok.load(Ordering::Relaxed));
    let err = st.last_announce_error.lock().unwrap().clone();
    assert!(err.to_ascii_lowercase().contains("authentication"), "the tracker's own words reach the operator: {err}");
    assert_eq!(t.torrent(&hash).await["peers"].as_array().map(|a| a.len()).unwrap_or(0), 0);
}
