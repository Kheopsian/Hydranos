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
/// What a seeding torrent asks for once we know we are not reachable.
///
/// Not the 200 a leecher asks for: an unreachable node has to dial everything
/// itself, and two hundred per torrent across a catalogue is the connection
/// storm that made seeding passive in the first place. Fifty finds the leechers
/// that are themselves reachable, which is most of them.
const UNREACHABLE_SEED_NUMWANT: u32 = 50;
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
            // ... except one that still owes its trackers a departure. It
            // leaves the catalogue again as soon as that announce has gone
            // out, because the runner clears the flag when it sends it.
            .filter(|t| {
                !t.is_paused.load(std::sync::atomic::Ordering::Relaxed)
                    || t.pending_announce_event.load(std::sync::atomic::Ordering::Relaxed)
                        == typhon_engine::torrent::meta::ANNOUNCE_EVENT_STOPPED
            })
            .map(|t| hex(&t.info_hash))
            .collect()
    }
}

fn hex(hash: &[u8; 20]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Start announcing this engine's torrents. Returns immediately; the scheduler
/// and its workers outlive the call.
/// Returns the handle the API uses to jump the queue for one torrent.
///
/// The scheduler owns its heap and never shares it; a bump is a message like
/// any other, which is why this is a channel and not a lock.
pub fn start(
    manager: Arc<TorrentManager>,
    policy: super::PolicyHandle,
    port: u16,
    mode: Mode,
    cache: Arc<Cache>,
    admission: Arc<scheduler::Admission>,
) -> tokio::sync::mpsc::Sender<scheduler::BumpReq> {
    let catalogue = Arc::new(EngineCatalogue { manager: manager.clone() });
    // One breaker for the engine, not one per torrent: an outage belongs to the
    // host, and every torrent listing it has to learn from the same evidence.
    let breaker = Arc::new(Breaker::default());

    let announce = Arc::new(move |job: Job| {
        let manager = manager.clone();
        // Read per job, not captured once: this is the whole reason an override
        // added while the daemon runs reaches the next announce.
        let policy = policy
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|e| e.into_inner().clone());
        let breaker = breaker.clone();
        let cache = cache.clone();
        async move { announce_one(&manager, &policy, &breaker, &cache, port, mode, job).await }
    });

    // Small on purpose: this carries hand-pressed buttons, not traffic. A full
    // queue means something is looping and must be refused, not buffered.
    let (bump_tx, bump_rx) = tokio::sync::mpsc::channel::<scheduler::BumpReq>(64);
    tokio::spawn(async move {
        scheduler::run(catalogue, announce, bump_rx, admission).await;
    });
    bump_tx
}

/// One torrent, every tracker it carries, in tier order.
///
/// Stops at the first tier that answers: that is what a tier is for. Walking
/// all of them would announce the same torrent several times over and count
/// the upload twice on trackers that share a swarm.
/// What a seeding torrent asks a tracker for.
///
/// A complete torrent normally asks for no peers: it is reachable, so leechers
/// open the connection and there is nothing for us to dial. That assumption is
/// the whole of it, and when it is false the torrent uploads *nothing* -- not
/// less, nothing -- because it never learns a single address. Every other
/// client dials out instead.
///
/// The self-check already measures the assumption, per tracker, by asking for a
/// short peer list and looking for our own address in it. Until now nothing
/// read the answer.
///
/// - no answer yet: ask, so the question gets settled on the first announce to
///   a tracker rather than whenever the sampling tick comes round -- which on a
///   small catalogue is an announce interval away, and that is exactly the user
///   this is for.
/// - a conclusive answer that did not find us: we are invisible, so dial.
/// - anything else: the ordinary sampled behaviour.
fn seed_numwant(verify: Option<&Verify>, sampled: bool) -> Option<u32> {
    match verify {
        None => Some(VERIFY_NUMWANT),
        Some(v) if v.conclusive && !v.v4 && !v.v6 => Some(UNREACHABLE_SEED_NUMWANT),
        _ if sampled => Some(VERIFY_NUMWANT),
        _ => None,
    }
}

