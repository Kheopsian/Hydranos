//! Magnet resolution: info hash in, raw info dict out.
//!
//! Runs as a background job rather than an RPC call. Resolution takes seconds
//! to minutes (announce, DHT lookup, then BEP 9 against whichever peer answers
//! first) and the RPC dispatch loop is shared by every call on the socket, so
//! blocking in it would stall the whole control plane. Callers kick off a job
//! and poll it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use librqbit_dht::Id20;
use tracing::{debug, info, warn};

use crate::config::EngineConfig;

/// How long we spend collecting peers before giving up on discovery.
pub const DISCOVERY_BUDGET: Duration = Duration::from_secs(20);

/// Enough peers to have a good chance one carries the dict; past this, more
/// candidates just cost dials.
pub const MAX_PEERS: usize = 100;

/// Whole-job ceiling, so a job cannot sit in `Resolving` forever.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(180);

/// How long a job keeps starting new rounds. Under `JOB_TIMEOUT`, so the last
/// round (discovery plus a fetch wave) ends before the job is declared dead.
pub const RESOLVE_BUDGET: Duration = Duration::from_secs(120);

/// Between two rounds.
const ROUND_PAUSE: Duration = Duration::from_secs(3);

/// Between two asks to the trackers and the DHT within one job.
const DISCOVERY_EVERY: Duration = Duration::from_secs(30);

/// We cannot know the torrent's size before we have the dict, which is the
/// point of the exercise. Trackers only read `left` to tell a leecher from a
/// seeder, and a magnet is always a leecher.
const UNKNOWN_LEFT: u64 = 16384;

#[derive(Clone, Debug)]
pub enum JobState {
    Resolving,
    Done(Vec<u8>),
    Failed(String),
}

struct Job {
    state: JobState,
    started: Instant,
}

/// The magnet resolutions in flight for ONE engine.
///
/// Keyed by info hash, which was unambiguous while one engine meant one
/// process. With two engines sharing a process, the same magnet added to both
/// collided: the second `start` saw a live job and returned false, so that
/// engine never resolved the magnet and never reported a failure either.
#[derive(Default)]
pub struct MagnetJobs {
    map: Mutex<HashMap<[u8; 20], Job>>,
    /// The engine's peer policy, so the peers a resolution dials follow
    /// `peer_encryption` like the torrent's own dials do.
    policy: Arc<crate::peer::extension::PeerPolicy>,
}

