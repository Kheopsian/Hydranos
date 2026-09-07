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
use std::time::Duration;

use tokio::sync::mpsc;
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

/// What one torrent owes the scheduler.
struct State {
    info_hash: String,
    first_announce: bool,
    in_flight: bool,
}

/// A deadline in the heap. Ordered by time only; the hash breaks ties so the
/// order is total and the heap is deterministic.
#[derive(PartialEq, Eq)]
struct Deadline {
    at: Instant,
    info_hash: String,
}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at).then_with(|| self.info_hash.cmp(&other.info_hash))
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

/// Run the scheduler until the process ends.
///
/// `announce` is called on a worker for one torrent, and returns when to come
/// back. It is given no lock and no shared state on purpose: everything the
/// scheduler owns stays on this task.
pub async fn run<C, F, Fut>(catalogue: Arc<C>, announce: Arc<F>)
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

    reconcile_now(&catalogue, &mut states, &mut heap);

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
                }));
            }
            _ = reconcile.tick() => {
                reconcile_now(&catalogue, &mut states, &mut heap);
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
) {
    let live = catalogue.hashes();
    let mut seen = std::collections::HashSet::with_capacity(live.len());
    for hash in live {
        seen.insert(hash.clone());
        if states.contains_key(&hash) {
            continue;
        }
        states.insert(
            hash.clone(),
            State { info_hash: hash.clone(), first_announce: true, in_flight: false },
        );
        // A torrent that has just appeared announces now: it is either newly
        // added or the engine has just started, and both want the tracker told
        // rather than a thirty-minute wait.
        heap.push(Reverse(Deadline { at: Instant::now(), info_hash: hash }));
    }
    // A torrent in flight is left alone: its worker still holds it, and its
    // result will remove it.
    states.retain(|h, s| seen.contains(h) || s.in_flight);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadlines_come_out_earliest_first() {
        let mut heap = BinaryHeap::new();
        let now = Instant::now();
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(30), info_hash: "c".into() }));
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(10), info_hash: "a".into() }));
        heap.push(Reverse(Deadline { at: now + Duration::from_secs(20), info_hash: "b".into() }));
        let order: Vec<String> =
            std::iter::from_fn(|| heap.pop().map(|Reverse(d)| d.info_hash)).collect();
        assert_eq!(order, ["a", "b", "c"], "a min-heap, not a max-heap");
    }

    #[test]
    fn two_torrents_due_at_the_same_instant_keep_a_total_order() {
        // Without the tie-break the ordering would be by insertion luck, and a
        // heap that is not a total order can loop on peek/pop.
        let now = Instant::now();
        let a = Deadline { at: now, info_hash: "aaa".into() };
        let b = Deadline { at: now, info_hash: "bbb".into() };
        assert!(a < b);
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
