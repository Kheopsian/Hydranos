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

/// Sized for ~200k torrents: throughput times latency, not a number of cores.
const WORKERS: usize = 512;
/// How often the set of torrents is re-read from the engine.
const RECONCILE: Duration = Duration::from_secs(10);
/// Used when a tracker gives no usable interval.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Floor on a tracker-supplied interval. A tracker asking to be announced to
/// every second is either broken or hostile, and honouring it would be a
/// self-inflicted flood.
const MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Let the engine finish loading its resume data before the first announce.
const BOOT_DELAY: Duration = Duration::from_secs(5);
/// How many torrents may JOIN the schedule per reconcile cycle.
///
/// Anti thundering-herd, and the number is not arbitrary: 500 per ten seconds
/// is fifty announces a second, which trackers tolerate. Admitting the whole
/// catalogue at once makes every torrent due at the same instant -- 300k
/// announces in a burst, which is how a tracker answers 429 and how an account
/// gets noticed.
const MAX_NEW_PER_CYCLE: usize = 500;
/// Floor between two manual reannounces of the same torrent.
///
/// The button exists to jump the queue, not to become a hammer: a private
/// tracker notices an account that announces the same hash ten times a minute,
/// and that is the one cost this feature could inflict. Same value as
/// `MIN_INTERVAL` on purpose -- the scheduler already treats a minute as the
/// shortest honest gap between two announces of one torrent.
const BUMP_COOLDOWN: Duration = Duration::from_secs(60);

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
    pub reply: Option<oneshot::Sender<BumpOutcome>>,
}

/// What one torrent owes the scheduler.
struct State {
    info_hash: String,
    first_announce: bool,
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
}

/// What a worker reports back.
pub struct Outcome {
    pub info_hash: String,
    /// When to come back. Anything under the floor is replaced by the default.
    pub next_in: Duration,
    /// The torrent is gone from the engine; stop tracking it.
    pub gone: bool,
}

/// What the scheduler needs from the engine it serves.
pub trait Catalogue: Send + Sync + 'static {
    /// Every torrent that should be announced right now.
    fn hashes(&self) -> Vec<String>;
}

/// How far the scheduler has got admitting the catalogue.
///
/// A 300k catalogue joins at `MAX_NEW_PER_CYCLE` per `RECONCILE`, so for the
/// first hour or so of a boot most torrents have no deadline yet and no
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
        let cycles = waiting.div_ceil(MAX_NEW_PER_CYCLE as u64);
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

    let (work_tx, work_rx) = mpsc::channel::<Job>(2 * WORKERS);
    let (result_tx, mut result_rx) = mpsc::channel::<Outcome>(2 * WORKERS);
    let work_rx = Arc::new(tokio::sync::Mutex::new(work_rx));

    for _ in 0..WORKERS {
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
                let outcome = announce(job).await;
                if tx.send(outcome).await.is_err() {
                    return;
                }
            }
        });
    }
    drop(result_tx);

    let mut states: HashMap<String, State> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<Deadline>> = BinaryHeap::new();
    let mut reconcile = tokio::time::interval(RECONCILE);

    reconcile_now(&catalogue, &mut states, &mut heap, &admission);

    loop {
        // Sleep until the next deadline, or an hour if there is nothing to do.
        // An empty heap is normal on an engine with no torrents; it must not
        // become a busy loop.
        let next = heap
            .peek()
            .map(|Reverse(d)| d.at)
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));

        tokio::select! {
            _ = tokio::time::sleep_until(next) => {
                let now = Instant::now();
                while let Some(Reverse(d)) = heap.peek() {
                    if d.at > now {
                        break;
                    }
                    let Some(state) = states.get_mut(&d.info_hash) else {
                        heap.pop();
                        continue;
                    };
                    // A deadline made before a bump: its replacement is already
                    // in the heap, so firing this one would announce twice.
                    if d.epoch != state.epoch {
                        heap.pop();
                        continue;
                    }
                    // A worker still holds it. Its Outcome will reschedule.
                    if state.in_flight {
                        heap.pop();
                        continue;
                    }
                    let job = Job { info_hash: d.info_hash.clone(), first: state.first_announce };
                    // try_send, not send: a full queue means the workers are
                    // behind, and blocking here would stop the scheduler from
                    // reading results -- which is what empties that queue.
                    match work_tx.try_send(job) {
                        Ok(()) => {
                            state.in_flight = true;
                            heap.pop();
                        }
                        Err(_) => break,
                    }
                }
            }
            Some(outcome) = result_rx.recv() => {
                let Some(state) = states.get_mut(&outcome.info_hash) else {
                    continue;
                };
                state.in_flight = false;
                if outcome.gone {
                    states.remove(&outcome.info_hash);
                    continue;
                }
                state.first_announce = false;
                let wait = if outcome.next_in < MIN_INTERVAL {
                    DEFAULT_INTERVAL
                } else {
                    outcome.next_in
                };
                heap.push(Reverse(Deadline {
                    at: Instant::now() + wait,
                    info_hash: outcome.info_hash,
                    epoch: state.epoch,
                }));
            }
            Some(req) = bump_rx.recv() => {
                let outcome = bump_now(&mut states, &mut heap, req.info_hash);
                if let Some(reply) = req.reply {
                    // The caller may have given up waiting; that is its right
                    // and not an error here.
                    let _ = reply.send(outcome);
                }
            }
            _ = reconcile.tick() => {
                reconcile_now(&catalogue, &mut states, &mut heap, &admission);
            }
        }
    }
}

