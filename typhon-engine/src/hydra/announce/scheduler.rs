//! One scheduler and a fixed pool of workers, for any number of torrents.
//!
//! 3.x first ran one goroutine per torrent. At 65k torrents that left ~63k
//! parked goroutines whose stacks the garbage collector had to walk, and the
//! scan alone measured 25% of CPU -- about seven cores, found by pprof on
//! 2026-07-22. The fix was this shape: one scheduler owning a heap of
//! deadlines, N workers, and a count of tasks that does not depend on the
//! catalogue.
//!
//! Tokio has no such collector, so the original reason does not carry over --
//! but the shape still does. 244k sleeping tasks are 244k futures held in
//! memory, each with its own state, for work that is idle by definition: a
//! torrent announces once every thirty minutes.
//!
//! The scheduler owns `states` and the heap outright and never locks them. The
//! workers only ever speak through channels.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// Tasks spawned once: the most announces in flight across every tracker.
/// Idle ones cost a parked future each, not a thread.
const MAX_CONCURRENCY: usize = 4096;
/// Announces in flight to ONE tracker: where it starts, and its bounds.
///
/// ⚠ History, because both earlier shapes failed in production.
/// A fixed pool of 512 for everything capped a 950k catalogue at ~350/s. The
/// first adaptive pool sized itself from throughput x latency (Little's law)
/// -- and a saturated tracker answers SLOWER the harder it is pushed, so the
/// measured latency asked for more concurrency, which raised the latency:
/// on 2026-09-28 it climbed to 4096 in flight, Calewood went from 1 s to 11 s
/// and timed out, and throughput did not move from ~350/s.
/// Now each tracker has its own limit, found the way TCP Vegas finds a
/// window: grow while the tracker answers at its natural speed and work is
/// waiting, shrink as soon as latency shows a queue on its side, or it
/// refuses (429) or times out. A slow tracker only ever slows itself.
const HOST_START: usize = 32;
const HOST_MIN: usize = 4;
const HOST_MAX: usize = 2048;
/// Latency over this multiple of the tracker's best means it is queueing us.
const QUEUE_FACTOR: f64 = 2.0;
/// Under this multiple it answers at its natural speed and may take more.
const CALM_FACTOR: f64 = 1.5;
/// The best latency is the lowest cycle mean of this many recent cycles (5
/// minutes): a route that became slower for good becomes the new normal once
/// the faster cycles have aged out.
///
/// ⚠ It used to be let go by 2% a cycle, and on the bench a queue building
/// slowly on the tracker's side rose at about that pace: the "best" followed it
/// up to 2.6 s on a tracker that answers in 0.5 s, "> 2x best" never fired, and
/// the limit climbed to 1 257 -- the first adaptive pool's runaway, only slower.
const BEST_WINDOW: usize = 30;
/// A limit raised last cycle must have bought at least this share of the rise
/// in more answers (+1/8 in flight, +1/16 answered); if not, the tracker is
/// saturated and the rise is taken back. That catches the queue that latency
/// alone reads too late. A share rather than a fixed gain: the last rise to the
/// ceiling, or +1 on a small limit, is a small rise.
const GROWTH_MUST_PAY: f64 = 0.5;
/// A cut for latency must bring the latency down at least this much; if not,
/// the latency is the route's, not a queue of ours, and becomes the new best.
const CUT_MUST_PAY: f64 = 0.95;
/// Share of a cycle's answers that were 429, and that timed out, above which
/// the tracker's limit shrinks whatever the latency says.
const THROTTLE_BACKOFF: f64 = 0.02;
const TIMEOUT_BACKOFF: f64 = 0.05;
/// Weight of each new sample in the latency shown to the operator.
const LATENCY_ALPHA: f64 = 0.02;
/// A torrent counts as late once its deadline is this far behind: dispatch
/// itself takes a moment, and that is not lateness.
const LATE_AFTER: Duration = Duration::from_secs(5);
/// How often the set of torrents is re-read from the engine.
const RECONCILE: Duration = Duration::from_secs(10);
/// Used when a tracker gives no usable interval.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Floor on a tracker-supplied interval. A tracker asking to be announced to
/// every second is either broken or hostile, and honouring it would be a
/// self-inflicted flood.
const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// The shortest wait a registration retry may ask for, whatever the runner
/// says: a guard against a bug turning the retry into a spin.
const REGISTRATION_RETRY_FLOOR: Duration = Duration::from_secs(5);
/// Let the engine finish loading its resume data before the first announce.
const BOOT_DELAY: Duration = Duration::from_secs(5);
/// Floor on how many torrents may JOIN the schedule per reconcile cycle.
///
/// ⚠ This used to be a FLAT 500, justified as "fifty announces a second, which
/// trackers tolerate". Two things were wrong with that.
///
/// The division was fiction: nothing spread those 500 across the ten seconds,
/// so they left in one burst and the next nine seconds were silent. Measured on
/// 2026-09-17, the announce rate alternated 127/s and 0 with a 10s period --
/// the shape of `RECONCILE`, not of a rate limit.
///
/// And the ceiling was stricter than the regime it protected. A 293k catalogue
/// in steady state announces at `total / interval` = 163/s; admitting at 50/s
/// meant 98 minutes of climb toward a state running three times faster, during
/// which the torrents not yet admitted were announced NOWHERE. At the 1M target
/// it would have been 5h33 toward a regime ten times faster.
///
/// The quota is now derived from that regime (`admit_quota`), and this is only
/// the floor so a small catalogue still joins promptly.
const MIN_NEW_PER_CYCLE: usize = 100;

/// How many torrents may join this cycle, given how many there are in total.
///
/// `total / DEFAULT_INTERVAL` announces per second is what the catalogue will
/// demand once every torrent has a deadline, so admitting at that rate never
/// exceeds the steady state -- it just reaches it. The catalogue is taken on in
/// ONE interval whatever its size: half an hour for 5k as for 1M.
fn admit_quota(total: usize) -> usize {
    let per_cycle = total.saturating_mul(RECONCILE.as_secs() as usize)
        / DEFAULT_INTERVAL.as_secs() as usize;
    per_cycle.max(MIN_NEW_PER_CYCLE)
}

/// A stable offset in `[0, window)`, derived from the info hash.
///
/// Deterministic on purpose, where `rand` would have done: the same torrent
/// always lands on the same offset, so a group admitted together stays spread
/// apart for good instead of re-converging at the next deadline. It is also the
/// only version of this that can be tested -- and an untestable guard is how
/// the unbounded race burst survived a whole release.
///
/// FNV-1a over the hash: no dependency, and hex info hashes differing in one
/// character land far apart.
fn spread(info_hash: &str, window: Duration) -> Duration {
    let millis = window.as_millis() as u64;
    if millis == 0 {
        return Duration::ZERO;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in info_hash.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    Duration::from_millis(h % millis)
}
/// The reschedule offset is at most `wait / JITTER_FRACTION`, and never more
/// than `MAX_JITTER`. Announcing LATE is always safe -- it is announcing early
/// that a tracker minds -- so the offset is only ever added.
const JITTER_FRACTION: u32 = 10;
const MAX_JITTER: Duration = Duration::from_secs(120);

/// Floor between two manual reannounces of the same torrent.
///
/// The button exists to jump the queue, not to become a hammer: a private
/// tracker notices an account that announces the same hash ten times a minute,
/// and that is the one cost this feature could inflict. Same value as
/// `MIN_INTERVAL` on purpose -- the scheduler already treats a minute as the
/// shortest honest gap between two announces of one torrent.
const BUMP_COOLDOWN: Duration = Duration::from_secs(60);

/// How long a torrent waits after an announce, given what the runner asked.
///
/// Under a minute is not an honest gap between two announces of one torrent
/// and falls back to the default -- except a registration retry, which is
/// bounded by the runner and floored here.
fn wait_after(outcome: &Outcome) -> Duration {
    if outcome.registration_retry {
        outcome.next_in.max(REGISTRATION_RETRY_FLOOR)
    } else if outcome.next_in < MIN_INTERVAL {
        DEFAULT_INTERVAL
    } else {
        outcome.next_in
    }
}

/// What the scheduler actually did with one hand-pressed reannounce.
///
/// Until 4.28.0 `bump_now` answered `bool` and the receive arm threw it away,
/// so a refused bump was indistinguishable from an applied one -- and the HTTP
/// route had already answered `{"status":"ok"}` the instant the message entered
/// the channel. A bulk reannounce of 540 torrents could therefore be a complete
/// no-op and report success for every one of them. The outcome travels back now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BumpOutcome {
    /// Moved to the head of the queue.
    Bumped,
    /// Refused: this hash was bumped less than `BUMP_COOLDOWN` ago.
    Cooldown { retry_in: Duration },
    /// Refused: a worker is announcing this hash right now, which is the
    /// announce the caller was asking for.
    InFlight,
}

