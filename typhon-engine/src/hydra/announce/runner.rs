//! Wiring the announcer to an engine.
//!
//! The scheduler decides *when*, the policy decides *what*, the engine's
//! tracker module puts it on the wire. This is the part that knows all three.

use std::sync::Arc;
use std::time::Duration;

use typhon_engine::torrent::meta::TorrentStatus;
use typhon_engine::torrent::TorrentManager;

use super::breaker::Breaker;
use super::cache::{Cache, Entry, Verify};
use super::overrides::override_host;
use super::policy::{self, Policy};
use super::scheduler::{self, Catalogue, Job, Outcome};

/// Which bucket an announce failure belongs in.
///
/// Matched on the REDACTED message, so no passkey can reach the counter. The
/// classes are the ones an operator acts on differently: back off, fix the
/// account, remove the torrent, or look at the network.
fn classify(err: &str) -> &'static str {
    // Only the IPv4 leg is classified when both families failed.
    //
    // `merge_announce` reports "v4: <e4> | v6: <e6>", and on an A-only tracker
    // the v6 leg ALWAYS fails with "Network unreachable" -- classifying the
    // concatenation lets that noise win over the real cause. Measured on the
    // bench: a tracker answering 429 on v4 was filed under `connect`.
    let primary = match err.find(" | v6: ") {
        Some(i) => &err[..i],
        None => err,
    };
    let e = primary.to_ascii_lowercase();
    if e.contains("429") || e.contains("too many requests") {
        "rate_limited"
    } else if e.contains("timed out") || e.contains("timeout") {
        "timeout"
    } else if e.contains("passkey") {
        "invalid_passkey"
    } else if e.contains("unregistered") || e.contains("not registered") || e.contains("introuvable") {
        "unknown_torrent"
    } else if e.contains("dns") {
        "dns"
    } else if e.contains("connect") || e.contains("refused") || e.contains("unreachable") {
        "connect"
    } else if e.contains("http ") {
        "http_error"
    } else {
        "other"
    }
}