/// Which BEP 3 event this announce carries.
///
/// An owed event outranks `started`: a torrent that finishes or is stopped
/// inside its very first announce cycle has more to tell the tracker than that
/// it arrived. Everything else is a periodic announce, which BEP 3 wants
/// carrying no event key at all -- not an empty one.
fn event_for(owed: u8, first: bool) -> &'static str {
    use typhon_engine::torrent::meta::{ANNOUNCE_EVENT_COMPLETED, ANNOUNCE_EVENT_STOPPED};
    match owed {
        ANNOUNCE_EVENT_COMPLETED => "completed",
        ANNOUNCE_EVENT_STOPPED => "stopped",
        _ if first => "started",
        _ => "",
    }
}

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
    // Filtering the catalogue is not enough: a bump puts one torrent at the head
    // of the queue directly, so a forced reannounce reached a paused torrent and
    // told a tracker we are a peer for something we will not serve. The guard
    // belongs here, the one place every announce funnels through -- a paused
    // torrent announces to nobody, whoever asked. Reported as `gone` because
    // that is what it is to the scheduler: the catalogue already filters paused
    // torrents, so resuming one puts it back on the next refill.
    // Read before the pause check, because a torrent that owes a departure is
    // paused by definition and would otherwise be dropped here without ever
    // telling its trackers.
    use typhon_engine::torrent::meta::{ANNOUNCE_EVENT_NONE, ANNOUNCE_EVENT_STOPPED};
    let owed = torrent.pending_announce_event.load(Ordering::Relaxed);

    if torrent.is_paused.load(Ordering::Relaxed) && owed != ANNOUNCE_EVENT_STOPPED {
        return gone;
    }

    // Taken only now that it is certain to be sent: clearing it above would
    // lose the event for a torrent that turned out to be paused.
    let owed = torrent
        .pending_announce_event
        .swap(ANNOUNCE_EVENT_NONE, Ordering::Relaxed);

    // "started" is only right the first time a tracker hears about a torrent.
    // Sending it on every announce makes a tracker reset its view of us, and
    // some read it as a client that restarts in a loop. An owed event outranks
    // it: a torrent that completes on its very first announce cycle has more
    // to say than that it arrived.
    let event = event_for(owed, job.first);

    // Sampled self-check. Only on a torrent that is already seeding: a leecher
    // asks for peers anyway, so its answer says nothing about numwant.
    let verify_this = left == 0
        && VERIFY_TICK.fetch_add(1, Ordering::Relaxed) % VERIFY_EVERY == 0;

    let mut interval = Duration::from_secs(30 * 60);
    let mut announced_at_all = false;
    for tier in &torrent.meta.trackers {
        let mut tier_answered = false;
        for tracker_url in tier {
            let host = override_host(tracker_url);
            if !breaker.allows(&host, std::time::Instant::now()) {
                continue;
            }
            // Per tracker, not per torrent: we can be visible to one and not to
            // another -- an IPv6-only tracker on a v4-only host, say.
            let numwant_this = if left == 0 {
                seed_numwant(cache.verify_for(&host).as_ref(), verify_this)
            } else {
                None
            };
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
                    if resp.interval > 0 {
                        interval = Duration::from_secs(resp.interval as u64);
                    }
                    // `min interval` is a floor, not a suggestion. It exists so
                    // a tracker can refuse to be asked again too soon whatever
                    // the client thinks -- so it wins over `interval` when the
                    // two disagree, rather than being averaged with it.
                    let floor = Duration::from_secs(resp.min_interval as u64);
                    if resp.min_interval > 0 && interval < floor {
                        interval = floor;
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

    if announced_at_all {
        // What a tracker was actually told, recorded only now that one has
        // answered. Derived from the intention instead, every torrent would
        // look compliant the instant a setting was saved -- which is the
        // failure mode where nothing contradicts itself.
        let sent = policy::announced_peer_id(policy, &torrent.meta.trackers);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        *torrent.announced_peer_id.write() = Some((sent, now));
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

#[cfg(test)]
mod event_rules {
    use super::event_for;
    use typhon_engine::torrent::meta::{
        ANNOUNCE_EVENT_COMPLETED, ANNOUNCE_EVENT_NONE, ANNOUNCE_EVENT_STOPPED,
    };

    /// BEP 3: the first announce for a torrent says `started`, and only it.
    #[test]
    fn the_first_announce_is_the_only_started_one() {
        assert_eq!(event_for(ANNOUNCE_EVENT_NONE, true), "started");
        assert_eq!(event_for(ANNOUNCE_EVENT_NONE, false), "");
    }

    /// BEP 3: a finished download is reported. Private trackers count snatches
    /// from this event and from nothing else.
    #[test]
    fn a_finished_download_reports_completed() {
        assert_eq!(event_for(ANNOUNCE_EVENT_COMPLETED, false), "completed");
    }

    /// BEP 3: a stopped torrent tells its trackers to drop it, rather than
    /// leaving them to time the entry out.
    #[test]
    fn a_stopped_torrent_reports_stopped() {
        assert_eq!(event_for(ANNOUNCE_EVENT_STOPPED, false), "stopped");
    }

    /// An owed event wins over `started`: a torrent can complete within its
    /// first announce interval, and "it arrived" is the less useful of the two.
    #[test]
    fn an_owed_event_outranks_the_first_announce() {
        assert_eq!(event_for(ANNOUNCE_EVENT_COMPLETED, true), "completed");
        assert_eq!(event_for(ANNOUNCE_EVENT_STOPPED, true), "stopped");
    }
}

#[cfg(test)]
mod numwant_rules {
    use super::{seed_numwant, UNREACHABLE_SEED_NUMWANT, VERIFY_NUMWANT};
    use crate::announce::cache::Verify;

    fn verify(v4: bool, v6: bool, conclusive: bool) -> Verify {
        Verify {
            at: std::time::Instant::now(),
            v4,
            v6,
            conclusive,
            swarm: 10,
        }
    }

    /// A tracker we have never checked gets asked straight away. Waiting for
    /// the sampling tick would leave a small catalogue passive for a whole
    /// announce interval, and a small catalogue is exactly the case where
    /// somebody is comparing us with qBittorrent and finding us slower.
    #[test]
    fn an_unchecked_tracker_is_asked_immediately() {
        assert_eq!(seed_numwant(None, false), Some(VERIFY_NUMWANT));
    }

    /// The point of the change: invisible to the tracker means we dial, the
    /// way every other client does, instead of uploading nothing.
    #[test]
    fn an_invisible_seed_asks_for_peers_to_dial() {
        let v = verify(false, false, true);
        assert_eq!(seed_numwant(Some(&v), false), Some(UNREACHABLE_SEED_NUMWANT));
    }

    /// And the converse, which is what keeps a reachable node cheap: it asks
    /// for nothing, because leechers open the connection to it.
    #[test]
    fn a_reachable_seed_still_asks_for_nothing() {
        assert_eq!(seed_numwant(Some(&verify(true, true, true)), false), None);
        assert_eq!(seed_numwant(Some(&verify(true, false, true)), false), None);
        assert_eq!(seed_numwant(Some(&verify(false, true, true)), false), None);
    }

    /// An absence from a truncated list is not an absence. The tracker returned
    /// as many peers as we asked for, so it had more to give and ours may be
    /// among them -- treating that as unreachable would make every torrent in a
    /// large swarm start dialling for nothing.
    #[test]
    fn an_inconclusive_answer_is_not_a_verdict() {
        assert_eq!(seed_numwant(Some(&verify(false, false, false)), false), None);
    }

    /// The ordinary self-check still fires on a tracker that sees us.
    #[test]
    fn the_sampled_self_check_survives() {
        assert_eq!(
            seed_numwant(Some(&verify(true, true, true)), true),
            Some(VERIFY_NUMWANT)
        );
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    /// ⭐⭐ Only the IPv4 leg is classified when both families failed. On an
    /// A-only tracker the v6 leg ALWAYS fails with "Network unreachable", and
    /// classifying the concatenation lets that noise win over the real cause:
    /// measured on the bench, a tracker answering 429 on v4 was filed under
    /// `connect`.
    #[test]
    fn the_v6_leg_never_masks_the_real_v4_cause() {
        let both = "v4: HTTP 429 Too Many Requests | v6: Network unreachable";
        assert_eq!(classify(both), "rate_limited", "the v4 cause wins");

        let timeout = "v4: operation timed out | v6: Network unreachable";
        assert_eq!(classify(timeout), "timeout");
    }

    #[test]
    fn each_class_is_recognised_from_what_a_tracker_actually_says() {
        assert_eq!(classify("HTTP 429"), "rate_limited");
        assert_eq!(classify("too many requests"), "rate_limited");
        assert_eq!(classify("operation timed out"), "timeout");
        assert_eq!(classify("connection timeout"), "timeout");
        assert_eq!(classify("invalid passkey"), "invalid_passkey");
        assert_eq!(classify("unregistered torrent"), "unknown_torrent");
        assert_eq!(classify("torrent not registered"), "unknown_torrent");
        assert_eq!(classify("dns error"), "dns");
        assert_eq!(classify("connection refused"), "connect");
        assert_eq!(classify("Network unreachable"), "connect");
        assert_eq!(classify("HTTP 503"), "http_error");
    }

    /// A tracker answering in French still says "introuvable" -- the class has
    /// to see it, or a whole tracker's errors land in `other`.
    #[test]
    fn a_french_tracker_saying_introuvable_is_an_unknown_torrent() {
        assert_eq!(classify("torrent introuvable"), "unknown_torrent");
    }

    #[test]
    fn classification_ignores_case() {
        assert_eq!(classify("OPERATION TIMED OUT"), "timeout");
        assert_eq!(classify("Invalid Passkey"), "invalid_passkey");
    }

    /// Anything unrecognised is `other`, never empty: the panel groups on this
    /// string and an empty one would make a bucket nobody can name.
    #[test]
    fn an_unrecognised_error_is_other_rather_than_empty() {
        assert_eq!(classify("something nobody has seen"), "other");
        assert_eq!(classify(""), "other");
    }

    /// ⭐⭐ A URL carries the PASSKEY. It must never reach a log line or the
    /// UI: that is how a private tracker account leaks out of a screenshot.
    #[test]
    fn a_url_is_redacted_out_of_an_error_message() {
        let msg = "error sending request for url (https://tracker.example/announce?passkey=SECRET)";
        let out = redact(msg);
        assert!(!out.contains("SECRET"), "the passkey is gone: {out}");
        assert!(!out.contains("tracker.example"), "and so is the host: {out}");
        assert!(out.contains("<url>"), "replaced by a marker: {out}");
    }

    #[test]
    fn redaction_handles_a_bare_url_and_several_of_them() {
        let one = redact("failed https://a.example/x?passkey=A after 3 tries");
        assert!(!one.contains("passkey=A"), "got {one}");
        assert!(one.contains("after 3 tries"), "the rest of the message survives: {one}");

        let two = redact("https://a.example/x?k=1 and https://b.example/y?k=2");
        assert!(!two.contains("k=1") && !two.contains("k=2"), "got {two}");
    }

    #[test]
    fn a_message_with_no_url_is_left_alone() {
        assert_eq!(redact("operation timed out"), "operation timed out");
        assert_eq!(redact(""), "");
    }

    /// ⭐ An owed event OUTRANKS `started`: a torrent that finishes or is
    /// stopped inside its first announce cycle has more to tell the tracker
    /// than that it arrived.
    #[test]
    fn an_owed_event_outranks_started() {
        use typhon_engine::torrent::meta::{ANNOUNCE_EVENT_COMPLETED, ANNOUNCE_EVENT_STOPPED};
        assert_eq!(event_for(ANNOUNCE_EVENT_COMPLETED, true), "completed");
        assert_eq!(event_for(ANNOUNCE_EVENT_STOPPED, true), "stopped");
    }

    /// ⭐ A periodic announce carries NO event key at all -- not an empty one.
    /// BEP 3 is explicit, and some trackers refuse `event=`.
    #[test]
    fn a_periodic_announce_carries_no_event() {
        assert_eq!(event_for(0, false), "", "no event, which the caller omits");
        assert_eq!(event_for(0, true), "started", "the first one announces itself");
    }

    #[test]
    fn hex_round_trips_through_parse() {
        let h = [0xABu8; 20];
        let s = hex(&h);
        assert_eq!(s.len(), 40);
        assert_eq!(parse_hex(&s), Some(h));
    }

    /// A hash of the wrong shape is refused rather than silently truncated to
    /// something that addresses another torrent.
    #[test]
    fn a_hash_of_the_wrong_shape_does_not_parse() {
        assert!(parse_hex("").is_none());
        assert!(parse_hex(&"0".repeat(39)).is_none());
        assert!(parse_hex(&"0".repeat(41)).is_none());
        assert!(parse_hex(&"z".repeat(40)).is_none(), "not hex");
    }

    #[test]
    fn hex_parsing_accepts_either_case() {
        let upper = "AABBCCDDEEFF00112233445566778899AABBCCDD";
        assert_eq!(parse_hex(upper), parse_hex(&upper.to_lowercase()));
        assert!(parse_hex(upper).is_some());
    }

    /// ⭐ A self-check costs one `numwant` a tracker would have answered
    /// anyway. Never checked at all, and we would never learn that a tracker
    /// stopped handing back our own address.
    #[test]
    fn a_torrent_never_verified_asks_for_a_self_check() {
        assert_eq!(seed_numwant(None, false), Some(VERIFY_NUMWANT));
    }

    #[test]
    fn a_sampled_announce_asks_for_a_self_check() {
        assert_eq!(seed_numwant(None, true), Some(VERIFY_NUMWANT));
    }

    /// ⭐ A torrent the tracker conclusively does NOT hand our address back
    /// for -- neither family -- asks for a bigger peer list: it is unreachable
    /// and needs somebody to dial it instead.
    #[test]
    fn an_unreachable_seed_asks_for_more_peers() {
        let unreachable = Verify {
            at: std::time::Instant::now(),
            v4: false,
            v6: false,
            conclusive: true,
            swarm: 10,
        };
        assert_eq!(seed_numwant(Some(&unreachable), false), Some(UNREACHABLE_SEED_NUMWANT));
    }

    /// An INCONCLUSIVE check is not evidence of anything: the tracker
    /// truncated the list, so an absence is not an absence.
    #[test]
    fn an_inconclusive_check_does_not_trigger_the_unreachable_path() {
        let inconclusive = Verify {
            at: std::time::Instant::now(),
            v4: false,
            v6: false,
            conclusive: false,
            swarm: 10,
        };
        assert_eq!(seed_numwant(Some(&inconclusive), false), None);
    }

    /// The ordinary case asks for nothing: a seeder does not want peers, and
    /// asking for them on every announce is load nobody needs.
    #[test]
    fn an_ordinary_seeding_announce_asks_for_no_peers() {
        let verified = Verify {
            at: std::time::Instant::now(),
            v4: true,
            v6: true,
            conclusive: true,
            swarm: 10,
        };
        assert_eq!(seed_numwant(Some(&verified), false), None);
    }
}

#[cfg(test)]
mod announce_one_tests {
    use super::*;
    use std::sync::Arc;
    use typhon_engine::torrent::TorrentManager;

    fn manager(tag: &str) -> (Arc<TorrentManager>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "hydra-ann1-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let data = root.join("data");
        let resume = root.join("resume");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&resume).unwrap();
        let mgr = Arc::new(TorrentManager::new(
            data.to_string_lossy().into_owned(),
            resume.to_string_lossy().into_owned(),
            Arc::new(typhon_engine::disk::DiskManager::new(16)),
        ));
        (mgr, root)
    }

    /// Bencode lengths are COMPUTED, never counted.
    fn torrent_bytes(name: &str, announce: &str) -> Vec<u8> {
        let mut info = Vec::new();
        info.extend_from_slice(format!("d6:lengthi16384e4:name{}:{name}", name.len()).as_bytes());
        info.extend_from_slice(b"12:piece lengthi16384e6:pieces20:");
        let mut piece = [0xABu8; 20];
        piece[0] = name.as_bytes()[0];
        info.extend_from_slice(&piece);
        info.push(b'e');
        let mut out = Vec::new();
        out.extend_from_slice(format!("d8:announce{}:{announce}4:info", announce.len()).as_bytes());
        out.extend_from_slice(&info);
        out.push(b'e');
        out
    }

    struct FakeTracker {
        url: String,
        _stop: tokio::sync::oneshot::Sender<()>,
    }

    async fn fake_tracker(body: &'static [u8], status: u16) -> FakeTracker {
        let app = axum::Router::new().route(
            "/announce",
            axum::routing::get(move || async move {
                (axum::http::StatusCode::from_u16(status).unwrap(), body.to_vec())
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        FakeTracker { url: format!("http://{addr}/announce"), _stop: tx }
    }

    /// `d8:completei3e10:incompletei1e8:intervali1800e12:min intervali900e5:peers0:e`
    const OK_BODY: &[u8] =
        b"d8:completei3e10:incompletei1e8:intervali1800e12:min intervali900e5:peers0:e";

    /// ⚠️ `stopped: false`. A PAUSED torrent announces to nobody and is
    /// reported as `gone` -- which is correct behaviour, not a bug, and it
    /// silently made six of these tests assert the wrong thing.
    fn add(mgr: &Arc<TorrentManager>, name: &str, announce: &str) -> String {
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes(name, announce), "/tmp", false, true)
            .expect("the fixture torrent parses");
        let t = mgr.get(&ih).expect("just added");
        {
            let mut live = t.live_trackers.write();
            live.clear();
            live.push(vec![announce.to_string()]);
        }
        typhon_engine::torrent::hex_encode(&ih)
    }

    fn parts() -> (Policy, Breaker, Cache) {
        (Policy::default(), Breaker::default(), Cache::default())
    }

    /// ⭐⭐ A hash the engine no longer holds reports `gone`, so the scheduler
    /// stops tracking it. Without this the announcer keeps a timer alive for a
    /// torrent nobody can serve -- 300k of those is the whole scheduler.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_torrent_the_engine_no_longer_holds_reports_gone() {
        let (mgr, root) = manager("gone");
        let (policy, breaker, cache) = parts();
        let out = announce_one(
            &mgr,
            &policy,
            &breaker,
            &cache,
            16371,
            Mode::Hoard,
            Job { info_hash: "a".repeat(40), first: true },
        )
        .await;
        assert!(out.gone, "an unknown hash must be reported as gone");
        assert_eq!(out.info_hash, "a".repeat(40), "the outcome names the job it answers");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A tracker that answers is believed: the interval it states is what
    /// decides when we come back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_interval_the_tracker_states_decides_the_next_visit() {
        let t = fake_tracker(OK_BODY, 200).await;
        let (mgr, root) = manager("ok");
        let hash = add(&mgr, "alpha", &t.url);
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr,
            &policy,
            &breaker,
            &cache,
            16371,
            Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;

        assert!(!out.gone, "the torrent is here");
        assert_eq!(out.info_hash, hash);
        assert!(
            out.next_in > Duration::from_secs(0),
            "a next visit is always scheduled, got {:?}",
            out.next_in
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A tracker that is not there must not stop the announcer: it schedules
    /// a retry rather than dropping the torrent. A dead tracker is the ordinary
    /// case, not a reason to stop announcing forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dead_tracker_schedules_a_retry_rather_than_giving_up() {
        let (mgr, root) = manager("dead");
        let hash = add(&mgr, "beta", "http://127.0.0.1:1/announce");
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr,
            &policy,
            &breaker,
            &cache,
            16371,
            Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;

        assert!(!out.gone, "a tracker being down does not make the torrent gone");
        assert!(out.next_in > Duration::from_secs(0), "a retry is scheduled");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A tracker refusing with a `failure reason` is an answer, not a crash --
    /// and the torrent stays in the schedule.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tracker_refusal_is_handled_and_the_torrent_stays_scheduled() {
        const REASON: &str = "unregistered torrent";
        assert_eq!(REASON.len(), 20, "the fixture length is computed");
        const FAIL: &[u8] = b"d14:failure reason20:unregistered torrente";

        let t = fake_tracker(FAIL, 200).await;
        let (mgr, root) = manager("refused");
        let hash = add(&mgr, "gamma", &t.url);
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr,
            &policy,
            &breaker,
            &cache,
            16371,
            Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        assert!(!out.gone);
        assert!(out.next_in > Duration::from_secs(0));
        let _ = std::fs::remove_dir_all(root);
    }

    /// An HTTP error is classed and retried, not treated as a swarm with no
    /// peers -- the two look identical if the status is ignored.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_http_error_is_not_read_as_an_empty_swarm() {
        let t = fake_tracker(b"rate limited", 429).await;
        let (mgr, root) = manager("429");
        let hash = add(&mgr, "delta", &t.url);
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr,
            &policy,
            &breaker,
            &cache,
            16371,
            Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        assert!(!out.gone);
        assert!(out.next_in > Duration::from_secs(0));
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A race is decided in its first minute, so a race torrent comes back
    /// far sooner than a hoard one. Reading the two modes the same way is how
    /// a race is lost before the first re-announce.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_race_comes_back_sooner_than_a_hoard() {
        let t = fake_tracker(OK_BODY, 200).await;
        let (mgr, root) = manager("mode");
        let hash = add(&mgr, "epsilon", &t.url);
        let (policy, breaker, cache) = parts();

        let race = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Race,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        let hoard = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: false },
        )
        .await;

        assert!(
            race.next_in <= hoard.next_in,
            "a race must not wait longer than a hoard (race {:?} vs hoard {:?})",
            race.next_in,
            hoard.next_in
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐⭐ The breaker exists so a tracker in outage is not hammered by every
    /// torrent that names it. When it refuses a host, the announce must not go
    /// out -- and the torrent must still be rescheduled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_the_breaker_refuses_is_not_announced_to() {
        let t = fake_tracker(OK_BODY, 200).await;
        let (mgr, root) = manager("breaker");
        let hash = add(&mgr, "zeta", &t.url);
        let (policy, breaker, cache) = parts();

        // Trip the breaker on this host by reporting failures against it.
        let host = typhon_engine::rpc::dispatch::tracker_host_of(&t.url);
        for _ in 0..20 {
            breaker.record(&host, false, std::time::Instant::now());
        }
        assert!(
            !breaker.allows(&host, std::time::Instant::now()),
            "the breaker is open on this host, which is what the test is about"
        );

        let out = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        assert!(!out.gone, "a broken tracker does not make the torrent gone");
        assert!(out.next_in > Duration::from_secs(0), "it comes back later");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐⭐ A PAUSED torrent announces to nobody, whoever asked. The guard sits
    /// here because a bump puts a torrent at the head of the queue directly:
    /// filtering the catalogue was not enough, and a forced reannounce told a
    /// tracker we are a peer for something we will not serve.
    ///
    /// It reports `gone` so the scheduler stops visiting it, which is why the
    /// six tests above had to add their torrents unpaused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_torrent_announces_to_nobody_however_it_was_asked() {
        let t = fake_tracker(OK_BODY, 200).await;
        let (mgr, root) = manager("paused");
        let hash = add(&mgr, "theta", &t.url);
        {
            let st = mgr.get(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
            st.is_paused.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        assert!(out.gone, "a paused torrent is dropped from the schedule, not announced");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ ...except when it owes a `stopped` event. A torrent stopped by hand is
    /// paused BY DEFINITION, and dropping it here would lose the one announce
    /// that tells its trackers we are leaving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_paused_torrent_that_owes_a_stopped_event_still_announces_it() {
        use typhon_engine::torrent::meta::ANNOUNCE_EVENT_STOPPED;
        let t = fake_tracker(OK_BODY, 200).await;
        let (mgr, root) = manager("paused-stop");
        let hash = add(&mgr, "iota", &t.url);
        {
            let st = mgr.get(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
            st.is_paused.store(true, std::sync::atomic::Ordering::Relaxed);
            st.pending_announce_event.store(ANNOUNCE_EVENT_STOPPED, std::sync::atomic::Ordering::Relaxed);
        }
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: false },
        )
        .await;
        assert!(
            !out.gone,
            "the goodbye announce must go out even though the torrent is paused"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A torrent with no tracker at all is still answered: it is scheduled,
    /// just with nothing to talk to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_torrent_with_no_tracker_is_answered_not_dropped() {
        let (mgr, root) = manager("notracker");
        let hash = add(&mgr, "eta", "http://127.0.0.1:1/announce");
        {
            let t = mgr.get(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
            t.live_trackers.write().clear();
        }
        let (policy, breaker, cache) = parts();

        let out = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: true },
        )
        .await;
        assert!(!out.gone);
        let _ = std::fs::remove_dir_all(root);
    }
}