/// One hand-pressed reannounce, and where to report what became of it.
///
/// `reply` is an `Option` so an internal caller can still fire and forget
/// without inventing a receiver it will never read.
pub struct BumpReq {
    pub info_hash: String,
    /// A re-announce a person asked for (the button, the API). It is allowed
    /// past each tracker's `min interval`, as qBittorrent's "Force reannounce"
    /// is (libtorrent's `ignore_min_interval`). An internal bump -- an owed
    /// event going out now -- is not forced: the event crosses the floor on its
    /// own, and nothing else should.
    pub forced: bool,
    pub reply: Option<oneshot::Sender<BumpOutcome>>,
}

/// What one torrent owes the scheduler.
struct State {
    info_hash: String,
    first_announce: bool,
    /// The next dispatch carries a forced bump. Cleared once it has.
    forced_next: bool,
    in_flight: bool,
    /// Bumped every time this torrent is rescheduled out of band.
    ///
    /// A manual reannounce pushes a second deadline for a hash that already has
    /// one in the heap. Without a way to tell them apart the old deadline fires
    /// later and announces a second time, so the button would cost two
    /// announces instead of one. Deadlines carry the epoch they were made with
    /// and a stale one is dropped on the way out -- lazy deletion, because a
    /// BinaryHeap cannot remove from the middle.
    epoch: u64,
    /// When this torrent was last bumped by hand, for `BUMP_COOLDOWN`.
    last_bump: Option<Instant>,
    /// The interval it was last rescheduled with, in seconds: what it will
    /// cost per second in steady state is its inverse.
    interval_s: u32,
    /// The tracker this torrent is announced to first: the one whose
    /// concurrency limit it waits on.
    host: Arc<str>,
}

/// A deadline in the heap. Ordered by time only; the hash breaks ties so the
/// order is total and the heap is deterministic.
#[derive(PartialEq, Eq)]
struct Deadline {
    at: Instant,
    info_hash: String,
    epoch: u64,
}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.info_hash.cmp(&other.info_hash))
            .then_with(|| self.epoch.cmp(&other.epoch))
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A torrent handed to a worker.
pub struct Job {
    pub info_hash: String,
    pub first: bool,
    /// Asked for by a person: allowed past `min interval` (see `BumpReq`).
    pub forced: bool,
}

/// What a worker reports back.
pub struct Outcome {
    pub info_hash: String,
    /// When to come back. Anything under the floor is replaced by the default.
    pub next_in: Duration,
    /// The torrent is gone from the engine; stop tracking it.
    pub gone: bool,
    /// A tracker answered 429. Fed to the concurrency control, which backs
    /// off instead of pushing harder at a tracker that asks for less.
    pub throttled: bool,
    /// The request timed out: the tracker is not answering in time, which is
    /// load to take off it. A tracker ANSWERING with a failure (unregistered
    /// torrent, bad passkey) is not this -- it answered, and fast.
    pub timed_out: bool,
    /// A race whose tracker has not registered the torrent yet, asking to be
    /// retried in seconds. The one case allowed under `MIN_INTERVAL`: it is
    /// what autobrr's reannounce does (every 7 s, 50 times at most), for a
    /// torrent the tracker is still refusing -- so no `min interval` exists
    /// to cross, and the attempts are bounded by the runner.
    pub registration_retry: bool,
}

/// What the scheduler needs from the engine it serves.
pub trait Catalogue: Send + Sync + 'static {
    /// Every torrent that should be announced right now.
    fn hashes(&self) -> Vec<String>;
    /// The host of the tracker this torrent announces to first. Torrents of
    /// one host share its concurrency limit.
    fn host_of(&self, _info_hash: &str) -> String {
        String::new()
    }
}

/// One tracker as the scheduler last saw it, for the API and the panel.
#[derive(Debug, Clone, Default)]
pub struct TrackerSnapshot {
    pub host: String,
    pub limit: usize,
    pub in_flight: usize,
    /// Due and waiting for a slot on this tracker.
    pub waiting: usize,
    pub latency_ms: u64,
    pub best_ms: u64,
}

/// How far the scheduler has got admitting the catalogue.
///
/// A catalogue joins at `admit_quota()` per `RECONCILE` -- one interval for the
/// whole of it -- so early in a boot many torrents have no deadline yet and no
/// announce behind them. Nothing published that, so the detail panel had to
/// guess -- and guessed "Success", which is the one answer that is certainly
/// wrong about a tracker nobody has spoken to.
///
/// The scheduler owns its `states` map and shares nothing, so this is a
/// snapshot it publishes, not a lock anyone takes.
#[derive(Default)]
pub struct Admission {
    /// Torrents the scheduler has taken on.
    pub admitted: AtomicU64,
    /// Torrents the catalogue holds that it has not reached yet.
    pub waiting: AtomicU64,
    /// Announces per second the schedule needs, x1000: the sum of
    /// 1/interval over every admitted torrent. The line the achieved rate has
    /// to meet; below it, lateness grows.
    pub needed_milli: AtomicU64,
    /// Torrents whose deadline passed more than `LATE_AFTER` ago.
    pub late: AtomicU64,
    /// How late they are, in seconds: median and 90th percentile.
    pub lag_p50_s: AtomicU64,
    pub lag_p90_s: AtomicU64,
    /// Announces allowed in flight right now, and in flight at the sample.
    pub concurrency: AtomicU64,
    pub in_flight: AtomicU64,
    /// Moving average of how long one announce takes, in milliseconds.
    pub latency_ms: AtomicU64,
    /// Share of the last cycle's answers that were 429, in thousandths.
    pub throttled_permille: AtomicU64,
    /// Per tracker, largest queue first.
    pub trackers: std::sync::Mutex<Vec<TrackerSnapshot>>,
}

impl Admission {
    /// Seconds before the last torrent still waiting can expect its turn.
    ///
    /// An upper bound for the whole queue, not a promise for one torrent: the
    /// scheduler admits in catalogue order and this side does not know where
    /// in that order any given hash sits. "At most this long" is the honest
    /// claim, and it is the one worth showing.
    pub fn drain_seconds(&self) -> i64 {
        let waiting = self.waiting.load(Ordering::Relaxed);
        if waiting == 0 {
            return 0;
        }
        // Against the quota for the WHOLE catalogue, which is what the
        // scheduler will actually apply -- not the floor.
        let total = self.admitted.load(Ordering::Relaxed).saturating_add(waiting);
        let cycles = waiting.div_ceil(admit_quota(total as usize) as u64);
        (cycles * RECONCILE.as_secs()) as i64
    }
}

