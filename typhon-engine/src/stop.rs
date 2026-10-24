//! The signal the background loops wait on to end themselves.
//!
//! A stop used to reach only the API and the final flush: every periodic task
//! (reconcile, jobs, rules, RSS, the resume sweeps...) kept running, and kept
//! writing to the store and the engines, while the departures were sent and the
//! resume state was flushed.
//!
//! The rule for a loop is: stop BETWEEN two iterations, never in the middle of
//! one. `sleep` and `tick` are the only places a loop listens, so an iteration
//! that has started always finishes its write; what the stop cuts is the wait
//! for the next one.
//!
//! One instance for the process (`global`, raised by the daemon's
//! `shutdown::begin_stop`), and any number of private ones: a test that stops
//! its own `Stop` cannot reach another test's loops.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::watch;

#[derive(Clone)]
pub struct Stop(Arc<watch::Sender<bool>>);

impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}

impl Stop {
    pub fn new() -> Self {
        Stop(Arc::new(watch::channel(false).0))
    }

    /// Raise the signal. Final: nothing lowers it again.
    pub fn begin(&self) {
        self.0.send_replace(true);
    }

    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the signal is raised (at once if it already is).
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|set| *set).await;
    }

    /// Wait `d`. True when the whole wait went by and the loop may run its
    /// next iteration; false when the stop began, and the loop must return.
    pub async fn sleep(&self, d: Duration) -> bool {
        if self.is_set() {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => !self.is_set(),
            _ = self.wait() => false,
        }
    }

    /// `Interval::tick` with the same answer as `sleep`.
    pub async fn tick(&self, interval: &mut tokio::time::Interval) -> bool {
        if self.is_set() {
            return false;
        }
        tokio::select! {
            _ = interval.tick() => !self.is_set(),
            _ = self.wait() => false,
        }
    }
}

static GLOBAL: OnceLock<Stop> = OnceLock::new();

/// The process-wide signal.
pub fn global() -> &'static Stop {
    GLOBAL.get_or_init(Stop::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_sleep_runs_its_course_until_the_stop_begins() {
        let stop = Stop::new();
        assert!(
            stop.sleep(Duration::from_secs(5)).await,
            "no stop: the whole wait goes by"
        );
        let s = stop.clone();
        let waiting = tokio::spawn(async move { s.sleep(Duration::from_secs(3600)).await });
        tokio::task::yield_now().await;
        stop.begin();
        assert!(
            !waiting.await.unwrap(),
            "the stop ends an hour-long wait at once"
        );
        assert!(
            !stop.sleep(Duration::from_secs(1)).await,
            "and nothing waits once it has begun"
        );
        stop.wait().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_tick_follows_the_same_rule() {
        let stop = Stop::new();
        let mut iv = tokio::time::interval(Duration::from_secs(60));
        assert!(stop.tick(&mut iv).await);
        stop.begin();
        assert!(!stop.tick(&mut iv).await);
    }

    #[tokio::test(start_paused = true)]
    async fn two_instances_are_independent() {
        let (a, b) = (Stop::new(), Stop::new());
        a.begin();
        assert!(a.is_set() && !b.is_set());
        assert!(b.sleep(Duration::from_secs(1)).await);
    }
}