impl MagnetJobs {
    /// Jobs whose peer dials follow `policy` (the engine's).
    pub fn with_policy(policy: Arc<crate::peer::extension::PeerPolicy>) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            policy,
        }
    }

    fn set_state(&self, info_hash: [u8; 20], state: JobState) {
        if let Ok(mut map) = self.map.lock() {
            if let Some(job) = map.get_mut(&info_hash) {
                job.state = state;
            }
        }
    }

    /// Current state of a resolution, if we know about one.
    pub fn state_of(&self, info_hash: &[u8; 20]) -> Option<JobState> {
        let map = self.map.lock().ok()?;
        let job = map.get(info_hash)?;
        // A job that blew its ceiling reports as failed rather than resolving
        // forever; the caller can start a fresh one.
        if matches!(job.state, JobState::Resolving) && job.started.elapsed() > JOB_TIMEOUT {
            return Some(JobState::Failed("resolution timed out".into()));
        }
        Some(job.state.clone())
    }

    /// Forget a job, so its dict stops occupying memory once collected.
    pub fn forget(&self, info_hash: &[u8; 20]) {
        if let Ok(mut map) = self.map.lock() {
            map.remove(info_hash);
        }
    }

    /// Start resolving, unless a job for this info hash is already alive.
    /// Returns false when one was already running (or finished and uncollected).
    pub fn start(
        self: &Arc<Self>,
        info_hash: [u8; 20],
        trackers: Vec<String>,
        seed_peers: Vec<SocketAddr>,
        config: &EngineConfig,
        binding_id: Option<u32>,
        // The engine's DHT node, if it has one. Passed in rather than read from a
        // global: with two engines in one process, a magnet must be resolved
        // through the node of the engine it was added to.
        dht: Option<librqbit_dht::Dht>,
    ) -> bool {
        {
            let mut map = match self.map.lock() {
                Ok(m) => m,
                Err(_) => return false,
            };
            if let Some(existing) = map.get(&info_hash) {
                if !matches!(existing.state, JobState::Resolving)
                    || existing.started.elapsed() <= JOB_TIMEOUT
                {
                    return false;
                }
            }
            map.insert(
                info_hash,
                Job {
                    state: JobState::Resolving,
                    started: Instant::now(),
                },
            );
        }

        // Pick the binding this resolution goes out on, so its peer dials carry
        // the engine's egress -- `bind_device` and the outbound SOCKS5 -- like the
        // torrent's own: dialling with none would leave by the default route and
        // show our real address to the whole swarm. By default the same binding
        // the torrent will land on. (Its fwmark is not applied to peer dials, and
        // Hydranos never sets one: see `Binding::fwmark`.)
        let bindings = config.resolved_bindings();
        let binding = match binding_id {
            Some(want) => bindings
                .iter()
                .find(|b| b.id == want)
                .or_else(|| bindings.first()),
            None => bindings.first(),
        };
        let (peer_id, egress, port) = match binding {
            Some(b) => (b.peer_id, b.egress.clone(), b.advertised_port),
            None => {
                self.set_state(
                    info_hash,
                    JobState::Failed("no usable network binding".into()),
                );
                return true;
            }
        };

        // The magnet's announces go where the engine's do: through its proxy
        // when it has one. A magnet announced directly would hand the tracker
        // this host's address before the torrent even exists.
        let http_proxy = config.http_proxy();
        let ip_mode = announce_ip_mode(config.enable_ipv6);
        let refuse_cleartext_key = config
            .announce_cleartext_passkey
            .trim()
            .eq_ignore_ascii_case("refuse");
        let jobs = self.clone();
        let policy = self.policy.clone();
        tokio::spawn(async move {
            // Rounds until the budget runs out, not one. A peer that is still
            // checking the torrent refuses the connection (libtorrent does, for
            // seconds after an add), a tracker answers late, the DHT fills in
            // slowly: one round made each of those a failure, and the magnet
            // waited a minute for its next chance.
            let started = Instant::now();
            let mut last = String::from("no peers found");
            let mut round = 0u32;
            // Peers found so far. Asked again every round; the trackers and the
            // DHT only every DISCOVERY_EVERY -- each tracker ask is an announce,
            // and a magnet must not announce `started` to a tracker every three
            // seconds.
            let mut peers: Vec<SocketAddr> = seed_peers.clone();
            let mut discovered_at: Option<Instant> = None;
            while started.elapsed() < RESOLVE_BUDGET {
                round += 1;
                if discovered_at.is_none_or(|t| t.elapsed() >= DISCOVERY_EVERY) {
                    discovered_at = Some(Instant::now());
                    for p in discover(
                        info_hash,
                        &trackers,
                        seed_peers.clone(),
                        &peer_id,
                        port,
                        dht.clone(),
                        &egress.device,
                        &http_proxy,
                        ip_mode,
                        refuse_cleartext_key,
                    )
                    .await
                    {
                        if !peers.contains(&p) {
                            peers.push(p);
                        }
                    }
                }
                if peers.is_empty() {
                    last = "no peers found".into();
                } else {
                    debug!(
                        "[magnet] round {} for {}: {} candidate peers",
                        round,
                        hex(&info_hash),
                        peers.len()
                    );
                    match crate::peer::metadata::fetch(
                        &peers,
                        info_hash,
                        peer_id,
                        // uTP is skipped for resolution: it is a short, one-shot
                        // exchange and the TCP legs cover it.
                        None,
                        port,
                        egress.clone(),
                        policy.clone(),
                        crate::peer::metadata::DEFAULT_CONCURRENCY,
                    )
                    .await
                    {
                        Ok(dict) => {
                            info!(
                                "[magnet] resolved {} ({} bytes, round {})",
                                hex(&info_hash),
                                dict.len(),
                                round
                            );
                            jobs.set_state(info_hash, JobState::Done(dict));
                            return;
                        }
                        Err(e) => last = e,
                    }
                }
                tokio::time::sleep(ROUND_PAUSE).await;
            }
            warn!(
                "[magnet] {} failed after {} rounds: {}",
                hex(&info_hash),
                round,
                last
            );
            jobs.set_state(info_hash, JobState::Failed(last));
        });
        true
    }
}

/// The address family a magnet's announces use: both only when the engine
/// listens on IPv6, v4 alone otherwise -- as its torrents' announces do.
fn announce_ip_mode(enable_ipv6: bool) -> crate::tracker::http::IpMode {
    if enable_ipv6 {
        crate::tracker::http::IpMode::Auto
    } else {
        crate::tracker::http::IpMode::V4
    }
}