/// Run the scheduler until the process ends.
///
/// `announce` is called on a worker for one torrent, and returns when to come
/// back. It is given no lock and no shared state on purpose: everything the
/// scheduler owns stays on this task.
pub async fn run<C, F, Fut>(
    catalogue: Arc<C>,
    announce: Arc<F>,
    mut bump_rx: mpsc::Receiver<BumpReq>,
    admission: Arc<Admission>,
)
where
    C: Catalogue,
    F: Fn(Job) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Outcome> + Send,
{
    tokio::time::sleep(BOOT_DELAY).await;

    let (work_tx, work_rx) = mpsc::channel::<Job>(2 * MAX_CONCURRENCY);
    let (result_tx, mut result_rx) = mpsc::channel::<(Outcome, Duration)>(2 * MAX_CONCURRENCY);
    let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));

    // Every task the pool may ever use is spawned now; how many are allowed
    // to work at once is decided by the scheduler when it hands out jobs.
    for _ in 0..MAX_CONCURRENCY {
        let rx = work_rx.clone();
        let tx = result_tx.clone();
        let announce = announce.clone();
        tokio::spawn(async move {
            loop {
                let job = {
                    let mut rx = rx.lock().await;
                    match rx.recv().await {
                        Some(j) => j,
                        None => return,
                    }
                };
                let started = Instant::now();
                let outcome = announce(job).await;
                if tx.send((outcome, started.elapsed())).await.is_err() {
                    return;
                }
            }
        });
    }
    drop(result_tx);

    let mut states: HashMap<String, State> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<Deadline>> = BinaryHeap::new();
    let mut reconcile = tokio::time::interval(RECONCILE);
    let mut pool = Pool::default();
    let mut hosts: HashMap<String, Arc<str>> = HashMap::new();

    off_the_runtime(|| reconcile_now(&catalogue, &mut states, &mut heap, &admission, &mut hosts));

    loop {
        // Hand out what is due, tracker by tracker, as far as each one's limit
        // allows. Everything due leaves the heap here -- into its tracker's
        // queue if the tracker is full -- so the heap only ever holds future
        // deadlines and the sleep below cannot spin on one already past.
        pool.dispatch(Instant::now(), &mut heap, &mut states, &work_tx);

        // Sleep until the next deadline, or an hour if there is nothing to do.
        // An empty heap is normal on an engine with no torrents; it must not
        // become a busy loop. A result arriving frees a slot and wakes us too.
        let next = heap
            .peek()
            .map(|Reverse(d)| d.at)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            _ = tokio::time::sleep_until(next) => {}
            Some((outcome, took)) = result_rx.recv() => {
                pool.done(&outcome, took);
                let Some(state) = states.get_mut(&outcome.info_hash) else {
                    continue;
                };
                state.in_flight = false;
                if outcome.gone {
                    states.remove(&outcome.info_hash);
                    continue;
                }
                state.first_announce = false;
                let wait = wait_after(&outcome);
                // Spread the return too, or the group re-forms: everyone
                // admitted together gets the same `wait` and comes due in the
                // same millisecond, thirty minutes later, for ever. The offset
                // is a fraction of the wait and is capped, so it disperses the
                // pack without meaningfully delaying any single torrent.
                let jitter = spread(
                    &outcome.info_hash,
                    (wait / JITTER_FRACTION).min(MAX_JITTER),
                );
                state.interval_s = wait.as_secs().clamp(1, u32::MAX as u64) as u32;
                heap.push(Reverse(Deadline {
                    at: Instant::now() + wait + jitter,
                    info_hash: outcome.info_hash,
                    epoch: state.epoch,
                }));
            }
            Some(req) = bump_rx.recv() => {
                let outcome = bump_now(&mut states, &mut heap, req.info_hash, req.forced);
                if let Some(reply) = req.reply {
                    // The caller may have given up waiting; that is its right
                    // and not an error here.
                    let _ = reply.send(outcome);
                }
            }
            _ = reconcile.tick() => {
                let health = off_the_runtime(|| measure(&states, &heap, &pool, Instant::now()));
                pool.adjust();
                health.publish(&admission, &pool);
                off_the_runtime(|| reconcile_now(&catalogue, &mut states, &mut heap, &admission, &mut hosts));
            }
        }
    }
}

/// One tracker's share of the pool.
struct Tracker {
    limit: usize,
    in_flight: usize,
    /// Due, waiting for a slot on this tracker, earliest deadline first.
    pending: std::collections::VecDeque<Deadline>,
    /// Best CYCLE-MEAN latency seen, let go a little every cycle
    /// (`BEST_WINDOW`).
    ///
    /// ⚠ The mean of a cycle, never its fastest answer. The first version
    /// compared this cycle's mean to the fastest single answer ever seen, and
    /// a tracker's answers are spread: on the bench, fastest 0.16 s against a
    /// mean of 0.39 s with no queue at all. The ratio read as congestion, and
    /// the limit fell to 17. Like must be compared with like.
    best_s: f64,
    /// Cycle means of the last `BEST_WINDOW` cycles that had answers.
    means: std::collections::VecDeque<f64>,
    /// Answers in the previous cycle, and whether that cycle raised the limit.
    prev_n: u64,
    /// The limit before last cycle's rise, if it rose.
    grew: Option<usize>,
    /// The cycle mean when last cycle cut the limit for latency, if it did.
    cut_at: Option<f64>,
    /// This cycle's answers.
    n: u64,
    sum_s: f64,
    throttled: u64,
    timed_out: u64,
    /// Mean latency of the last cycle that had answers, for display.
    shown_s: f64,
}

impl Tracker {
    fn new() -> Self {
        Tracker {
            limit: HOST_START,
            in_flight: 0,
            pending: std::collections::VecDeque::new(),
            best_s: f64::INFINITY,
            means: std::collections::VecDeque::new(),
            prev_n: 0,
            grew: None,
            cut_at: None,
            n: 0,
            sum_s: 0.0,
            throttled: 0,
            timed_out: 0,
            shown_s: 0.0,
        }
    }

    fn done(&mut self, outcome: &Outcome, took: Duration) {
        self.in_flight = self.in_flight.saturating_sub(1);
        // A torrent found gone never reached a tracker.
        if outcome.gone {
            return;
        }
        let t = took.as_secs_f64();
        self.n += 1;
        self.sum_s += t;
        self.throttled += outcome.throttled as u64;
        self.timed_out += outcome.timed_out as u64;
    }

    /// One control step, at the end of a cycle.
    fn adjust(&mut self) {
        // No answers, nothing learnt: an idle tracker keeps its limit.
        if self.n == 0 {
            return;
        }
        let n = self.n as f64;
        let mean = self.sum_s / n;
        self.shown_s = mean;
        self.means.push_back(mean);
        if self.means.len() > BEST_WINDOW {
            self.means.pop_front();
        }
        self.best_s = self.means.iter().cloned().fold(f64::INFINITY, f64::min);
        if let Some(was) = self.cut_at.take() {
            if mean > was * CUT_MUST_PAY {
                // ⭐ Prod, 2026-09-28: archive.org answered one early cycle in
                // 0.29 s, then 0.6-1 s whatever we sent. Every cycle read as a
                // queue, and the limit sat at the floor with 26 000 announces
                // waiting. Sending less did not make it faster: that latency
                // is where the tracker is, not a queue we built.
                self.means.clear();
                self.means.push_back(mean);
                self.best_s = mean;
            }
        }
        let busy = !self.pending.is_empty() || self.in_flight * 10 >= self.limit * 9;
        let unpaid = self.grew.take().filter(|&before| {
            let rise = self.limit as f64 / before as f64 - 1.0;
            n < self.prev_n as f64 * (1.0 + rise * GROWTH_MUST_PAY)
        });
        if let Some(before) = unpaid {
            // The rise bought nothing: take it back, exactly. Not a step down
            // from where it got to -- +1/8 then -1/10 is still a net rise, and
            // the limit crept past the knee 1% every other cycle. Not below
            // where it started either: at a limit of 12 a rise is one request,
            // its gain drowns in the noise, and each missed probe cost 10% --
            // archive.org fell from 28 to 12 while answering at its usual speed.
            self.limit = before;
        } else if self.throttled as f64 / n > THROTTLE_BACKOFF || self.timed_out as f64 / n > TIMEOUT_BACKOFF {
            self.limit = (self.limit * 4 / 5).max(HOST_MIN);
        } else if mean > QUEUE_FACTOR * self.best_s {
            // The tracker is queueing us: more concurrency is only a longer queue.
            self.limit = (self.limit * 9 / 10).max(HOST_MIN);
            self.cut_at = Some(mean);
        } else if mean < CALM_FACTOR * self.best_s && busy {
            let before = self.limit;
            self.limit = (self.limit + (self.limit / 8).max(1)).min(HOST_MAX);
            if self.limit > before {
                self.grew = Some(before);
            }
        }
        self.prev_n = self.n;
        self.n = 0;
        self.sum_s = 0.0;
        self.throttled = 0;
        self.timed_out = 0;
    }
}

