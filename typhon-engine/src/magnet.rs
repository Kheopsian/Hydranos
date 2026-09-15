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
}

impl MagnetJobs {
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
        map.insert(info_hash, Job { state: JobState::Resolving, started: Instant::now() });
    }

    // Pick the tunnel this resolution goes out on. Dialling peers with no
    // fwmark would leave via the default route and show our real address to
    // the whole swarm, so we always resolve on a binding -- by default the
    // same one the torrent will land on.
    let bindings = config.resolved_bindings();
    let binding = match binding_id {
        Some(want) => bindings.iter().find(|b| b.id == want).or_else(|| bindings.first()),
        None => bindings.first(),
    };
    let (peer_id, egress, port) = match binding {
        Some(b) => (b.peer_id, b.egress.clone(), b.advertised_port),
        None => {
            self.set_state(info_hash, JobState::Failed("no usable network binding".into()));
            return true;
        }
    };

    let jobs = self.clone();
    tokio::spawn(async move {
        let peers = discover(info_hash, &trackers, seed_peers, &peer_id, port, dht).await;
        if peers.is_empty() {
            warn!("[magnet] no peers found for {}", hex(&info_hash));
            jobs.set_state(info_hash, JobState::Failed("no peers found".into()));
            return;
        }
        debug!("[magnet] {} candidate peers for {}", peers.len(), hex(&info_hash));
        let result = crate::peer::metadata::fetch(
            &peers,
            info_hash,
            peer_id,
            // uTP is skipped for resolution: it is a short, one-shot exchange
            // and the TCP legs cover it. Peers reachable only over uTP simply
            // are not used as metadata sources.
            None,
            port,
            egress,
            crate::peer::metadata::DEFAULT_CONCURRENCY,
        )
        .await;
        match result {
            Ok(dict) => {
                info!("[magnet] resolved {} ({} bytes)", hex(&info_hash), dict.len());
                jobs.set_state(info_hash, JobState::Done(dict));
            }
            Err(e) => {
                warn!("[magnet] {} failed: {}", hex(&info_hash), e);
                jobs.set_state(info_hash, JobState::Failed(e));
            }
        }
    });
    true
}
}

/// Collect peer candidates from the magnet's trackers and the DHT.
async fn discover(
    info_hash: [u8; 20],
    trackers: &[String],
    seed_peers: Vec<SocketAddr>,
    peer_id: &[u8; 20],
    port: u16,
    dht: Option<librqbit_dht::Dht>,
) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for p in seed_peers {
        if seen.insert(p) {
            out.push(p);
        }
    }

    for url in trackers {
        // Only HTTP(S) for now -- Typhon has no UDP tracker client yet, so
        // udp:// entries in a magnet are skipped and the DHT covers them.
        if !url.starts_with("http://") && !url.starts_with("https://") {
            continue;
        }
        match crate::tracker::http::announce(
            url,
            &info_hash,
            peer_id,
            port,
            0,
            0,
            UNKNOWN_LEFT,
            "started",
        )
        .await
        {
            Ok(resp) => {
                if let Some(f) = resp.failure {
                    debug!("[magnet] tracker {} refused: {}", url, f);
                    continue;
                }
                for p in resp.peers {
                    if seen.insert(p) {
                        out.push(p);
                    }
                }
            }
            Err(e) => debug!("[magnet] tracker {} failed: {}", url, e),
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

    fn ih(n: u8) -> [u8; 20] {
        [n; 20]
    }

    fn jobs_with(state: JobState, age: Duration) -> Arc<MagnetJobs> {
        let jobs = Arc::new(MagnetJobs::default());
        jobs.map.lock().unwrap().insert(
            ih(1),
            Job { state, started: Instant::now() - age },
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
        assert!(jobs.state_of(&ih(1)).is_none(), "a collected job stops occupying memory");
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
        assert!(jobs.start(ih(2), vec![], vec![], &cfg, None, None), "the job was accepted");
        match jobs.state_of(&ih(2)) {
            Some(JobState::Failed(msg)) => {
                assert!(msg.contains("binding"), "the reason names the binding: {msg}")
            }
            other => panic!("expected a binding failure, got {other:?}"),
        }
    }

    /// The budget exists so a resolution cannot sit forever; a ceiling below
    /// the discovery budget would cut discovery off before it ever reported.
    #[test]
    fn the_job_ceiling_is_wider_than_the_discovery_budget() {
        assert!(JOB_TIMEOUT > DISCOVERY_BUDGET);
        assert!(MAX_PEERS > 0);
    }
}