/// Collect peer candidates from the magnet's trackers and the DHT.
// Every argument is a distinct input the caller already holds; a parameter struct would only move the list.
#[allow(clippy::too_many_arguments)]
async fn discover(
    info_hash: [u8; 20],
    trackers: &[String],
    seed_peers: Vec<SocketAddr>,
    peer_id: &[u8; 20],
    port: u16,
    dht: Option<librqbit_dht::Dht>,
    // The engine's interface: the magnet's announces leave by it, as its
    // metadata dials already did. 4.3 announced magnets by the default route.
    device: &str,
    // The engine's announce proxy (`EngineConfig::http_proxy`).
    proxy: &str,
    // The family the announces keep to: the engine's own announces are v4
    // only unless `enable_ipv6`, and a magnet's must not be the exception.
    ip_mode: crate::tracker::http::IpMode,
    // `announce_cleartext_passkey = "refuse"`: a plain http:// tracker whose
    // URL carries a key is not contacted.
    refuse_cleartext_key: bool,
) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in seed_peers {
        if seen.insert(p) {
            out.push(p);
        }
    }

    for url in trackers {
        if refuse_cleartext_key && crate::tracker::carries_key_in_clear(url) {
            debug!(
                "[magnet] tracker {} skipped: its URL carries a key over plain http://",
                log_host(url)
            );
            continue;
        }
        let asked = if crate::tracker::udp::is_udp(url) {
            // Most public magnets list only UDP trackers.
            let a = crate::tracker::udp::UdpAnnounce {
                tracker: url.clone(),
                info_hash,
                peer_id: *peer_id,
                downloaded: 0,
                left: UNKNOWN_LEFT,
                uploaded: 0,
                event: crate::tracker::udp::event_code("started"),
                ip: 0,
                key: u32::from_str_radix(
                    &crate::tracker::http::announce_key(peer_id, &info_hash),
                    16,
                )
                .unwrap_or(0),
                num_want: 200,
                port,
            };
            crate::tracker::udp::send_announce_on(&a, ip_mode, device, proxy).await
        } else if url.starts_with("http://") || url.starts_with("https://") {
            crate::tracker::http::announce_on_mode(
                url,
                &info_hash,
                peer_id,
                port,
                0,
                0,
                UNKNOWN_LEFT,
                "started",
                device,
                proxy,
                ip_mode,
            )
            .await
        } else {
            continue;
        };
        match asked {
            Ok(resp) => {
                if let Some(f) = resp.failure {
                    debug!("[magnet] tracker {} refused: {}", log_host(url), f);
                    continue;
                }
                for p in resp.peers {
                    if seen.insert(p) {
                        out.push(p);
                    }
                }
            }
            Err(e) => debug!("[magnet] tracker {} failed: {}", log_host(url), e),
        }
        if out.len() >= MAX_PEERS {
            return out;
        }
    }

    if out.len() < MAX_PEERS {
        dht_peers(info_hash, &mut out, &mut seen, dht).await;
    }
    out
}

/// Ask the DHT for peers, bounded by the discovery budget.
async fn dht_peers(
    info_hash: [u8; 20],
    out: &mut Vec<SocketAddr>,
    seen: &mut std::collections::HashSet<SocketAddr>,
    dht: Option<librqbit_dht::Dht>,
) {
    let Some(dht) = dht else { return };
    let mut stream = dht.get_peers(Id20::new(info_hash), None);
    let deadline = tokio::time::sleep(DISCOVERY_BUDGET);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            next = stream.next() => match next {
                Some(addr) => {
                    if seen.insert(addr) {
                        out.push(addr);
                        if out.len() >= MAX_PEERS { break; }
                    }
                }
                None => break,
            },
        }
    }
}

/// The tracker's host, for a log line: the URL itself carries the passkey.
fn log_host(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "<tracker>".into())
}