/// Every tracker's share, and what ties them together.
#[derive(Default)]
struct Pool {
    trackers: HashMap<Arc<str>, Tracker>,
    /// Which tracker each in-flight torrent holds a slot on.
    holding: HashMap<String, Arc<str>>,
    in_flight: usize,
    /// Latency across all answers, for display only.
    latency_s: f64,
    /// Across all trackers, this cycle: answers and 429s. Read by `measure`
    /// before `adjust` resets the per-tracker counters.
    answered: u64,
    throttled: u64,
    last_throttled: f64,
}

impl Pool {
    /// Move every due deadline to its tracker's queue, then hand out work from
    /// each queue as far as that tracker's limit allows.
    fn dispatch(
        &mut self,
        now: Instant,
        heap: &mut BinaryHeap<Reverse<Deadline>>,
        states: &mut HashMap<String, State>,
        work_tx: &mpsc::Sender<Job>,
    ) {
        while let Some(Reverse(d)) = heap.peek() {
            if d.at > now {
                break;
            }
            let Reverse(d) = heap.pop().expect("peeked");
            let Some(state) = states.get(&d.info_hash) else { continue };
            // A deadline made before a bump: its replacement is in the heap.
            if d.epoch != state.epoch || state.in_flight {
                continue;
            }
            self.trackers
                .entry(state.host.clone())
                .or_insert_with(Tracker::new)
                .pending
                .push_back(d);
        }
        for (host, t) in self.trackers.iter_mut() {
            while t.in_flight < t.limit && self.in_flight < MAX_CONCURRENCY {
                let Some(d) = t.pending.pop_front() else { break };
                // Checked again: it may have been bumped or dispatched while
                // it waited here.
                let Some(state) = states.get_mut(&d.info_hash) else { continue };
                if d.epoch != state.epoch || state.in_flight {
                    continue;
                }
                let job = Job {
                    info_hash: d.info_hash.clone(),
                    first: state.first_announce,
                    forced: state.forced_next,
                };
                // try_send, not send: a full queue means the workers are
                // behind, and blocking here would stop the scheduler from
                // reading results -- which is what empties that queue.
                match work_tx.try_send(job) {
                    Ok(()) => {
                        state.in_flight = true;
                        state.forced_next = false;
                        t.in_flight += 1;
                        self.in_flight += 1;
                        self.holding.insert(d.info_hash, host.clone());
                    }
                    Err(_) => {
                        t.pending.push_front(d);
                        return;
                    }
                }
            }
        }
    }

    fn done(&mut self, outcome: &Outcome, took: Duration) {
        self.in_flight = self.in_flight.saturating_sub(1);
        if let Some(host) = self.holding.remove(&outcome.info_hash) {
            if let Some(t) = self.trackers.get_mut(&host) {
                t.done(outcome, took);
            }
        }
        if !outcome.gone {
            let t = took.as_secs_f64();
            self.latency_s = if self.answered == 0 && self.latency_s == 0.0 { t } else { self.latency_s + LATENCY_ALPHA * (t - self.latency_s) };
            self.answered += 1;
            self.throttled += outcome.throttled as u64;
        }
    }

    fn adjust(&mut self) {
        self.last_throttled = if self.answered == 0 { 0.0 } else { self.throttled as f64 / self.answered as f64 };
        self.answered = 0;
        self.throttled = 0;
        for t in self.trackers.values_mut() {
            t.adjust();
        }
        // A tracker nothing is queued on and nothing is in flight to holds no
        // state worth keeping; it starts over if it comes back.
        self.trackers.retain(|_, t| t.in_flight > 0 || !t.pending.is_empty() || t.n > 0);
    }

    fn limit(&self) -> usize {
        self.trackers.values().map(|t| t.limit).sum::<usize>().min(MAX_CONCURRENCY)
    }
}

/// The schedule's health at one instant.
struct Health {
    needed_per_s: f64,
    late: usize,
    lag_p50: Duration,
    lag_p90: Duration,
}

impl Health {
    fn publish(&self, a: &Admission, pool: &Pool) {
        a.needed_milli.store((self.needed_per_s * 1000.0) as u64, Ordering::Relaxed);
        a.late.store(self.late as u64, Ordering::Relaxed);
        a.lag_p50_s.store(self.lag_p50.as_secs(), Ordering::Relaxed);
        a.lag_p90_s.store(self.lag_p90.as_secs(), Ordering::Relaxed);
        a.concurrency.store(pool.limit() as u64, Ordering::Relaxed);
        a.in_flight.store(pool.in_flight as u64, Ordering::Relaxed);
        a.latency_ms.store((pool.latency_s * 1000.0) as u64, Ordering::Relaxed);
        a.throttled_permille.store((pool.last_throttled * 1000.0).round() as u64, Ordering::Relaxed);
        let mut v: Vec<TrackerSnapshot> = pool
            .trackers
            .iter()
            .map(|(h, t)| TrackerSnapshot {
                host: h.to_string(),
                limit: t.limit,
                in_flight: t.in_flight,
                waiting: t.pending.len(),
                latency_ms: (t.shown_s * 1000.0) as u64,
                best_ms: if t.best_s.is_finite() { (t.best_s * 1000.0) as u64 } else { 0 },
            })
            .collect();
        v.sort_by(|a, b| b.waiting.cmp(&a.waiting).then(b.in_flight.cmp(&a.in_flight)));
        if let Ok(mut g) = a.trackers.lock() {
            *g = v;
        }
    }
}

/// What the schedule needs and how far behind it is.
///
/// Late torrents are the ones queued on a full tracker, plus any past-due
/// deadline still in the heap. Stale entries (older epochs, forgotten
/// torrents, torrents a worker holds) are not lateness and are skipped.
fn measure(
    states: &HashMap<String, State>,
    heap: &BinaryHeap<Reverse<Deadline>>,
    pool: &Pool,
    now: Instant,
) -> Health {
    let needed_per_s: f64 = states.values().map(|s| 1.0 / s.interval_s.max(1) as f64).sum();
    let live = |d: &Deadline| -> Option<Duration> {
        let s = states.get(&d.info_hash)?;
        if s.epoch != d.epoch || s.in_flight || d.at + LATE_AFTER > now {
            return None;
        }
        Some(now.duration_since(d.at))
    };
    let mut lags: Vec<Duration> = heap
        .iter()
        .filter_map(|Reverse(d)| live(d))
        .chain(pool.trackers.values().flat_map(|t| t.pending.iter()).filter_map(|d| live(d)))
        .collect();
    let late = lags.len();
    let pct = |lags: &mut Vec<Duration>, p: usize| -> Duration {
        if lags.is_empty() {
            return Duration::ZERO;
        }
        let i = (lags.len() * p / 100).min(lags.len() - 1);
        *lags.select_nth_unstable(i).1
    };
    let lag_p50 = pct(&mut lags, 50);
    let lag_p90 = pct(&mut lags, 90);
    Health { needed_per_s, late, lag_p50, lag_p90 }
}

