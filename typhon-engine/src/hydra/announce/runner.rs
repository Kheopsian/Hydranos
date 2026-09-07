//! Wiring the announcer to an engine.
//!
//! The scheduler decides *when*, the policy decides *what*, the engine's
//! tracker module puts it on the wire. This is the part that knows all three.

use std::sync::Arc;
use std::time::Duration;

use typhon_engine::torrent::TorrentManager;

use super::breaker::Breaker;
use super::cache::{Cache, Entry};
use super::overrides::override_host;
use super::policy::{self, Policy};
use super::scheduler::{self, Catalogue, Job, Outcome};

/// How an engine announces.
///
/// The two are not a tuning difference, they are different jobs. A hoard holds
/// a quarter of a million complete torrents and wants to be known cheaply: the
/// first tracker that answers is enough. A race is downloading something now,
/// against other people downloading the same thing, and wants every swarm it
/// belongs to -- so it announces to all of its trackers and dials what they
/// return.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Hoard,
    Race,
}

/// Race phase one: every 5 seconds for the first minute after the torrent
/// appears. A race is decided in that minute.
const RACE_FAST: Duration = Duration::from_secs(5);
const RACE_FAST_FOR: Duration = Duration::from_secs(60);
/// Race phase two: every 30 seconds while it is still downloading.
///
/// The rule used to be "stop once we have a peer", which killed the loop on the
/// first announce: a private-tracker race rarely has more than five peers at
/// all. It keeps feeding the swarm instead.
const RACE_SUSTAINED: Duration = Duration::from_secs(30);

/// The engine's live torrent list, seen by the scheduler.
struct EngineCatalogue {
    manager: Arc<TorrentManager>,
}

impl Catalogue for EngineCatalogue {
    fn hashes(&self) -> Vec<String> {
        self.manager
            .all()
            .iter()
            // A paused torrent is not announced. Telling a tracker we are a
            // peer for something we will not serve earns a connection attempt
            // from every leecher and answers none of them.
            .filter(|t| !t.is_paused.load(std::sync::atomic::Ordering::Relaxed))
            .map(|t| hex(&t.info_hash))
            .collect()
    }
}

fn hex(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Start announcing this engine's torrents. Returns immediately; the scheduler
/// and its workers outlive the call.
pub fn start(
    manager: Arc<TorrentManager>,
    policy: Policy,
    port: u16,
    mode: Mode,
    cache: Arc<Cache>,
) {
    let catalogue = Arc::new(EngineCatalogue { manager: manager.clone() });
    let policy = Arc::new(policy);
    // One breaker for the engine, not one per torrent: an outage belongs to the
    // host, and every torrent listing it has to learn from the same evidence.
    let breaker = Arc::new(Breaker::default());

    let announce = Arc::new(move |job: Job| {
        let manager = manager.clone();
        let policy = policy.clone();
        let breaker = breaker.clone();
        let cache = cache.clone();
        async move { announce_one(&manager, &policy, &breaker, &cache, port, mode, job).await }
    });

    tokio::spawn(async move {
        scheduler::run(catalogue, announce).await;
    });
}

/// One torrent, every tracker it carries, in tier order.
///
/// Stops at the first tier that answers: that is what a tier is for. Walking
/// all of them would announce the same torrent several times over and count
/// the upload twice on trackers that share a swarm.
async fn announce_one(
    manager: &Arc<TorrentManager>,
    policy: &Policy,
    breaker: &Breaker,
    cache: &Cache,
    port: u16,
    mode: Mode,
    job: Job,
) -> Outcome {
    let gone = Outcome { info_hash: job.info_hash.clone(), next_in: Duration::ZERO, gone: true };

    let Some(hash) = parse_hex(&job.info_hash) else {
        return gone;
    };
    let Some(torrent) = manager.get(&hash) else {
        cache.forget(&job.info_hash);
        return gone;
    };

    use std::sync::atomic::Ordering;
    let uploaded = torrent.total_uploaded.load(Ordering::Relaxed) as i64;
    let downloaded = torrent.total_downloaded.load(Ordering::Relaxed) as i64;
    let left = (torrent.meta.total_size as i64 - downloaded).max(0);
    // "started" is only right the first time a tracker hears about a torrent.
    // Sending it on every announce makes a tracker reset its view of us, and
    // some read it as a client that restarts in a loop.
    let event = if job.first { "started" } else { "" };

    let mut interval = Duration::from_secs(30 * 60);
    let mut announced_at_all = false;
    for tier in &torrent.meta.trackers {
        let mut tier_answered = false;
        for tracker_url in tier {
            let host = override_host(tracker_url);
            if !breaker.allows(&host, std::time::Instant::now()) {
                continue;
            }
            let Some(req) = policy::prepare(
                policy,
                tracker_url,
                &job.info_hash,
                port,
                uploaded,
                downloaded,
                left,
                event,
            ) else {
                continue;
            };
            match typhon_engine::tracker::http::send_announce(&req.url, &req.user_agent).await {
                Ok(resp) => {
                    breaker.record(&host, true, std::time::Instant::now());
                    if let Some(secondary) = req.secondary_url {
                        typhon_engine::tracker::http::spawn_secondary_announce(secondary);
                    }
                    if resp.interval > 0 {
                        interval = Duration::from_secs(resp.interval as u64);
                    }
                    // The swarm counts only exist here. Nothing else in the
                    // process can tell how many seeders a parked torrent has.
                    cache.record(
                        &job.info_hash,
                        Entry {
                            complete: resp.complete as i64,
                            incomplete: resp.incomplete as i64,
                            tracker: tracker_url.clone(),
                            at: std::time::Instant::now(),
                            interval,
                        },
                    );
                    announced_at_all = true;
                    // The peers a tracker returns are only worth asking for if
                    // something dials them. The engine's queue is where the DHT
                    // puts its finds too, so they share one dial budget.
                    for peer in &resp.peers {
                        typhon_engine::tracker::enqueue_dial(*peer, torrent.clone());
                    }
                    tier_answered = true;
                    // A race stays in every swarm it belongs to: a cross-seeded
                    // torrent announced only to its first tracker is absent
                    // from the others, which is where its peers are.
                    if mode == Mode::Hoard {
                        break;
                    }
                }
                Err(e) => {
                    breaker.record(&host, false, std::time::Instant::now());
                    // The host, never the URL: a tracker URL carries the
                    // passkey in its path, and that is an account credential.
                    // Logs get pasted into issues and shipped in bug reports.
                    tracing::debug!(tracker = %host, error = %e, "announce failed");
                }
            }
        }
        if tier_answered && mode == Mode::Hoard {
            break;
        }
    }

    let next_in = match mode {
        Mode::Hoard => interval,
        // A complete race torrent is a seed like any other and falls back to
        // what the tracker asked for.
        Mode::Race if left == 0 => interval,
        Mode::Race if job.first => RACE_FAST,
        Mode::Race => {
            if announced_at_all && torrent.total_uploaded.load(Ordering::Relaxed) > 0 {
                RACE_SUSTAINED
            } else {
                RACE_FAST
            }
        }
    };

    Outcome { info_hash: job.info_hash, next_in, gone: false }
}

fn parse_hex(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_survives_the_round_trip() {
        let raw = [0xabu8; 20];
        assert_eq!(parse_hex(&hex(&raw)), Some(raw));
        assert_eq!(parse_hex("short"), None);
        assert_eq!(parse_hex(&"zz".repeat(20)), None);
    }
}