/// One announce in this many is a self-check.
///
/// Cheap on purpose: the point is a trickle of evidence per tracker per hour,
/// not a measurement campaign. A check costs one `numwant` a tracker would have
/// answered anyway.
const VERIFY_EVERY: u64 = 64;
/// How many peers a self-check asks for. Small enough that a tracker returning
/// fewer than this proves the list was not truncated.
const VERIFY_NUMWANT: u32 = 50;
static VERIFY_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    // `left` is what we still NEED, not what this client happened to download.
    // A torrent seeded from data already on disk -- an inject, a cross-seed, one
    // of our own uploads -- never downloaded a byte through Hydra, so deriving
    // left from the traffic counter announced it as a 0%-complete leecher: the
    // tracker stopped counting it as a seed, and numwant jumped to 200. A
    // seeding torrent is complete by definition, the same rule row.rs applies
    // to progress.
    let left = if torrent.status.load(Ordering::Relaxed)
        == TorrentStatus::Seeding as u8
    {
        0
    } else {
        (torrent.meta.total_size as i64 - downloaded).max(0)
    };
    // "started" is only right the first time a tracker hears about a torrent.
    // Sending it on every announce makes a tracker reset its view of us, and
    // some read it as a client that restarts in a loop.
    let event = if job.first { "started" } else { "" };

    // Sampled self-check. Only on a torrent that is already seeding: a leecher
    // asks for peers anyway, so its answer says nothing about numwant.
    let verify_this = left == 0
        && VERIFY_TICK.fetch_add(1, Ordering::Relaxed) % VERIFY_EVERY == 0;
    let numwant_this = if verify_this { Some(VERIFY_NUMWANT) } else { None };

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
                numwant_this,
            ) else {
                continue;
            };
            match typhon_engine::tracker::http::send_announce(&req.url, &req.user_agent, req.ip_mode).await {
                Ok(resp) => {
                    breaker.record(&host, true, std::time::Instant::now());
                    cache.count_ok();
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
                    // Publish the answer onto the torrent itself.
                    //
                    // These atomics are what every reader in the process
                    // consults -- the detail panel, the list rows, the qBit
                    // shim -- and until 4.4.5 nothing ever wrote them. They
                    // were filled by the Go front, which owned the announce
                    // loop; 4.0.0 moved that loop here and recorded the answer
                    // only in `cache`, which no reader consults. The result was
                    // a node reporting 0 seeders, 0 leechers and "never
                    // announced" for all 300k torrents while announcing
                    // normally, with no error anywhere.
                    {
                        use std::sync::atomic::Ordering;
                        torrent.scrape_seeders.store(resp.complete as u32, Ordering::Relaxed);
                        torrent.scrape_leechers.store(resp.incomplete as u32, Ordering::Relaxed);
                        let now_unix = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        torrent.last_announce_at.store(now_unix, Ordering::Relaxed);
                        torrent
                            .next_announce_at
                            .store(now_unix + interval.as_secs() as i64, Ordering::Relaxed);
                        torrent.last_announce_ok.store(true, Ordering::Relaxed);
                        if let Ok(mut g) = torrent.last_announce_error.lock() {
                            g.clear();
                        }
                        if let Ok(mut g) = torrent.current_tracker.lock() {
                            *g = host.clone();
                        }
                    }
                    if verify_this {
                        // Our own listen port is the marker: the tracker hands
                        // back addresses, and only ours carries this port on
                        // this swarm. Family tells us which half survived.
                        let mut v4 = false;
                        let mut v6 = false;
                        for peer in &resp.peers {
                            if peer.port() == port {
                                match peer.ip() {
                                    std::net::IpAddr::V4(_) => v4 = true,
                                    std::net::IpAddr::V6(_) => v6 = true,
                                }
                            }
                        }
                        let swarm = resp.complete as i64 + resp.incomplete as i64;
                        cache.record_verify(
                            &host,
                            Verify {
                                at: std::time::Instant::now(),
                                v4,
                                v6,
                                // Fewer peers returned than asked for means the
                                // tracker gave us everything it had.
                                conclusive: (resp.peers.len() as u32) < VERIFY_NUMWANT,
                                swarm,
                            },
                        );
                    }
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
                    cache.count_failed_kind(&host, classify(&redact(&e)));
                    // At warn, not debug: a breaker that says a tracker
                    // "stopped answering" without saying why sends an operator
                    // to look at their network for a bug that is here. The
                    // host, never the URL -- a tracker URL carries the passkey
                    // in its path, and logs get pasted into issues.
                    // ⚠ The error is redacted, not printed. reqwest embeds the
                    // whole URL in its message, and a tracker URL carries the
                    // passkey in its path -- logging it verbatim puts an
                    // account credential in a file people paste into issues.
                    tracing::warn!(tracker = %host, error = %redact(&e), "announce failed");
                    // Same reason as the success path: the panel's "last error"
                    // column read an atomic nobody wrote, so every tracker
                    // showed "Success" while the log filled with refusals.
                    // Redacted here too -- the raw error embeds the announce
                    // URL, and that URL carries the passkey.
                    {
                        use std::sync::atomic::Ordering;
                        torrent.last_announce_ok.store(false, Ordering::Relaxed);
                        if let Ok(mut g) = torrent.last_announce_error.lock() {
                            *g = redact(&e).to_string();
                        }
                        if let Ok(mut g) = torrent.current_tracker.lock() {
                            *g = host.clone();
                        }
                    }
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

/// An error message with any URL taken out of it.
///
/// reqwest reports "error sending request for url (https://tracker/announce/
/// PASSKEY?...)" -- the reason is worth keeping, the URL is a credential.
fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find("http") {
        out.push_str(&rest[..start]);
        out.push_str("<url>");
        let tail = &rest[start..];
        // The URL runs to the closing parenthesis reqwest wraps it in, or to
        // the first space when it is not wrapped.
        let end = tail.find(')').or_else(|| tail.find(' ')).unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
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

    /// ⭐ A tracker URL carries the passkey in its path. reqwest puts the
    /// whole URL in its error message, so printing that message verbatim
    /// publishes an account credential into the logs.
    #[test]
    fn an_error_message_never_carries_the_url() {
        let raw = "http request: error sending request for url \
                   (https://tk.tr4ker.net/announce/SECRETKEY?info_hash=%AB): timed out";
        let clean = redact(raw);
        assert!(!clean.contains("SECRETKEY"), "the passkey survived: {clean}");
        assert!(!clean.contains("tk.tr4ker.net"));
        assert!(clean.contains("timed out"), "the reason is what we keep: {clean}");
    }

    #[test]
    fn a_message_without_a_url_is_left_alone() {
        assert_eq!(redact("tracker: torrent introuvable"), "tracker: torrent introuvable");
    }

    #[test]
    fn a_hash_survives_the_round_trip() {
        let raw = [0xabu8; 20];
        assert_eq!(parse_hex(&hex(&raw)), Some(raw));
        assert_eq!(parse_hex("short"), None);
        assert_eq!(parse_hex(&"zz".repeat(20)), None);
    }
}