/// Bring the tracked set in line with the engine's: add what appeared, forget
/// what left.
fn reconcile_now<C: Catalogue>(
    catalogue: &Arc<C>,
    states: &mut HashMap<String, State>,
    heap: &mut BinaryHeap<Reverse<Deadline>>,
    admission: &Admission,
) {
    let live = catalogue.hashes();
    let total = live.len() as u64;
    let mut seen = std::collections::HashSet::with_capacity(live.len());
    let mut added = 0usize;
    for hash in live {
        seen.insert(hash.clone());
        if states.contains_key(&hash) {
            continue;
        }
        // The rest join on the next cycle. `seen` already holds them, so they
        // are not mistaken for departures in the meantime.
        if added >= MAX_NEW_PER_CYCLE {
            continue;
        }
        added += 1;
        states.insert(
            hash.clone(),
            State {
                info_hash: hash.clone(),
                first_announce: true,
                in_flight: false,
                epoch: 0,
                last_bump: None,
            },
        );
        // A torrent that has just appeared announces now: it is either newly
        // added or the engine has just started, and both want the tracker told
        // rather than a thirty-minute wait.
        heap.push(Reverse(Deadline { at: Instant::now(), info_hash: hash, epoch: 0 }));
    }
    // A torrent in flight is left alone: its worker still holds it, and its
    // result will remove it.
    states.retain(|h, s| seen.contains(h) || s.in_flight);

    // Published after the retain, so the two numbers describe the same moment.
    let admitted = states.len() as u64;
    admission.admitted.store(admitted, Ordering::Relaxed);
    admission.waiting.store(total.saturating_sub(admitted), Ordering::Relaxed);
}

