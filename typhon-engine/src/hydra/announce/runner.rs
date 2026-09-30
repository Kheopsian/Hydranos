//! Wiring the announcer to an engine.
//!
//! The scheduler decides *when*, the policy decides *what*, the engine's
//! tracker module puts it on the wire. This is the part that knows all three.

use std::sync::Arc;
use std::time::Duration;

use typhon_engine::torrent::meta::{TorrentStatus, ANNOUNCE_EVENT_NONE, ANNOUNCE_EVENT_STOPPED};
use typhon_engine::torrent::TorrentManager;

use super::book;
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
/// Whether a failed announce still shows the tracker is up, for the breaker.
/// A 429 is the tracker answering, and asking us to slow down.
fn proves_alive(kind: &str) -> bool {
    kind == "rate_limited"
}

const VERIFY_EVERY: u64 = 64;
/// How soon to look again at a torrent whose data is being checked.
const CHECKING_RETRY: Duration = Duration::from_secs(60);
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
                        & typhon_engine::torrent::meta::ANNOUNCE_EVENT_STOPPED
                        != 0
            })
            .map(|t| hex(&t.info_hash))
            .collect()
    }

    fn host_of(&self, info_hash: &str) -> String {
        let Some(ih) = parse_hex(info_hash) else { return String::new() };
        let Some(t) = self.manager.get(&ih) else { return String::new() };
        let tiers = t.live_trackers.read();
        tiers.iter().flatten().next().map(|u| super::cache::host_of(u)).unwrap_or_default()
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
    let hook_manager = manager.clone();
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
    // An owed event -- `completed`, `stopped`, a resumed torrent's `started`
    // -- goes out now, not at the next scheduled announce, which for a seed
    // is half an hour away. Through the bump queue, and `try_send`: a bulk
    // stop of fifty thousand torrents must not become fifty thousand queued
    // announces; what does not fit goes out with the catalogue refill.
    {
        let tx = bump_tx.clone();
        hook_manager.set_announce_hook(Arc::new(move |ih: [u8; 20]| {
            let _ = tx.try_send(scheduler::BumpReq { info_hash: hex(&ih), reply: None, forced: false });
        }));
    }
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

/// When to come back, given what this announce learned.
///
/// Pulled out of `announce_one` so it can be tested: reaching it through the
/// real function means a tracker, a socket and a torrent on disk, which is why
/// the unbounded fast phase below survived as long as it did.
fn next_announce_in(
    mode: Mode,
    interval: Duration,
    left: i64,
    first: bool,
    fast_window_open: bool,
    uploading: bool,
) -> Duration {
    match mode {
        Mode::Hoard => interval,
        // A complete race torrent is a seed like any other and falls back to
        // what the tracker asked for.
        Mode::Race if left == 0 => interval,
        Mode::Race if first && fast_window_open => RACE_FAST,
        Mode::Race => {
            if uploading {
                RACE_SUSTAINED
            } else if fast_window_open {
                RACE_FAST
            } else {
                // Past the first minute: keep feeding the swarm at the sustained
                // rate rather than falling silent. Stopping at the first peer was
                // tried and reverted -- see RACE_SUSTAINED -- because a private
                // tracker race rarely has more than five peers at all.
                RACE_SUSTAINED
            }
        }
    }
}

pub(super) async fn announce_one(
    manager: &Arc<TorrentManager>,
    policy: &Policy,
    breaker: &Breaker,
    cache: &Cache,
    port: u16,
    mode: Mode,
    job: Job,
) -> Outcome {
    let gone = Outcome {
        info_hash: job.info_hash.clone(),
        next_in: Duration::ZERO,
        gone: true,
        throttled: false,
        timed_out: false,
        registration_retry: false,
    };

    let Some(hash) = parse_hex(&job.info_hash) else {
        return gone;
    };
    let Some(torrent) = manager.get(&hash) else {
        cache.forget(&job.info_hash);
        return gone;
    };

    use std::sync::atomic::Ordering;
    let status = torrent.status.load(Ordering::Relaxed);
    // Nothing is said while the data is being checked: `left` is not known
    // until the check ends, and every client stays quiet until then. Back in a
    // minute; an event raised meanwhile is still owed and still waiting.
    if status == TorrentStatus::Checking as u8 {
        return Outcome { next_in: CHECKING_RETRY, gone: false, ..gone };
    }
    // A torrent whose data is gone cannot serve a single piece. Announcing it
    // hands its address to leechers it will refuse; it leaves the swarm the
    // way a stopped one does, and comes back with `started` once a recheck
    // finds the data.
    let silent = torrent.is_paused.load(Ordering::Relaxed) || status == TorrentStatus::Error as u8;

    // BEP 3's counters are the SESSION's: bytes moved since `started`. The
    // lifetime totals are ours and stay ours -- see `begin_announce_session`.
    let uploaded = torrent.session_uploaded() as i64;
    let downloaded = torrent.session_downloaded() as i64;
    // `left` is what we still NEED, from the pieces we hold -- not what this
    // client happened to download. Data already on disk (an inject, a
    // cross-seed, a resumed download) was never downloaded by us and is not
    // needed either; deriving left from the traffic counter announced such a
    // torrent as a 0 %-complete leecher.
    let left = torrent.bytes_left() as i64;

    // The engine's raised events, filed onto the trackers they are owed to.
    // Taken in the same breath: from here on the book is the only record.
    let mut raised = torrent.pending_announce_event.swap(ANNOUNCE_EVENT_NONE, Ordering::Relaxed);
    if status == TorrentStatus::Error as u8 {
        raised |= ANNOUNCE_EVENT_STOPPED;
    }
    let owes_something = {
        let mut book = torrent.announce_book.lock().unwrap_or_else(|e| e.into_inner());
        book::file_owed(&mut book, raised, silent);
        book.iter().any(|s| s.stopped_owed || (s.completed_owed && s.started))
    };
    // A paused torrent announces to nobody -- including when a forced
    // re-announce targets it directly -- except to take back a `started` it
    // gave. Reported as `gone`: the catalogue refill puts it back on resume.
    if silent && !owes_something {
        return if status == TorrentStatus::Error as u8 {
            Outcome { next_in: Duration::from_secs(30 * 60), gone: false, ..gone }
        } else {
            gone
        };
    }

    // Sampled self-check. Only on a torrent that is already seeding: a leecher
    // asks for peers anyway, so its answer says nothing about numwant.
    let verify_this = left == 0
        && VERIFY_TICK.fetch_add(1, Ordering::Relaxed) % VERIFY_EVERY == 0;

    // The list actually announced to: the operator can edit it at runtime,
    // and `meta.trackers` is only what the .torrent said when it was added.
    let tiers: Vec<Vec<String>> = torrent.live_trackers.read().clone();

    let mut interval = Duration::from_secs(30 * 60);
    let mut announced_at_all = false;
    // Any tracker answered 429: reported to the scheduler's concurrency control.
    let mut throttled = false;
    let mut timed_out = false;
    // Hoard: the first tier that has us ends the pass for periodic announces.
    // Owed events still reach every tracker they are owed to.
    let mut tier_done = false;
    for tier in &tiers {
        for tracker_url in tier {
            let host = override_host(tracker_url);
            // At most two sends to one tracker in one pass, and the second
            // only ever an event: `completed` then `stopped`, for a download
            // finished and stopped in one breath. Never an event followed by
            // a periodic announce -- that is two requests where one is due.
            for pass in 0..2 {
                let now = typhon_engine::torrent::meta::now_secs();
                let (step, tracker_id) = {
                    let mut book = torrent.announce_book.lock().unwrap_or_else(|e| e.into_inner());
                    let slot = book::slot_mut(&mut book, tracker_url);
                    let st = book::step(slot, now, tier_done && mode == Mode::Hoard, silent, job.forced);
                    if let book::Step::Skip(why) = st {
                        if mode == Mode::Hoard && book::holds_us(slot, why) {
                            tier_done = true;
                        }
                    }
                    (st, slot.tracker_id.as_deref().map(String::from))
                };
                let event = match step {
                    book::Step::Send(ev) if pass == 0 || ev == "completed" || ev == "stopped" => ev,
                    _ => break,
                };
                if !breaker.allows(&host, std::time::Instant::now()) {
                    break;
                }
                // Per tracker, not per torrent: we can be visible to one and
                // not to another -- an IPv6-only tracker on a v4-only host, say.
                let numwant_this = if left == 0 && event != "stopped" {
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
                    tracker_id.as_deref(),
                ) else {
                    break;
                };
                let result = typhon_engine::tracker::http::send_announce(&req.url, &req.user_agent, req.ip_mode).await;
                {
                    let mut book = torrent.announce_book.lock().unwrap_or_else(|e| e.into_inner());
                    book::record(book::slot_mut(&mut book, tracker_url), event, &result, typhon_engine::torrent::meta::now_secs());
                }
                match result {
                    Ok(resp) => {
                        breaker.record(&host, true, std::time::Instant::now());
                        cache.count_ok();
                        if let Some(w) = &resp.warning {
                            // Accepted, with a remark. Worth an operator's eye,
                            // not an error: the announce counted.
                            tracing::info!(tracker = %host, warning = %w, "tracker warning");
                        }
                        if event == "stopped" {
                            // A departure answers nothing we act on: no peers
                            // to dial, no swarm counts for a torrent that is
                            // leaving, no interval to wait for.
                            continue;
                        }
                        if resp.interval > 0 {
                            interval = Duration::from_secs(resp.interval as u64);
                        }
                        // `min interval` is a floor, not a suggestion. It exists
                        // so a tracker can refuse to be asked again too soon
                        // whatever the client thinks -- so it wins over
                        // `interval` when the two disagree.
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
                        // loop; 4.0.0 moved that loop here and recorded the
                        // answer only in `cache`, which no reader consults. The
                        // result was a node reporting 0 seeders, 0 leechers and
                        // "never announced" for all 300k torrents while
                        // announcing normally, with no error anywhere.
                        {
                            torrent.scrape_seeders.store(resp.complete, Ordering::Relaxed);
                            torrent.scrape_leechers.store(resp.incomplete, Ordering::Relaxed);
                            let now_unix = typhon_engine::torrent::meta::now_secs();
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
                            // Our own listen port is the marker: the tracker
                            // hands back addresses, and only ours carries this
                            // port on this swarm. Family tells us which half
                            // survived.
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
                                    // Fewer peers returned than asked for means
                                    // the tracker gave us everything it had.
                                    conclusive: (resp.peers.len() as u32) < VERIFY_NUMWANT,
                                    swarm,
                                },
                            );
                        }
                        announced_at_all = true;
                        // The peers a tracker returns are only worth asking for
                        // if something dials them. The engine's queue is where
                        // the DHT puts its finds too, so they share one dial
                        // budget.
                        for peer in &resp.peers {
                            typhon_engine::tracker::enqueue_dial(*peer, torrent.clone());
                        }
                        // Hoard: this tier has us. A race stays in every swarm
                        // it belongs to: a cross-seeded torrent announced only
                        // to its first tracker is absent from the others,
                        // which is where its peers are.
                        if mode == Mode::Hoard {
                            tier_done = true;
                        }
                    }
                    Err(e) => {
                        let kind = classify(&redact(&e));
                        // A 429 is an answer, not an outage: the tracker is up
                        // and asking us to slow down, which the scheduler does,
                        // per tracker. Counted as a failure here, five of them
                        // paused the host for ten minutes, and every torrent
                        // due in that time was pushed back half an hour without
                        // a request, a failure or a minute of lateness to show
                        // for it. Calewood on 2026-09-28: announced one second
                        // in every ten minutes, 1 150 announces in an hour for
                        // 789 000 torrents.
                        // A `failure reason` is an answer too: the tracker is
                        // up and saying no. Counted as an outage, five
                        // "unregistered torrent" in a row -- 35 seconds of a
                        // race's registration retries -- would set the whole
                        // host aside for ten minutes.
                        breaker.record(
                            &host,
                            proves_alive(kind) || book::is_refusal(&e),
                            std::time::Instant::now(),
                        );
                        if kind == "rate_limited" {
                            throttled = true;
                        }
                        if kind == "timeout" {
                            timed_out = true;
                        }
                        cache.count_failed_kind(&host, kind);
                        // At warn, not debug: a breaker that says a tracker
                        // "stopped answering" without saying why sends an
                        // operator to look at their network for a bug that is
                        // here. The host, never the URL -- a tracker URL
                        // carries the passkey in its path, and logs get pasted
                        // into issues.
                        tracing::warn!(tracker = %host, event = %event, error = %redact(&e), "announce failed");
                        // Same reason as the success path: the panel's "last
                        // error" column read an atomic nobody wrote, so every
                        // tracker showed "Success" while the log filled with
                        // refusals. Redacted here too.
                        torrent.last_announce_ok.store(false, Ordering::Relaxed);
                        if let Ok(mut g) = torrent.last_announce_error.lock() {
                            *g = redact(&e).to_string();
                        }
                        if let Ok(mut g) = torrent.current_tracker.lock() {
                            *g = host.clone();
                        }
                        break;
                    }
                }
                // Only an event can be followed by a second send in one pass.
                if event.is_empty() || event == "started" {
                    break;
                }
            }
        }
    }

    if silent {
        // The departures are out (or attempted, once). Nothing else to say
        // until the torrent runs again; the catalogue drops it on refill.
        return if status == TorrentStatus::Error as u8 {
            Outcome { next_in: Duration::from_secs(30 * 60), gone: false, throttled, timed_out, ..gone }
        } else {
            Outcome { throttled, timed_out, ..gone }
        };
    }

    if announced_at_all {
        // What a tracker was actually told, recorded only now that one has
        // answered. Derived from the intention instead, every torrent would
        // look compliant the instant a setting was saved -- which is the
        // failure mode where nothing contradicts itself.
        let sent = policy::announced_peer_id(policy, &tiers);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        *torrent.announced_peer_id.write() = Some((sent, now));
    }

    // ⚠⚠ THE FAST PHASE IS BOUNDED IN TIME. It was not, for the whole life of
    // this code: `RACE_FAST_FOR` was declared, documented as "every 5 seconds
    // for the first minute", and never read -- the compiler said so
    // ("constant RACE_FAST_FOR is never used") and nobody was listening. What
    // actually ran was 5s FOR AS LONG AS THE TORRENT HAD NOT UPLOADED, so a
    // race downloading for half an hour announced ~360 times.
    //
    // For scale: autobrr's default action reannounces every 7s, 25 times, then
    // stops. Five seconds for a minute sits inside what the ecosystem does; an
    // unbounded loop does not, and it is the one number a tracker can spot.
    //
    // Measured on 288 races the same day: 74% find their first peer within 15s
    // and 91% within 60s. Widening to three minutes would buy five points and
    // triple the announces for everyone.
    //
    // Age is counted from `added_time`, NOT from the first announce of this
    // process: every torrent is `job.first` again after a restart, and a race
    // added two hours ago has no business re-entering a burst because the
    // daemon was restarted.
    //
    // Whatever the phase wants, no tracker is asked inside its `min interval`:
    // the book skips a floored tracker, so a fast phase against a tracker that
    // states a floor costs no request at all.
    let fast_window_open = {
        let added = torrent.added_time;
        // An unknown or absurd added_time (0, or in the future) must not grant
        // an unbounded burst: treat it as outside the window.
        added > 0 && typhon_engine::torrent::meta::now_secs().saturating_sub(added) < RACE_FAST_FOR.as_secs() as i64
    };
    let mut next_in = next_announce_in(
        mode,
        interval,
        left,
        job.first,
        fast_window_open,
        announced_at_all && torrent.total_uploaded.load(Ordering::Relaxed) > 0,
    );
    // A race the tracker has not registered yet: the .torrent reached us
    // before the tracker finished taking the upload, and it answers
    // "unregistered torrent". Retried every 7 seconds, 50 times at most --
    // exactly what autobrr's reannounce does for every racing qBittorrent,
    // so it is traffic trackers already expect. No `min interval` is crossed:
    // a tracker that refuses a torrent has not set one for it. Not on a
    // 429 or a timeout -- those are a tracker asking for less, not more.
    let registration_retry = mode == Mode::Race
        && left > 0
        && !announced_at_all
        && !throttled
        && !timed_out
        && {
            let book = torrent.announce_book.lock().unwrap_or_else(|e| e.into_inner());
            book::registration_pending(&book)
        };
    if registration_retry {
        next_in = book::REGISTRATION_RETRY;
    }
    // Coming back before any tracker will accept us is a wasted wake-up.
    {
        let book = torrent.announce_book.lock().unwrap_or_else(|e| e.into_inner());
        let open = book::earliest_open(&book, typhon_engine::torrent::meta::now_secs());
        if open > next_in {
            next_in = open;
        }
    }

    Outcome { info_hash: job.info_hash, next_in, gone: false, throttled, timed_out, registration_retry }
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

    /// ⭐⭐ Calewood's 429 as it came on 2026-09-28: an answer, so the breaker
    /// must not set the tracker aside however many come -- slowing down is the
    /// scheduler's job. A tracker that does not answer at all still trips it.
    #[test]
    fn a_tracker_asking_to_slow_down_is_not_set_aside() {
        let err = "v4: https://t.example/announce 429 Too Many Requests: Merci de bien vouloir boire un cafe, et ralentir votre spam. | v6: https://t.example/announce 429 Too Many Requests";
        let b = Breaker::default();
        let now = std::time::Instant::now();
        for _ in 0..20 {
            b.record("t.example", proves_alive(classify(err)), now);
        }
        assert!(b.allows("t.example", now), "twenty 429s are twenty answers, not an outage");
        for _ in 0..20 {
            b.record("dead.example", proves_alive(classify("v4: tcp connect error: Connection refused")), now);
        }
        assert!(!b.allows("dead.example", now), "a tracker that does not answer is still spared");
    }

    /// ⭐ THE FAST PHASE MUST END. `RACE_FAST_FOR` was declared, documented as
    /// "every 5 seconds for the first minute", and never read: what ran was 5s
    /// for as long as the torrent had not uploaded. A race downloading for half
    /// an hour announced ~360 times, where autobrr's default stops at 25.
    #[test]
    fn a_race_leaves_the_fast_phase_after_the_first_minute() {
        let tracker = Duration::from_secs(1800);
        // Inside the window, nothing uploaded yet: burst.
        assert_eq!(
            next_announce_in(Mode::Race, tracker, 100, true, true, false),
            RACE_FAST
        );
        // Same torrent, same state, one minute later: the burst is over.
        assert_eq!(
            next_announce_in(Mode::Race, tracker, 100, true, false, false),
            RACE_SUSTAINED,
            "past the window a race must not keep announcing every 5s"
        );
        assert_eq!(
            next_announce_in(Mode::Race, tracker, 100, false, false, false),
            RACE_SUSTAINED
        );
    }

    /// Uploading means the swarm found us; the burst has done its job.
    #[test]
    fn a_race_that_uploads_drops_to_the_sustained_rate() {
        let tracker = Duration::from_secs(1800);
        assert_eq!(
            next_announce_in(Mode::Race, tracker, 100, false, true, true),
            RACE_SUSTAINED
        );
    }

    /// A finished race is a seed like any other: whatever the tracker asked for.
    #[test]
    fn a_complete_race_obeys_the_tracker() {
        let tracker = Duration::from_secs(1800);
        assert_eq!(
            next_announce_in(Mode::Race, tracker, 0, true, true, false),
            tracker,
            "left == 0 wins over the fast window"
        );
    }

    /// The hoard never bursts, whatever the window says.
    #[test]
    fn the_hoard_always_obeys_the_tracker() {
        let tracker = Duration::from_secs(1800);
        assert_eq!(
            next_announce_in(Mode::Hoard, tracker, 100, true, true, false),
            tracker
        );
    }

    /// ⭐ A tracker URL carries the passkey in its path. reqwest puts the
    /// whole URL in its error message, so printing that message verbatim
    /// publishes an account credential into the logs.
    ///
    /// (This test had lost its `#[test]` attribute to a neighbour and had not
    /// run in a long time; it passes, which is the good news.)
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
            Job { info_hash: "a".repeat(40), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
        )
        .await;
        let hoard = announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Hoard,
            Job { info_hash: hash.clone(), first: false, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
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
            Job { info_hash: hash.clone(), first: true, forced: false },
        )
        .await;
        assert!(out.gone, "a paused torrent is dropped from the schedule, not announced");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A paused torrent that owes `stopped` to a tracker that never heard
    /// `started` from it sends nothing: there is no session to take back.
    /// (A departure after a `started` is `the_wire_sequence_of_a_whole_session`.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_owed_to_nobody_sends_nothing() {
        use typhon_engine::torrent::meta::ANNOUNCE_EVENT_STOPPED;
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("paused-stop");
        let hash = add(&mgr, "iota", &t.url);
        {
            let st = mgr.get(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
            st.is_paused.store(true, std::sync::atomic::Ordering::Relaxed);
            st.pending_announce_event.store(ANNOUNCE_EVENT_STOPPED, std::sync::atomic::Ordering::Relaxed);
        }
        let (policy, breaker, cache) = parts();
        let out = run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert!(out.gone);
        assert!(t.queries().is_empty(), "got {:?}", t.queries());
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
            Job { info_hash: hash.clone(), first: true, forced: false },
        )
        .await;
        assert!(!out.gone);
        let _ = std::fs::remove_dir_all(root);
    }

    // -----------------------------------------------------------------------
    // Whole-session scenarios against a tracker that records what it receives.
    //
    // Each one drives the real `announce_one` -- book, policy, URL builder,
    // HTTP client -- and asserts on the queries that ARRIVED, never on what
    // our own builder meant to send.
    // -----------------------------------------------------------------------

    struct Recording {
        url: String,
        seen: Arc<std::sync::Mutex<Vec<String>>>,
        _stop: tokio::sync::oneshot::Sender<()>,
    }

    impl Recording {
        fn queries(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
        fn last(&self) -> String {
            self.queries().last().cloned().expect("the tracker received an announce")
        }
    }

    /// `d8:intervali1800e5:peers0:e` -- no `min interval`, so no floor.
    const OPEN_BODY: &[u8] = b"d8:intervali1800e5:peers0:e";

    async fn recording_tracker(body: &'static [u8]) -> Recording {
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let s2 = seen.clone();
        let app = axum::Router::new().route(
            "/announce",
            axum::routing::get(move |axum::extract::RawQuery(q): axum::extract::RawQuery| {
                let s3 = s2.clone();
                async move {
                    s3.lock().unwrap().push(q.unwrap_or_default());
                    body.to_vec()
                }
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
        Recording { url: format!("http://{addr}/announce"), seen, _stop: tx }
    }

    fn param(q: &str, name: &str) -> Option<String> {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == name).then(|| v.to_string())
        })
    }

    fn state(mgr: &Arc<TorrentManager>, hash: &str) -> Arc<typhon_engine::torrent::meta::TorrentState> {
        mgr.get(&typhon_engine::torrent::hex_decode(hash).unwrap()).unwrap()
    }

    async fn run(
        mgr: &Arc<TorrentManager>,
        policy: &Policy,
        breaker: &Breaker,
        cache: &Cache,
        mode: Mode,
        hash: &str,
    ) -> Outcome {
        announce_one(mgr, policy, breaker, cache, 16371, mode, Job { info_hash: hash.to_string(), first: false, forced: false }).await
    }

    /// ⭐⭐⭐ The whole life of a torrent, as the tracker sees it:
    /// `started` with zero counters, periodic announces with no event and the
    /// session's upload, `stopped` on pause, silence while paused, and a new
    /// session -- `started`, counters from zero -- on resume.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wire_sequence_of_a_whole_session() {
        use std::sync::atomic::Ordering;
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("session");
        let hash = add(&mgr, "sess", &t.url);
        let (policy, breaker, cache) = parts();

        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let q = t.last();
        assert_eq!(param(&q, "event").as_deref(), Some("started"), "{q}");
        assert_eq!(param(&q, "uploaded").as_deref(), Some("0"));
        assert_eq!(param(&q, "left").as_deref(), Some("0"));
        assert!(param(&q, "key").is_some());

        state(&mgr, &hash).total_uploaded.fetch_add(5000, Ordering::Relaxed);
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let q = t.last();
        assert_eq!(param(&q, "event"), None, "a periodic announce has no event key: {q}");
        assert_eq!(param(&q, "uploaded").as_deref(), Some("5000"));

        mgr.stop_torrent(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
        let out = run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let q = t.last();
        assert_eq!(param(&q, "event").as_deref(), Some("stopped"), "{q}");
        assert_eq!(param(&q, "uploaded").as_deref(), Some("5000"), "the departure carries the final count");
        assert!(out.gone, "a stopped torrent leaves the schedule once it said so");

        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(t.queries().len(), 3, "silence while paused");

        mgr.start_torrent(&typhon_engine::torrent::hex_decode(&hash).unwrap()).unwrap();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let q = t.last();
        assert_eq!(param(&q, "event").as_deref(), Some("started"), "a resume is a new session: {q}");
        assert_eq!(param(&q, "uploaded").as_deref(), Some("0"), "with counters from zero");
        assert_eq!(t.queries().len(), 4);
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐⭐ After a restart the tracker hears `started` with ZERO counters, not
    /// the lifetime total: a `started` claiming half a terabyte is what
    /// anti-cheat flags, and what a naive tracker credits twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_session_never_reports_the_lifetime_total() {
        use std::sync::atomic::Ordering;
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("lifetime");
        let hash = add(&mgr, "life", &t.url);
        let st = state(&mgr, &hash);
        st.total_uploaded.store(500_000_000_000, Ordering::Relaxed);
        st.total_downloaded.store(16384, Ordering::Relaxed);
        st.begin_announce_session(); // what loading the resume data does
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let q = t.last();
        assert_eq!(param(&q, "event").as_deref(), Some("started"));
        assert_eq!(param(&q, "uploaded").as_deref(), Some("0"), "{q}");
        assert_eq!(param(&q, "downloaded").as_deref(), Some("0"), "{q}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐⭐ `min interval` holds back every kind of announce: the periodic one,
    /// a forced re-announce, and a race in its fast phase. Nothing reaches the
    /// tracker before the floor it stated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn min_interval_holds_back_bumps_and_races_alike() {
        const FLOOR: &[u8] = b"d8:intervali1800e12:min intervali900e5:peers0:e";
        let t = recording_tracker(FLOOR).await;
        let (mgr, root) = manager("floor");
        let hash = add(&mgr, "floor", &t.url);
        let (policy, breaker, cache) = parts();

        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert_eq!(t.queries().len(), 1);
        let hoard = run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        let race = run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert_eq!(t.queries().len(), 1, "inside the floor nothing is sent, whoever asks");
        assert!(race.next_in >= Duration::from_secs(890), "a race waits for the floor too: {:?}", race.next_in);
        assert!(hoard.next_in >= Duration::from_secs(890), "{:?}", hoard.next_in);
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A person's re-announce crosses `min interval`, as qBittorrent's
    /// "Force reannounce" does; the periodic schedule and a race do not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_forced_reannounce_crosses_min_interval_like_qbittorrent() {
        const FLOOR: &[u8] = b"d8:intervali1800e12:min intervali900e5:peers0:e";
        let t = recording_tracker(FLOOR).await;
        let (mgr, root) = manager("forced");
        let hash = add(&mgr, "forced", &t.url);
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert_eq!(t.queries().len(), 1, "not forced: the floor holds");
        announce_one(
            &mgr, &policy, &breaker, &cache, 16371, Mode::Race,
            Job { info_hash: hash.clone(), first: false, forced: true },
        )
        .await;
        assert_eq!(t.queries().len(), 2, "forced: past min interval");
        assert_eq!(param(&t.last(), "event"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐⭐ A race whose tracker has not registered the torrent yet comes back
    /// in 7 seconds, as autobrr's reannounce does -- and a hoard, or a race
    /// that is already seeding, does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_race_is_retried_in_seconds() {
        use std::sync::atomic::Ordering;
        use typhon_engine::torrent::meta::TorrentStatus;
        const UNREG: &[u8] = b"d14:failure reason20:unregistered torrente";
        let t = recording_tracker(UNREG).await;
        let (mgr, root) = manager("unreg");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("unreg", &t.url), "/tmp", false, false)
            .expect("parses");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        let st = state(&mgr, &hash);
        *st.live_trackers.write() = vec![vec![t.url.clone()]];
        st.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let (policy, breaker, cache) = parts();

        let race = run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert!(race.registration_retry);
        assert_eq!(race.next_in, book::REGISTRATION_RETRY);
        for _ in 0..10 {
            let again = run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
            assert!(again.registration_retry, "still refused, still retried");
        }
        assert_eq!(t.queries().len(), 11, "a refusal is an answer: the breaker stays closed");

        let hoard = run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert!(!hoard.registration_retry, "a hoard is not racing");

        st.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        let seed = run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert!(!seed.registration_retry, "nothing left to download, nothing to hurry");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ `completed` reaches every tracker that saw us leeching -- a race
    /// announces to all of them -- and carries `left=0`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_reaches_every_tracker_that_saw_us_leeching() {
        use std::sync::atomic::Ordering;
        use typhon_engine::torrent::meta::{TorrentStatus, ANNOUNCE_EVENT_COMPLETED};
        let a = recording_tracker(OPEN_BODY).await;
        let b = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("completed");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("leech", &a.url), "/tmp", false, false)
            .expect("parses");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        let st = state(&mgr, &hash);
        *st.live_trackers.write() = vec![vec![a.url.clone()], vec![b.url.clone()]];
        st.status.store(TorrentStatus::Downloading as u8, Ordering::Relaxed);
        let (policy, breaker, cache) = parts();

        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        for t in [&a, &b] {
            let q = t.last();
            assert_eq!(param(&q, "event").as_deref(), Some("started"));
            assert_eq!(param(&q, "left").as_deref(), Some("16384"), "a leecher needs its bytes: {q}");
        }

        st.status.store(TorrentStatus::Seeding as u8, Ordering::Relaxed);
        st.pending_announce_event.fetch_or(ANNOUNCE_EVENT_COMPLETED, Ordering::Relaxed);
        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        for t in [&a, &b] {
            let q = t.last();
            assert_eq!(param(&q, "event").as_deref(), Some("completed"), "{q}");
            assert_eq!(param(&q, "left").as_deref(), Some("0"));
        }
        run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        assert_eq!(param(&a.last(), "event"), None, "completed is said once");
        let _ = std::fs::remove_dir_all(root);
    }

    /// BEP 12 in hoard mode: the first tier that answers has us; the next
    /// tier is not announced to at all.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hoard_stops_at_the_first_tier_that_answers() {
        let a = recording_tracker(OPEN_BODY).await;
        let b = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("tiers");
        let hash = add(&mgr, "tier", &a.url);
        *state(&mgr, &hash).live_trackers.write() = vec![vec![a.url.clone()], vec![b.url.clone()]];
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(a.queries().len(), 2);
        assert!(b.queries().is_empty(), "the backup tier is for failure, not for every announce");
        let _ = std::fs::remove_dir_all(root);
    }

    /// ⭐ A fail-over tracker hears `started` first. It used to hear a
    /// periodic announce from a peer it had never registered.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_fail_over_tracker_hears_started_first() {
        let b = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("failover");
        let hash = add(&mgr, "fail", &b.url);
        *state(&mgr, &hash).live_trackers.write() =
            vec![vec!["http://127.0.0.1:1/announce".into()], vec![b.url.clone()]];
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(param(&b.last(), "event").as_deref(), Some("started"));
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(param(&b.last(), "event"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// BEP 3: a `tracker id` comes back as `trackerid` on the next announce.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tracker_id_is_echoed_on_the_next_announce() {
        const WITH_ID: &[u8] = b"d8:intervali1800e5:peers0:10:tracker id5:T-123e";
        let t = recording_tracker(WITH_ID).await;
        let (mgr, root) = manager("trackerid");
        let hash = add(&mgr, "tid", &t.url);
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(param(&t.last(), "trackerid"), None, "nothing to echo on the first one");
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(param(&t.last(), "trackerid").as_deref(), Some("T-123"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// BEP 31: `retry in: never` retires the tracker for the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bep31_never_means_never() {
        const NEVER: &[u8] = b"d14:failure reason14:client refused8:retry in5:nevere";
        let t = recording_tracker(NEVER).await;
        let (mgr, root) = manager("never");
        let hash = add(&mgr, "never", &t.url);
        let (policy, breaker, cache) = parts();
        for _ in 0..3 {
            run(&mgr, &policy, &breaker, &cache, Mode::Race, &hash).await;
        }
        assert_eq!(t.queries().len(), 1, "asked once, told never, never asked again");
        let _ = std::fs::remove_dir_all(root);
    }

    /// Nothing is said while the data is being checked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_torrent_being_checked_says_nothing() {
        use typhon_engine::torrent::meta::TorrentStatus;
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("checking");
        let hash = add(&mgr, "chk", &t.url);
        state(&mgr, &hash).status.store(TorrentStatus::Checking as u8, std::sync::atomic::Ordering::Relaxed);
        let (policy, breaker, cache) = parts();
        let out = run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert!(t.queries().is_empty());
        assert!(!out.gone, "it is looked at again once the check is over");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A torrent whose data vanished cannot serve a piece: it leaves the swarm
    /// with `stopped` and stays silent until a recheck brings it back.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_torrent_whose_data_is_gone_leaves_the_swarm() {
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("error");
        let hash = add(&mgr, "err", &t.url);
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        state(&mgr, &hash).mark_error("No such file or directory");
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(param(&t.last(), "event").as_deref(), Some("stopped"));
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(t.queries().len(), 2, "silent until the data is back");
        let _ = std::fs::remove_dir_all(root);
    }

    /// The list announced to is the live one: a tracker edited at runtime is
    /// what the next announce reaches, not what the .torrent said.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_edited_tracker_list_is_the_one_announced_to() {
        let t = recording_tracker(OPEN_BODY).await;
        let (mgr, root) = manager("edited");
        let (ih, _) = mgr
            .add_torrent_bytes(&torrent_bytes("edit", "http://127.0.0.1:1/announce"), "/tmp", false, true)
            .expect("parses");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        *state(&mgr, &hash).live_trackers.write() = vec![vec![t.url.clone()]];
        let (policy, breaker, cache) = parts();
        run(&mgr, &policy, &breaker, &cache, Mode::Hoard, &hash).await;
        assert_eq!(t.queries().len(), 1, "the edited list is the list");
        let _ = std::fs::remove_dir_all(root);
    }
}