/// Hex for arbitrary byte slices (the torrent one is fixed at 20 bytes).
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EngineConfig has no Default; every field carries a serde default, so an
    /// empty document is the configuration a fresh install runs with.
    fn test_config() -> crate::config::EngineConfig {
        toml::from_str("").expect("every EngineConfig field has a serde default")
    }

    /// A magnet follows the engine's family policy: with IPv6 off its announces
    /// are v4 only, like every other announce of the engine.
    #[test]
    fn a_magnet_announces_v4_only_while_ipv6_is_off() {
        use crate::tracker::http::IpMode;
        assert!(!test_config().enable_ipv6, "the default engine is v4 only");
        assert_eq!(announce_ip_mode(test_config().enable_ipv6), IpMode::V4);
        assert_eq!(announce_ip_mode(true), IpMode::Auto);
    }

    fn ih(n: u8) -> [u8; 20] {
        [n; 20]
    }

    fn jobs_with(state: JobState, age: Duration) -> Arc<MagnetJobs> {
        let jobs = Arc::new(MagnetJobs::default());
        jobs.map.lock().unwrap().insert(
            ih(1),
            Job {
                state,
                started: Instant::now() - age,
            },
        );
        jobs
    }

    #[test]
    fn hex_pads_every_byte_to_two_digits() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&ih(0xab)).len(), 40, "a 20-byte hash is 40 hex digits");
    }

    #[test]
    fn an_unknown_hash_has_no_state() {
        let jobs = Arc::new(MagnetJobs::default());
        assert!(jobs.state_of(&ih(9)).is_none());
    }

    #[test]
    fn a_live_job_reports_resolving() {
        let jobs = jobs_with(JobState::Resolving, Duration::from_secs(1));
        assert!(matches!(jobs.state_of(&ih(1)), Some(JobState::Resolving)));
    }

    /// A job that blew its ceiling must report failed rather than resolve
    /// forever: `Resolving` is what the UI spins on, and nothing else would
    /// ever stop it.
    #[test]
    fn a_job_past_its_ceiling_reports_failed_not_resolving() {
        let jobs = jobs_with(JobState::Resolving, JOB_TIMEOUT + Duration::from_secs(1));
        match jobs.state_of(&ih(1)) {
            Some(JobState::Failed(msg)) => assert!(msg.contains("timed out"), "{msg}"),
            other => panic!("expected a timeout failure, got {other:?}"),
        }
    }

    /// The ceiling applies to `Resolving` only. A dict that took longer than
    /// the ceiling to arrive is still a dict, and reporting it as a timeout
    /// would throw away work that succeeded.
    #[test]
    fn a_finished_job_is_not_retroactively_timed_out() {
        let jobs = jobs_with(JobState::Done(vec![1, 2, 3]), JOB_TIMEOUT * 2);
        match jobs.state_of(&ih(1)) {
            Some(JobState::Done(d)) => assert_eq!(d, vec![1, 2, 3]),
            other => panic!("expected the resolved dict, got {other:?}"),
        }
    }

    #[test]
    fn forgetting_a_job_releases_its_dict() {
        let jobs = jobs_with(JobState::Done(vec![0u8; 64]), Duration::from_secs(0));
        assert!(jobs.state_of(&ih(1)).is_some());
        jobs.forget(&ih(1));
        assert!(
            jobs.state_of(&ih(1)).is_none(),
            "a collected job stops occupying memory"
        );
    }

    #[test]
    fn forgetting_a_job_that_never_existed_is_not_an_error() {
        let jobs = Arc::new(MagnetJobs::default());
        jobs.forget(&ih(7));
    }

    /// `set_state` addresses a job by hash; with no such job there is nothing
    /// to write, and inventing one would resurrect a resolution the caller
    /// already collected.
    #[test]
    fn setting_the_state_of_a_forgotten_job_does_not_recreate_it() {
        let jobs = jobs_with(JobState::Resolving, Duration::from_secs(0));
        jobs.forget(&ih(1));
        jobs.set_state(ih(1), JobState::Done(vec![9]));
        assert!(jobs.state_of(&ih(1)).is_none());
    }

    /// Two engines in one process share no job map, but within ONE engine a
    /// second start on a live hash must not displace the first.
    #[test]
    fn a_second_start_on_a_live_job_is_refused() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let jobs = jobs_with(JobState::Resolving, Duration::from_secs(1));
        let cfg = test_config();
        assert!(
            !jobs.start(ih(1), vec![], vec![], &cfg, None, None),
            "a resolution already in flight is not restarted"
        );
        assert!(matches!(jobs.state_of(&ih(1)), Some(JobState::Resolving)));
    }

    /// A finished-but-uncollected job also refuses a restart: its dict is
    /// still owed to whoever asked for it.
    #[test]
    fn a_finished_uncollected_job_is_not_restarted() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let jobs = jobs_with(JobState::Done(vec![4]), JOB_TIMEOUT * 2);
        let cfg = test_config();
        assert!(!jobs.start(ih(1), vec![], vec![], &cfg, None, None));
    }

    /// Resolving always goes out on a binding: without one we would dial the
    /// swarm over the default route and show our real address. That is a
    /// reported failure, never a silent fallback.
    #[test]
    fn with_no_binding_the_job_fails_rather_than_dialling_in_the_clear() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let jobs = Arc::new(MagnetJobs::default());
        let cfg = test_config();
        if !cfg.resolved_bindings().is_empty() {
            return; // a default config that has a binding cannot exercise this
        }
        assert!(
            jobs.start(ih(2), vec![], vec![], &cfg, None, None),
            "the job was accepted"
        );
        match jobs.state_of(&ih(2)) {
            Some(JobState::Failed(msg)) => {
                assert!(
                    msg.contains("binding"),
                    "the reason names the binding: {msg}"
                )
            }
            other => panic!("expected a binding failure, got {other:?}"),
        }
    }

    /// A tracker that answers every announce with a refusal and notes it was
    /// asked: (its port, whether anything connected).
    async fn tracker() -> (u16, Arc<std::sync::atomic::AtomicBool>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = asked.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = c.read(&mut buf).await;
                let body = "d14:failure reason4:nopee";
                let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        (port, asked)
    }

    /// `announce_cleartext_passkey = "refuse"`: a plain http:// `tr=` that
    /// carries a key is not contacted; the other trackers of the magnet still
    /// are. Left at its default the same tracker is contacted as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refuse_keeps_a_magnet_from_announcing_to_a_plain_http_tracker_with_a_key() {
        for (mode, keyed_is_contacted) in [("", true), ("refuse", false)] {
            let (keyed_port, keyed) = tracker().await;
            let (plain_port, plain) = tracker().await;
            let trackers = vec![
                format!("http://127.0.0.1:{keyed_port}/announce?passkey=0123456789abcdef0123"),
                format!("http://127.0.0.1:{plain_port}/announce"),
            ];
            let cfg: crate::config::EngineConfig =
                toml::from_str(&format!("announce_cleartext_passkey = \"{mode}\""))
                    .expect("config");
            assert!(
                !cfg.resolved_bindings().is_empty(),
                "the job needs a binding to run"
            );
            let jobs = Arc::new(MagnetJobs::default());
            assert!(jobs.start(ih(7), trackers, vec![], &cfg, None, None));
            let seen =
                |f: &std::sync::atomic::AtomicBool| f.load(std::sync::atomic::Ordering::SeqCst);
            for _ in 0..50 {
                if seen(&plain) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(
                seen(&plain),
                "mode {mode:?}: a tracker without a key is announced to"
            );
            assert_eq!(
                seen(&keyed),
                keyed_is_contacted,
                "mode {mode:?}: the keyed http:// tracker"
            );
            jobs.forget(&ih(7));
        }
    }

    /// What a fake peer sees as the opening of each connection a magnet
    /// resolution makes to it: true = a plaintext BitTorrent handshake.
    async fn magnet_openings(mode: &str) -> Vec<bool> {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let peer = tcp.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = tcp.accept().await else {
                    return;
                };
                let log = log.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = [0u8; 20];
                    let n = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
                    log.lock()
                        .unwrap()
                        .push(matches!(n, Ok(Ok(20))) && buf == *b"\x13BitTorrent protocol");
                });
            }
        });
        let root =
            std::env::temp_dir().join(format!("typhon-magnet-enc-{mode}-{}", std::process::id()));
        let (data, resume) = (root.join("data"), root.join("resume"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(crate::torrent::TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(crate::disk::DiskManager::new(16)),
        ));
        mgr.policy().set_encryption(mode);
        assert!(mgr
            .magnet()
            .start(ih(9), vec![], vec![peer], &test_config(), None, None));
        // The first round dials at once; the fake peer drops every connection
        // after reading it, so the legs follow one another quickly.
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if seen.lock().unwrap().len() >= 2 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let v = seen.lock().unwrap().clone();
        let _ = std::fs::remove_dir_all(&root);
        v
    }

    /// A magnet resolution follows the engine's `peer_encryption`: under
    /// `require` the BEP 9 fetch never opens in the clear.
    #[tokio::test]
    async fn magnet_metadata_fetch_never_dials_plaintext_under_require() {
        let seen = magnet_openings("require").await;
        assert!(!seen.is_empty(), "the fetch dialled the peer");
        assert!(
            !seen.contains(&true),
            "a plaintext handshake went out: {seen:?}"
        );
    }

    /// Control: the default mode still opens in the clear first.
    #[tokio::test]
    async fn magnet_metadata_fetch_dials_plaintext_under_allow() {
        let seen = magnet_openings("allow").await;
        assert_eq!(seen.first(), Some(&true), "{seen:?}");
    }

    /// The budget exists so a resolution cannot sit forever; a ceiling below
    /// the discovery budget would cut discovery off before it ever reported.
    #[test]
    fn the_job_ceiling_is_wider_than_the_discovery_budget() {
        assert!(JOB_TIMEOUT > DISCOVERY_BUDGET);
        const { assert!(MAX_PEERS > 0) };
    }
}