/// Move one torrent to the head of the queue, out of band.
///
/// Returns whether it was actually scheduled: a caller too soon after the last
/// bump, or one whose torrent is already being announced, is told no rather
/// than silently dropped.
///
/// A torrent the scheduler has never seen is admitted here and now, deliberately
/// outside `MAX_NEW_PER_CYCLE`: that quota exists to stop a whole catalogue
/// arriving at once, and one person pressing one button is not a herd.
fn bump_now(
    states: &mut HashMap<String, State>,
    heap: &mut BinaryHeap<Reverse<Deadline>>,
    hash: String,
) -> BumpOutcome {
    let now = Instant::now();
    let state = states.entry(hash.clone()).or_insert_with(|| State {
        info_hash: hash.clone(),
        first_announce: true,
        in_flight: false,
        epoch: 0,
        last_bump: None,
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

        a.waiting.store(MAX_NEW_PER_CYCLE as u64, Ordering::Relaxed);
        assert_eq!(a.drain_seconds(), RECONCILE.as_secs() as i64);

        a.waiting.store(MAX_NEW_PER_CYCLE as u64 + 1, Ordering::Relaxed);
        assert_eq!(a.drain_seconds(), 2 * RECONCILE.as_secs() as i64);

        // The number that made this worth showing: a 300k catalogue at boot.
        a.waiting.store(300_000, Ordering::Relaxed);
        let minutes = a.drain_seconds() as f64 / 60.0;
        assert!((99.0..=101.0).contains(&minutes), "{minutes} minutes for 300k");
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
        let catalogue = Arc::new(Big(MAX_NEW_PER_CYCLE * 3));
        let admission = Admission::default();
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();

        reconcile_now(&catalogue, &mut states, &mut heap, &admission);
        assert_eq!(admission.admitted.load(Ordering::Relaxed), MAX_NEW_PER_CYCLE as u64);
        assert_eq!(admission.waiting.load(Ordering::Relaxed), (MAX_NEW_PER_CYCLE * 2) as u64);

        reconcile_now(&catalogue, &mut states, &mut heap, &admission);
        assert_eq!(admission.admitted.load(Ordering::Relaxed), (MAX_NEW_PER_CYCLE * 2) as u64);
        assert_eq!(admission.waiting.load(Ordering::Relaxed), MAX_NEW_PER_CYCLE as u64);

        reconcile_now(&catalogue, &mut states, &mut heap, &admission);
        assert_eq!(admission.waiting.load(Ordering::Relaxed), 0, "the whole catalogue is in");
        assert_eq!(admission.drain_seconds(), 0);
    }

    #[test]
    fn no_more_than_a_slice_of_the_catalogue_joins_per_cycle() {
        assert_eq!(MAX_NEW_PER_CYCLE, 500);
        let per_second = MAX_NEW_PER_CYCLE as f64 / RECONCILE.as_secs_f64();
        assert!(per_second <= 50.0, "{per_second}/s is more than a tracker tolerates");
        // And a catalogue of 300k takes a bounded, knowable time to enter.
        let cycles = 300_000_f64 / MAX_NEW_PER_CYCLE as f64;
        let minutes = cycles * RECONCILE.as_secs_f64() / 60.0;
        assert!(minutes < 120.0, "{minutes} minutes to admit the catalogue is too slow");
    }

    fn fresh(hash: &str) -> State {
        State {
            info_hash: hash.into(),
            first_announce: false,
            in_flight: false,
            epoch: 0,
            last_bump: None,
        }
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

        assert_eq!(bump_now(&mut states, &mut heap, "a".into()), BumpOutcome::Bumped);

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

        assert_eq!(bump_now(&mut states, &mut heap, "a".into()), BumpOutcome::Bumped);

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
            bump_now(&mut states, &mut heap, "a".into()),
            BumpOutcome::Bumped,
            "first press works"
        );
        assert!(
            matches!(
                bump_now(&mut states, &mut heap, "a".into()),
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

        assert_eq!(bump_now(&mut states, &mut heap, "a".into()), BumpOutcome::Bumped);
        match bump_now(&mut states, &mut heap, "a".into()) {
            BumpOutcome::Cooldown { retry_in } => {
                assert!(retry_in <= BUMP_COOLDOWN, "never longer than the cooldown");
                assert!(!retry_in.is_zero(), "and a caller can be told when to retry");
            }
            other => panic!("expected a cooldown refusal, got {other:?}"),
        }

        // A torrent already with a worker is refused for its own reason: the
        // announce being asked for is the one in progress.
        states.insert("b".to_string(), State { in_flight: true, ..fresh("b") });
        assert_eq!(bump_now(&mut states, &mut heap, "b".into()), BumpOutcome::InFlight);
    }

    /// A torrent still waiting its turn to join must be announceable by hand:
    /// at 500 per cycle a 300k catalogue takes over an hour to be admitted, and
    /// "wait an hour" is not an answer to someone pressing reannounce.
    #[test]
    fn a_bump_admits_a_torrent_the_scheduler_has_never_seen() {
        let mut states = HashMap::new();
        let mut heap = BinaryHeap::new();

        assert_eq!(bump_now(&mut states, &mut heap, "new".into()), BumpOutcome::Bumped);

        assert!(states.contains_key("new"), "admitted outside MAX_NEW_PER_CYCLE");
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
