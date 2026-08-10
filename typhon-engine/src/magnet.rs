//! Magnet resolution: info hash in, raw info dict out.
//!
//! Runs as a background job rather than an RPC call. Resolution takes seconds
//! to minutes (announce, DHT lookup, then BEP 9 against whichever peer answers
//! first) and the RPC dispatch loop is shared by every call on the socket, so
//! blocking in it would stall the whole control plane. Callers kick off a job
//! and poll it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
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

static JOBS: OnceLock<Mutex<HashMap<[u8; 20], Job>>> = OnceLock::new();

fn jobs() -> &'static Mutex<HashMap<[u8; 20], Job>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn set_state(info_hash: [u8; 20], state: JobState) {
    if let Ok(mut map) = jobs().lock() {
        if let Some(job) = map.get_mut(&info_hash) {
            job.state = state;
        }
    }
}

/// Current state of a resolution, if we know about one.
pub fn state_of(info_hash: &[u8; 20]) -> Option<JobState> {
    let map = jobs().lock().ok()?;
    let job = map.get(info_hash)?;
    // A job that blew its ceiling reports as failed rather than resolving
    // forever; the caller can start a fresh one.
    if matches!(job.state, JobState::Resolving) && job.started.elapsed() > JOB_TIMEOUT {
        return Some(JobState::Failed("resolution timed out".into()));
    }
    Some(job.state.clone())
}

/// Forget a job, so its dict stops occupying memory once collected.
pub fn forget(info_hash: &[u8; 20]) {
    if let Ok(mut map) = jobs().lock() {
        map.remove(info_hash);
    }
}

/// Start resolving, unless a job for this info hash is already alive.
/// Returns false when one was already running (or finished and uncollected).
pub fn start(
    info_hash: [u8; 20],
    trackers: Vec<String>,
    seed_peers: Vec<SocketAddr>,
    config: &EngineConfig,
    binding_id: Option<u32>,
) -> bool {
    {
        let mut map = match jobs().lock() {
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
    let (peer_id, fwmark, port) = match binding {
        Some(b) => (b.peer_id, b.fwmark, b.advertised_port),
        None => {
            set_state(info_hash, JobState::Failed("no usable network binding".into()));
            return true;
        }
    };

    tokio::spawn(async move {
        let peers = discover(info_hash, &trackers, seed_peers, &peer_id, port).await;
        if peers.is_empty() {
            warn!("[magnet] no peers found for {}", hex(&info_hash));
            set_state(info_hash, JobState::Failed("no peers found".into()));
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
            fwmark,
            crate::peer::metadata::DEFAULT_CONCURRENCY,
        )
        .await;
        match result {
            Ok(dict) => {
                info!("[magnet] resolved {} ({} bytes)", hex(&info_hash), dict.len());
                set_state(info_hash, JobState::Done(dict));
            }
            Err(e) => {
                warn!("[magnet] {} failed: {}", hex(&info_hash), e);
                set_state(info_hash, JobState::Failed(e));
            }
        }
    });
    true
}

/// Collect peer candidates from the magnet's trackers and the DHT.
async fn discover(
    info_hash: [u8; 20],
    trackers: &[String],
    seed_peers: Vec<SocketAddr>,
    peer_id: &[u8; 20],
    port: u16,
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
        dht_peers(info_hash, &mut out, &mut seen).await;
    }
    out
}

/// Ask the DHT for peers, bounded by the discovery budget.
async fn dht_peers(
    info_hash: [u8; 20],
    out: &mut Vec<SocketAddr>,
    seen: &mut std::collections::HashSet<SocketAddr>,
) {
    let Some(dht) = crate::dht::handle() else { return };
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