/// Run a synchronous pass without holding a runtime worker hostage.
///
/// `reconcile_now` walks the whole catalogue: at 300k torrents it takes over
/// half a second, every ten seconds, with no await inside. On the worker it
/// ran on, every other task waited it out -- the API's accept loop included,
/// so `/health` itself stalled on the same beat. `block_in_place` hands those
/// tasks to another worker first. It needs the multi-thread runtime; on any
/// other (the unit tests) the pass simply runs inline.
fn off_the_runtime<R>(f: impl FnOnce() -> R) -> R {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Bring the tracked set in line with the engine's: add what appeared, forget
/// what left.
fn reconcile_now<C: Catalogue>(
    catalogue: &Arc<C>,
    states: &mut HashMap<String, State>,
    heap: &mut BinaryHeap<Reverse<Deadline>>,
    admission: &Admission,
    hosts: &mut HashMap<String, Arc<str>>,
) -> usize {
    let live = catalogue.hashes();
    let total = live.len() as u64;
    // Derived from the catalogue in hand: the bigger it is, the faster it has
    // to be taken on, because the steady state it is heading for is faster too.
    let quota = admit_quota(live.len());
    let mut seen = std::collections::HashSet::with_capacity(live.len());
    let mut added = 0usize;
    for hash in live {
        seen.insert(hash.clone());
        if states.contains_key(&hash) {
            continue;
        }
        // The rest join on the next cycle. `seen` already holds them, so they
        // are not mistaken for departures in the meantime.
        if added >= quota {
            continue;
        }
        added += 1;
        states.insert(
            hash.clone(),
            State {
                info_hash: hash.clone(),
                first_announce: true,
                forced_next: false,
                in_flight: false,
                epoch: 0,
                last_bump: None,
                interval_s: DEFAULT_INTERVAL.as_secs() as u32,
                // Interned: a million torrents share a handful of trackers.
                host: {
                    let h = catalogue.host_of(&hash);
                    hosts.entry(h.clone()).or_insert_with(|| Arc::from(h.as_str())).clone()
                },
            },
        );
        // A torrent that has just appeared announces now: it is either newly
        // added or the engine has just started, and both want the tracker told
        // rather than a thirty-minute wait.
        //
        // "Now" is spread across the cycle rather than taken literally: a whole
        // quota leaving in the same millisecond is the burst this admission
        // limit exists to prevent, and it would only get bigger now that the
        // quota scales with the catalogue.
        let at = Instant::now() + spread(&hash, RECONCILE);
        heap.push(Reverse(Deadline { at, info_hash: hash, epoch: 0 }));
    }
    // A torrent in flight is left alone: its worker still holds it, and its
    // result will remove it.
    states.retain(|h, s| seen.contains(h) || s.in_flight);

    // Published after the retain, so the two numbers describe the same moment.
    let admitted = states.len() as u64;
    admission.admitted.store(admitted, Ordering::Relaxed);
    admission.waiting.store(total.saturating_sub(admitted), Ordering::Relaxed);
    added
}

/// Move one torrent to the head of the queue, out of band.
///
/// Returns whether it was actually scheduled: a caller too soon after the last
/// bump, or one whose torrent is already being announced, is told no rather
/// than silently dropped.
///
/// A torrent the scheduler has never seen is admitted here and now, deliberately
/// outside the admission quota: that quota exists to stop a whole catalogue
/// arriving at once, and one person pressing one button is not a herd.
fn bump_now(
    states: &mut HashMap<String, State>,
    heap: &mut BinaryHeap<Reverse<Deadline>>,
    hash: String,
    forced: bool,
) -> BumpOutcome {
    let now = Instant::now();
    let state = states.entry(hash.clone()).or_insert_with(|| State {
        info_hash: hash.clone(),
        first_announce: true,
        forced_next: false,
        in_flight: false,
        epoch: 0,
        last_bump: None,
        interval_s: DEFAULT_INTERVAL.as_secs() as u32,
        host: Arc::from(""),
    });
    if let Some(last) = state.last_bump {
        let since = now.duration_since(last);
        if since < BUMP_COOLDOWN {
            return BumpOutcome::Cooldown { retry_in: BUMP_COOLDOWN - since };
        }
    }
    // Already with a worker: the announce the caller wants is in progress.
    if state.in_flight {
        return BumpOutcome::InFlight;
    }
    // The epoch moves first: every deadline made before this one is now stale
    // and will be dropped when it surfaces.
    state.epoch += 1;
    state.last_bump = Some(now);
    state.forced_next |= forced;
    heap.push(Reverse(Deadline { at: now, info_hash: hash, epoch: state.epoch }));
    BumpOutcome::Bumped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_come_out_earliest_first() {
        let mut heap = BinaryHeap::new();
        let now = Instant::now();
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(30), info_hash: "c".into(), epoch: 0 }));
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(10), info_hash: "a".into(), epoch: 0 }));
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(20), info_hash: "b".into(), epoch: 0 }));
        let order: Vec<String> =
            std::iter::from_fn(|| heap.pop().map(|Reverse(d)| d.info_hash)).collect();
        assert_eq!(order, ["a", "b", "c"], "a min-heap, not a max-heap");
    }

    #[test]
    fn two_torrents_due_at_the_same_instant_keep_a_total_order() {
        // Without the tie-break the ordering would be by insertion luck, and a
        // heap that is not a total order can loop on peek/pop.
        let now = Instant::now();
        let a = Deadline { at: now, info_hash: "aaa".into(), epoch: 0 };
        let b = Deadline { at: now, info_hash: "bbb".into(), epoch: 0 };
        assert!(a < b);
    }

    /// ⭐ The catalogue joins the schedule in slices, not all at once.
    ///
    /// Production answered 429 on the first switch because every torrent was
    /// admitted with a deadline of "now": 300k announces in one burst. Fifty a
    /// second is what a tracker tolerates.
    #[test]
    /// ⭐ An estimate that says "at most", and says nothing when there is
    /// nothing to wait for.
    ///
    /// The panel puts this next to real timings, so a zero backlog has to read
    /// as "no wait" rather than as a small one.
    #[test]
    fn the_drain_estimate_bounds_the_queue_and_is_zero_when_it_is_empty() {
        let a = Admission::default();
        assert_eq!(a.drain_seconds(), 0, "nothing waiting is no wait, not 10s");

        // One short cycle still costs a whole cycle: the scheduler admits on a
        // tick, not continuously.
        a.waiting.store(1, Ordering::Relaxed);
        assert_eq!(a.drain_seconds(), RECONCILE.as_secs() as i64);

        a.waiting.store(MIN_NEW_PER_CYCLE as u64, Ordering::Relaxed);
        assert_eq!(a.drain_seconds(), RECONCILE.as_secs() as i64);

        a.waiting.store(MIN_NEW_PER_CYCLE as u64 + 1, Ordering::Relaxed);
        assert_eq!(a.drain_seconds(), 2 * RECONCILE.as_secs() as i64);

        // The number that made this worth showing: a 300k catalogue at boot.
        // It used to be ~100 minutes, because admission was capped at a flat
        // 500/cycle whatever the size. Derived from the regime, the whole
        // catalogue is now taken on in ONE interval.
        a.waiting.store(300_000, Ordering::Relaxed);
        let minutes = a.drain_seconds() as f64 / 60.0;
        let target = DEFAULT_INTERVAL.as_secs_f64() / 60.0;
        assert!(
            (minutes - target).abs() <= 1.0,
            "{minutes} minutes for 300k, expected about {target}"
        );
    }

    /// The scheduler publishes what it admitted, and what is still queued.
    ///
    /// Without this the API can only guess, and its guess was "Success".
    #[test]
    fn admission_is_published_as_the_catalogue_joins() {
        struct Big(usize);
        impl Catalogue for Big {
            fn hashes(&self) -> Vec<String> {
                (0..self.0).map(|i| format!("{i:040x}")).collect()
            }
        }
        let catalogue = Arc::new(Big(MIN_NEW_PER_CYCLE * 3));
        let admission = Admission::default();
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();

        reconcile_now(&catalogue, &mut states, &mut heap, &admission, &mut HashMap::new());
        assert_eq!(admission.admitted.load(Ordering::Relaxed), MIN_NEW_PER_CYCLE as u64);
        assert_eq!(admission.waiting.load(Ordering::Relaxed), (MIN_NEW_PER_CYCLE * 2) as u64);

        reconcile_now(&catalogue, &mut states, &mut heap, &admission, &mut HashMap::new());
        assert_eq!(admission.admitted.load(Ordering::Relaxed), (MIN_NEW_PER_CYCLE * 2) as u64);
        assert_eq!(admission.waiting.load(Ordering::Relaxed), MIN_NEW_PER_CYCLE as u64);

        reconcile_now(&catalogue, &mut states, &mut heap, &admission, &mut HashMap::new());
        assert_eq!(admission.waiting.load(Ordering::Relaxed), 0, "the whole catalogue is in");
        assert_eq!(admission.drain_seconds(), 0);
    }

    /// ⭐ Admission never runs faster than the steady state it leads to.
    ///
    /// That is the whole justification for the quota: a catalogue of N torrents
    /// will announce at `N / interval` per second once every one of them has a
    /// deadline, so taking them on at that same rate adds nothing a tracker was
    /// not going to see anyway. The old flat 500/cycle was BOTH too slow for a
    /// large catalogue (98 minutes of climb at 300k, 5h33 at 1M, during which
    /// the torrents not yet admitted announced nowhere) and unrelated to the
    /// load it claimed to bound.
    #[test]
    fn admission_never_exceeds_the_steady_state_it_leads_to() {
        // ⚠ Above the floor only. Below it the floor wins, on purpose: a 5k
        // catalogue has a 2.8/s regime, and admitting at the floor's 10/s to
        // get it in within minutes is a rate no tracker has an opinion about.
        // The rule is about large catalogues, where the absolute numbers bite.
        for total in [50_000usize, 300_000, 1_000_000] {
            let quota = admit_quota(total);
            assert!(quota > MIN_NEW_PER_CYCLE, "{total} should be past the floor");
            let admit_per_s = quota as f64 / RECONCILE.as_secs_f64();
            let steady_per_s = total as f64 / DEFAULT_INTERVAL.as_secs_f64();
            assert!(
                admit_per_s <= steady_per_s * 1.05,
                "{total}: admitting {admit_per_s}/s exceeds the {steady_per_s}/s regime"
            );
        }
        // And the floor never lets a small catalogue announce at a rate worth
        // noticing in absolute terms.
        let floor_per_s = MIN_NEW_PER_CYCLE as f64 / RECONCILE.as_secs_f64();
        assert!(floor_per_s <= 20.0, "the floor alone is {floor_per_s}/s");
    }

    /// Whatever its size, the catalogue is taken on in about one interval.
    #[test]
    fn a_catalogue_joins_in_one_interval_whatever_its_size() {
        for total in [50_000usize, 300_000, 1_000_000] {
            let cycles = total as f64 / admit_quota(total) as f64;
            let minutes = cycles * RECONCILE.as_secs_f64() / 60.0;
            let target = DEFAULT_INTERVAL.as_secs_f64() / 60.0;
            assert!(
                (minutes - target).abs() <= 1.0,
                "{total} torrents take {minutes} minutes, expected about {target}"
            );
        }
    }

    /// A small catalogue must not crawl: `300 / 1800` is one torrent per cycle,
    /// which would take an hour to admit three hundred torrents.
    #[test]
    fn a_small_catalogue_gets_the_floor() {
        assert_eq!(admit_quota(300), MIN_NEW_PER_CYCLE);
        assert_eq!(admit_quota(0), MIN_NEW_PER_CYCLE);
    }

    /// ⭐ THE SHAPE, not just the rate. A quota that leaves in one burst is the
    /// thundering herd this limit exists to prevent -- and the burst gets bigger
    /// now that the quota scales. Measured on 2026-09-17 before this existed:
    /// the announce rate alternated 127/s and 0 on a 10s period.
    #[test]
    fn the_offset_spreads_a_group_across_the_window() {
        let window = RECONCILE;
        let offsets: Vec<u128> = (0..500)
            .map(|i| spread(&format!("{i:040x}"), window).as_millis())
            .collect();

        assert!(
            offsets.iter().all(|o| *o < window.as_millis()),
            "an offset must stay inside the window"
        );
        // Spread across the window rather than clumped: every tenth of the
        // window holds some of them.
        let tenth = window.as_millis() / 10;
        let buckets: std::collections::HashSet<u128> =
            offsets.iter().map(|o| o / tenth).collect();
        assert_eq!(buckets.len(), 10, "500 torrents left {} of 10 slots empty", 10 - buckets.len());
    }

    /// Deterministic: the same torrent always lands on the same offset, so a
    /// group that was spread apart stays apart instead of re-converging.
    #[test]
    fn the_offset_is_stable_for_a_given_torrent() {
        let h = "a".repeat(40);
        assert_eq!(spread(&h, RECONCILE), spread(&h, RECONCILE));
        assert_ne!(
            spread(&h, RECONCILE),
            spread(&"b".repeat(40), RECONCILE),
            "two torrents must not share one offset"
        );
        assert_eq!(spread(&h, Duration::ZERO), Duration::ZERO, "a zero window is not a panic");
    }

    fn fresh(hash: &str) -> State {
        State {
            info_hash: hash.into(),
            first_announce: false,
            forced_next: false,
            in_flight: false,
            epoch: 0,
            last_bump: None,
            interval_s: DEFAULT_INTERVAL.as_secs() as u32,
            host: Arc::from(""),
        }
    }

    fn answered(throttled: bool) -> Outcome {
        Outcome { info_hash: "x".into(), next_in: DEFAULT_INTERVAL, gone: false, throttled, timed_out: false, registration_retry: false }
    }

    /// Feed a tracker one cycle of `n` answers at `lat` seconds and step it.
    fn cycle(t: &mut Tracker, lat: f64, n: usize, busy: bool) {
        for _ in 0..n {
            t.in_flight += 1;
            t.done(&answered(false), Duration::from_secs_f64(lat));
        }
        if busy {
            t.pending.push_back(Deadline { at: Instant::now(), info_hash: "q".into(), epoch: 0 });
        } else {
            t.pending.clear();
        }
        t.adjust();
    }

    /// A tracker serving at most `cap` answers a second at `base` seconds each;
    /// past that, requests queue on its side (Little's law) -- what Calewood
    /// did on 2026-09-28. Work always waiting; one cycle at the current limit.
    fn saturable_cycle(t: &mut Tracker, base: f64, cap: f64) -> (f64, f64) {
        let rate = (t.limit as f64 / base).min(cap);
        let lat = (t.limit as f64 / rate).max(base);
        let n = (rate * RECONCILE.as_secs_f64()) as usize;
        cycle(t, lat, n, true);
        (rate, lat)
    }

    /// Answering at its natural speed, with work waiting and each rise buying
    /// more answers: an eighth more per cycle.
    #[test]
    fn a_calm_busy_tracker_is_given_more() {
        let mut t = Tracker::new();
        let mut seen = vec![t.limit];
        for _ in 0..4 {
            saturable_cycle(&mut t, 0.5, 100_000.0);
            seen.push(t.limit);
        }
        assert_eq!(seen, [32, 36, 40, 45, 50]);
    }

    /// ⭐ The runaway of both earlier versions: a tracker that saturates at
    /// 350/s with 0.5 s answers has its knee at 175 in flight. The limit must
    /// settle near it -- not climb into the thousands while the tracker's queue
    /// grows -- and throughput must stay at the tracker's ceiling.
    #[test]
    fn a_saturated_tracker_is_held_near_its_knee() {
        let mut t = Tracker::new();
        let (mut rates, mut lats, mut limits) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..400 {
            let (r, l) = saturable_cycle(&mut t, 0.5, 350.0);
            rates.push(r);
            lats.push(l);
            limits.push(t.limit);
        }
        let tail = 100;
        let max_limit = *limits[limits.len() - tail..].iter().max().unwrap();
        let mean_rate: f64 = rates[rates.len() - tail..].iter().sum::<f64>() / tail as f64;
        let max_lat = lats[lats.len() - tail..].iter().cloned().fold(0.0, f64::max);
        assert!(max_limit < 175 * 2, "limit reached {max_limit}, the knee is 175");
        assert!(mean_rate > 350.0 * 0.85, "throughput {mean_rate:.0}/s, ceiling 350/s");
        assert!(max_lat < 0.5 * 2.0, "latency {max_lat:.2} s: the tracker's queue must not grow");
    }

    /// Naturally spread answers -- half fast, half slow, no queue -- are not
    /// congestion: the limit must not fall.
    #[test]
    fn a_natural_spread_is_not_congestion() {
        let mut t = Tracker::new();
        for _ in 0..20 {
            // As many answers as slots: each rise pays.
            for i in 0..t.limit * 2 {
                t.in_flight += 1;
                let lat = if i % 2 == 0 { 0.1 } else { 0.9 };
                t.done(&answered(false), Duration::from_secs_f64(lat));
            }
            t.pending.push_back(Deadline { at: Instant::now(), info_hash: "q".into(), epoch: 0 });
            t.adjust();
        }
        assert!(t.limit > HOST_START, "grew to {}, never cut", t.limit);
    }

    /// Nothing waiting, nothing to gain: the limit holds.
    #[test]
    fn an_idle_tracker_keeps_its_limit() {
        let mut t = Tracker::new();
        for _ in 0..10 {
            cycle(&mut t, 0.5, 5, false);
        }
        assert_eq!(t.limit, HOST_START);
    }

    /// Latency rising with our load must shrink the limit: a tracker falling
    /// to a tenth of its capacity is queueing us, and each cut shortens that
    /// queue -- so the cuts pay and continue down to the new knee.
    #[test]
    fn latency_rising_with_load_shrinks_the_limit() {
        let mut t = Tracker::new();
        for _ in 0..40 {
            saturable_cycle(&mut t, 0.5, 350.0);
        }
        let before = t.limit;
        for _ in 0..60 {
            saturable_cycle(&mut t, 0.5, 35.0);
        }
        // The new knee is 35/s x 0.5 s = 17.5 in flight.
        assert!(t.limit < 40, "{before} -> {}", t.limit);
    }

    /// A route that got slower for good becomes the new normal once the faster
    /// cycles have left the window, and the tracker grows again.
    #[test]
    fn a_permanent_slowdown_is_learnt() {
        let mut t = Tracker::new();
        for _ in 0..5 {
            saturable_cycle(&mut t, 0.5, 100_000.0);
        }
        let mut lowest = t.limit;
        for _ in 0..(BEST_WINDOW * 3) {
            saturable_cycle(&mut t, 1.2, 100_000.0);
            lowest = lowest.min(t.limit);
        }
        assert!(t.best_s > 1.0, "best moved to {}", t.best_s);
        assert!(t.limit > lowest, "grew again at the new normal: {} from {lowest}", t.limit);
    }

    /// A tracker slower than its one lucky cycle, whatever we send, is not
    /// congested: the limit must grow, not sit at the floor.
    #[test]
    fn latency_that_is_not_ours_is_learnt() {
        let mut t = Tracker::new();
        cycle(&mut t, 0.29, 20, true);
        for i in 0..40 {
            let lat = if i % 3 == 0 { 0.6 } else { 0.9 };
            let n = t.limit * 10;
            cycle(&mut t, lat, n, true);
        }
        assert!(t.limit > HOST_START, "stuck at {}", t.limit);
    }

    /// A probe that misses because of noise costs nothing: a tracker answering
    /// at its usual speed never ends below where it started.
    #[test]
    fn a_missed_probe_costs_nothing() {
        let mut t = Tracker::new();
        t.limit = 12;
        for i in 0..60 {
            // Answers flat at 180 a cycle whatever the limit, +-3%.
            let n = if i % 2 == 0 { 185 } else { 175 };
            cycle(&mut t, 0.6, n, true);
            assert!(t.limit >= 12, "fell to {} at cycle {i}", t.limit);
        }
    }

    #[test]
    fn refusals_and_timeouts_shrink_the_limit() {
        let mut t = Tracker::new();
        cycle(&mut t, 0.5, 20, true);
        let before = t.limit;
        for i in 0..100 {
            t.in_flight += 1;
            t.done(&answered(i < 5), Duration::from_millis(500));
        }
        t.adjust();
        assert_eq!(t.limit, before * 4 / 5);
        let before = t.limit;
        for i in 0..100 {
            t.in_flight += 1;
            let o = Outcome { timed_out: i < 10, ..answered(false) };
            t.done(&o, Duration::from_millis(500));
        }
        t.adjust();
        assert_eq!(t.limit, before * 4 / 5);
    }

    #[test]
    fn the_limit_stays_within_its_bounds() {
        let mut t = Tracker::new();
        for _ in 0..200 {
            saturable_cycle(&mut t, 0.5, 1e9);
        }
        assert_eq!(t.limit, HOST_MAX);
        for _ in 0..200 {
            for _ in 0..10 {
                t.in_flight += 1;
                t.done(&answered(true), Duration::from_millis(500));
            }
            t.adjust();
        }
        assert_eq!(t.limit, HOST_MIN, "backing off never goes to zero");
    }

    /// ⭐ Isolation: a saturated tracker fills its own queue and leaves the
    /// others their slots.
    #[test]
    fn a_full_tracker_does_not_hold_the_others() {
        let (tx, mut rx) = mpsc::channel::<Job>(10_000);
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();
        let now = Instant::now() + Duration::from_secs(10);
        let slow: Arc<str> = Arc::from("slow.example");
        let fast: Arc<str> = Arc::from("fast.example");
        for i in 0..100 {
            let h = format!("s{i}");
            states.insert(h.clone(), State { host: slow.clone(), ..fresh(&h) });
            heap.push(Reverse(Deadline { at: now, info_hash: h, epoch: 0 }));
        }
        for i in 0..10 {
            let h = format!("f{i}");
            states.insert(h.clone(), State { host: fast.clone(), ..fresh(&h) });
            heap.push(Reverse(Deadline { at: now, info_hash: h, epoch: 0 }));
        }
        let mut pool = Pool::default();
        pool.dispatch(now, &mut heap, &mut states, &tx);
        let mut sent = Vec::new();
        while let Ok(j) = rx.try_recv() {
            sent.push(j.info_hash);
        }
        assert_eq!(sent.iter().filter(|h| h.starts_with('f')).count(), 10, "every fast torrent went out");
        assert_eq!(sent.iter().filter(|h| h.starts_with('s')).count(), HOST_START, "the slow tracker got its limit, no more");
        assert_eq!(pool.trackers[&slow].pending.len(), 100 - HOST_START, "the rest waits on the slow tracker's queue");
        assert!(heap.is_empty(), "nothing due is left in the heap to spin on");
        // A freed slot on the slow tracker takes the next in its queue.
        let first = sent.iter().find(|h| h.starts_with('s')).unwrap().clone();
        states.get_mut(&first).unwrap().in_flight = false;
        pool.done(&Outcome { info_hash: first, ..answered(false) }, Duration::from_millis(100));
        pool.dispatch(now, &mut heap, &mut states, &tx);
        assert_eq!(rx.try_recv().map(|j| j.info_hash.starts_with('s')), Ok(true));
    }

    /// Lateness counts only real deadlines: not the future, not a torrent a
    /// worker holds, not a deadline a bump made stale.
    #[test]
    fn lateness_is_measured_on_live_deadlines_only() {
        let now = Instant::now() + Duration::from_secs(1000);
        let mut states = HashMap::new();
        states.insert("late".to_string(), fresh("late"));
        states.insert("soon".to_string(), State { interval_s: 60, ..fresh("soon") });
        states.insert("held".to_string(), State { in_flight: true, ..fresh("held") });
        states.insert("bumped".to_string(), State { epoch: 1, ..fresh("bumped") });
        let mut heap = BinaryHeap::new();
        let d = |h: &str, at: Instant, epoch: u64| Reverse(Deadline { at, info_hash: h.into(), epoch });
        heap.push(d("late", now - Duration::from_secs(100), 0));
        heap.push(d("soon", now + Duration::from_secs(10), 0));
        heap.push(d("held", now - Duration::from_secs(500), 0));
        heap.push(d("bumped", now - Duration::from_secs(700), 0));
        heap.push(d("bumped", now + Duration::from_secs(5), 1));
        heap.push(d("forgotten", now - Duration::from_secs(900), 0));
        let h = measure(&states, &heap, &Pool::default(), now);
        assert_eq!(h.late, 1);
        assert_eq!(h.lag_p50, Duration::from_secs(100));
        let want = 3.0 / 1800.0 + 1.0 / 60.0;
        assert!((h.needed_per_s - want).abs() < 1e-9, "{}", h.needed_per_s);
    }



    #[test]
    fn a_deadline_just_due_is_not_late() {
        let now = Instant::now() + Duration::from_secs(100);
        let mut states = HashMap::new();
        states.insert("a".to_string(), fresh("a"));
        let mut heap = BinaryHeap::new();
        heap.push(Reverse(Deadline { at: now - Duration::from_secs(2), info_hash: "a".into(), epoch: 0 }));
        assert_eq!(measure(&states, &heap, &Pool::default(), now).late, 0, "dispatch takes a moment");
    }

    /// A person's bump lifts `min interval`; an owed event's does not.
    #[test]
    fn a_person_s_bump_is_forced_and_an_internal_one_is_not() {
        let (mut states, mut heap) = (HashMap::new(), BinaryHeap::new());
        assert_eq!(bump_now(&mut states, &mut heap, "ev".into(), false), BumpOutcome::Bumped);
        assert!(!states["ev"].forced_next, "an owed event is not a forced re-announce");
        assert_eq!(bump_now(&mut states, &mut heap, "btn".into(), true), BumpOutcome::Bumped);
        assert!(states["btn"].forced_next, "the button is");
    }

    /// Only a registration retry comes back in seconds; any other sub-minute
    /// request still becomes the default, and the retry has its own floor.
    #[test]
    fn only_a_registration_retry_may_wait_under_a_minute() {
        let o = |secs: u64, retry: bool| Outcome {
            next_in: Duration::from_secs(secs),
            registration_retry: retry,
            ..answered(false)
        };
        assert_eq!(wait_after(&o(7, true)), Duration::from_secs(7));
        assert_eq!(wait_after(&o(1, true)), REGISTRATION_RETRY_FLOOR, "never a spin");
        assert_eq!(wait_after(&o(7, false)), DEFAULT_INTERVAL);
        assert_eq!(wait_after(&o(900, false)), Duration::from_secs(900));
    }

    /// ⭐ The whole point of the button: skip the queue.
    #[test]
    fn a_bump_goes_to_the_head_of_the_queue() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();
        states.insert("a".to_string(), fresh("a"));
        states.insert("b".to_string(), fresh("b"));
        let now = Instant::now();
        // "a" is not due for half an hour.
        heap.push(Reverse(Deadline { at: now + DEFAULT_INTERVAL, info_hash: "a".into(), epoch: 0 }));
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(60), info_hash: "b".into(), epoch: 0 }));

        assert_eq!(bump_now(&mut states, &mut heap, "a".into(), true), BumpOutcome::Bumped);

        let Reverse(head) = heap.peek().expect("a deadline");
        assert_eq!(head.info_hash, "a", "the bumped torrent must come out first");
        assert!(head.at <= Instant::now(), "and it must be due now, not later");
    }

    /// Without the epoch this test fails by announcing twice: the deadline the
    /// bump replaced is still in the heap and nothing marks it as superseded.
    #[test]
    fn a_deadline_made_before_a_bump_is_stale() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();
        states.insert("a".to_string(), fresh("a"));
        heap.push(Reverse(Deadline { at: Instant::now(), info_hash: "a".into(), epoch: 0 }));

        assert_eq!(bump_now(&mut states, &mut heap, "a".into(), true), BumpOutcome::Bumped);

        let epoch = states["a"].epoch;
        assert_eq!(epoch, 1);
        let stale = heap.iter().filter(|Reverse(d)| d.epoch != epoch).count();
        let live = heap.iter().filter(|Reverse(d)| d.epoch == epoch).count();
        assert_eq!((stale, live), (1, 1), "one superseded deadline, one current");
    }

    /// The button must not become a hammer on a private tracker.
    #[test]
    fn a_second_bump_inside_the_cooldown_is_refused() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();
        states.insert("a".to_string(), fresh("a"));

        assert_eq!(
            bump_now(&mut states, &mut heap, "a".into(), true),
            BumpOutcome::Bumped,
            "first press works"
        );
        assert!(
            matches!(
                bump_now(&mut states, &mut heap, "a".into(), true),
                BumpOutcome::Cooldown { .. }
            ),
            "second press is refused"
        );
        assert_eq!(heap.len(), 1, "and schedules nothing extra");
    }

    /// ⭐ The refusal must be NAMED, not merely counted. 540 torrents were left
    /// on `invalid passkey` on 2026-09-12 because a bulk reannounce inside the
    /// cooldown answered ok for every one of them and did nothing.
    #[test]
    fn a_refused_bump_says_why_and_when_to_come_back() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();
        states.insert("a".to_string(), fresh("a"));

        assert_eq!(bump_now(&mut states, &mut heap, "a".into(), true), BumpOutcome::Bumped);
        match bump_now(&mut states, &mut heap, "a".into(), true) {
            BumpOutcome::Cooldown { retry_in } => {
                assert!(retry_in <= BUMP_COOLDOWN, "never longer than the cooldown");
                assert!(!retry_in.is_zero(), "and a caller can be told when to retry");
            }
            other => panic!("expected a cooldown refusal, got {other:?}"),
        }

        // A torrent already with a worker is refused for its own reason: the
        // announce being asked for is the one in progress.
        states.insert("b".to_string(), State { in_flight: true, ..fresh("b") });
        assert_eq!(bump_now(&mut states, &mut heap, "b".into(), true), BumpOutcome::InFlight);
    }

    /// A torrent still waiting its turn to join must be announceable by hand:
    /// at 500 per cycle a 300k catalogue takes over an hour to be admitted, and
    /// "wait an hour" is not an answer to someone pressing reannounce.
    #[test]
    fn a_bump_admits_a_torrent_the_scheduler_has_never_seen() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();

        assert_eq!(bump_now(&mut states, &mut heap, "new".into(), true), BumpOutcome::Bumped);

        assert!(states.contains_key("new"), "admitted outside MIN_NEW_PER_CYCLE");
        assert!(states["new"].first_announce, "and it announces as a first announce");
        assert_eq!(heap.len(), 1);
    }

    /// A tracker asking for a one-second interval is broken or hostile, and
    /// honouring it would be a flood we inflicted on ourselves.
    #[test]
    fn an_absurd_tracker_interval_falls_back_to_the_default() {
        let clamp = |d: Duration| if d < MIN_INTERVAL { DEFAULT_INTERVAL } else { d };
        assert_eq!(clamp(Duration::from_secs(1)), DEFAULT_INTERVAL);
        assert_eq!(clamp(Duration::from_secs(0)), DEFAULT_INTERVAL);
        assert_eq!(clamp(Duration::from_secs(1800)), Duration::from_secs(1800));
        // Exactly the floor is honoured: it is a floor, not a threshold.
        assert_eq!(clamp(MIN_INTERVAL), MIN_INTERVAL);
    }
}
