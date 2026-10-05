//! What each tracker is owed, and what to send it now.
//!
//! BEP 3 events belong to a (torrent, tracker) pair, not to a torrent:
//! `started` opens a session with ONE tracker, and `completed` and `stopped`
//! are said to a tracker that heard `started`. The announcer used to keep one
//! event per torrent and spend it on whichever tracker came first, so a
//! fail-over tracker heard a periodic announce without ever hearing `started`,
//! a second tracker never heard `completed`, and an event that met a tracker
//! that was down was simply lost.
//!
//! Pure functions over `TrackerSlot`, so every rule below is a unit test and
//! not a network scenario.

use std::time::Duration;

use typhon_engine::torrent::meta::{
    tracker_key, TrackerSlot, ANNOUNCE_EVENT_COMPLETED, ANNOUNCE_EVENT_STOPPED,
};
use typhon_engine::tracker::http::{retry_hint, AnnounceResponse, RetryHint};

/// What to do with one tracker on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Announce, with this BEP 3 event ("" = periodic, no event key at all).
    Send(&'static str),
    Skip(Why),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// BEP 31 `retry in: never`.
    Disabled,
    /// Inside `min interval` (unless forced), `retry in` or `Retry-After`.
    Floor,
    /// Hoard: an earlier tracker of the tier list already has us.
    TierDone,
    /// Paused, and nothing is owed to this tracker.
    Quiet,
}

/// The slot for a tracker URL, created on first contact.
pub fn slot_mut<'a>(book: &'a mut Vec<TrackerSlot>, url: &str) -> &'a mut TrackerSlot {
    let key = tracker_key(url);
    if let Some(i) = book.iter().position(|s| s.key == key) {
        return &mut book[i];
    }
    book.push(TrackerSlot { key, ..Default::default() });
    book.last_mut().expect("just pushed")
}

/// File the events the engine raised onto the trackers they are owed to.
///
/// Only a tracker that heard `started` is owed `completed` or `stopped`: one
/// that never heard of us has nothing to be told. A stop whose torrent is no
/// longer paused was undone before we got here; the resume already opened a
/// new session, so there is no departure to send.
pub fn file_owed(book: &mut [TrackerSlot], owed: u8, paused: bool) {
    for slot in book.iter_mut().filter(|s| s.started) {
        if owed & ANNOUNCE_EVENT_COMPLETED != 0 {
            slot.completed_owed = true;
        }
        if owed & ANNOUNCE_EVENT_STOPPED != 0 && paused {
            slot.stopped_owed = true;
        }
    }
}

/// What to send this tracker now.
///
/// Order matters and is the whole of the rule set:
/// - events already owed go out first and are exempt from the floor and the
///   tier: they are one-shot, and `completed` before `stopped` so a download
///   finished and stopped in one breath still counts as a snatch;
/// - a paused torrent says nothing else;
/// - in hoard mode, a tracker behind one that already has us is left alone;
/// - BEP 31 `retry in` and `Retry-After` are a floor nothing crosses;
/// - `min interval` is one that only a re-announce a person forced crosses,
///   as qBittorrent's "Force reannounce" does (`ignore_min_interval`);
/// - the first announce of a session is `started`, every later one carries
///   no event at all.
pub fn step(slot: &TrackerSlot, now: i64, tier_done: bool, paused: bool, forced: bool) -> Step {
    if slot.disabled {
        return Step::Skip(Why::Disabled);
    }
    if slot.completed_owed && slot.started {
        return Step::Send("completed");
    }
    if slot.stopped_owed {
        return Step::Send("stopped");
    }
    if paused {
        return Step::Skip(Why::Quiet);
    }
    if tier_done {
        return Step::Skip(Why::TierDone);
    }
    if now < slot.hint_until || (!forced && now < slot.not_before) {
        return Step::Skip(Why::Floor);
    }
    if !slot.started {
        return Step::Send("started");
    }
    Step::Send("")
}

/// Record what one announce to this tracker achieved.
pub fn record(slot: &mut TrackerSlot, event: &str, result: &Result<AnnounceResponse, String>, now: i64) {
    match result {
        Ok(resp) => {
            slot.last_ok = now;
            slot.refusals = 0;
            slot.not_before = now + resp.min_interval as i64;
            if let Some(id) = &resp.tracker_id {
                slot.tracker_id = Some(id.as_str().into());
            }
            match event {
                "started" => slot.started = true,
                "completed" => slot.completed_owed = false,
                "stopped" => {
                    slot.started = false;
                    slot.stopped_owed = false;
                    slot.completed_owed = false;
                    // The tracker id belongs to the session that just ended,
                    // and so does the floor: `min interval` paces a registered
                    // peer's re-announces, and we are no longer registered. A
                    // resume opens a new session at once, as libtorrent does.
                    slot.tracker_id = None;
                    slot.not_before = 0;
                }
                _ => {}
            }
        }
        Err(e) => {
            match retry_hint(e) {
                Some(RetryHint::After(d)) => slot.hint_until = now + d.as_secs() as i64,
                Some(RetryHint::Never) => slot.disabled = true,
                None => {}
            }
            if !slot.started && is_refusal(e) {
                slot.refusals = slot.refusals.saturating_add(1);
            }
            // A departure is attempted once, as every client does: a tracker
            // that is down when we leave times the entry out on its own, and
            // retrying a stop for a torrent that no longer runs would keep it
            // announcing indefinitely. `completed` stays owed and is retried:
            // it is the snatch, and nothing else records it.
            if event == "stopped" {
                slot.stopped_owed = false;
                slot.started = false;
                slot.completed_owed = false;
            }
        }
    }
}

/// The tracker answered, and the answer was no (`failure reason`) -- as
/// opposed to not answering at all. Before a torrent is registered this is
/// the "unregistered torrent" a race retries through.
pub fn is_refusal(err: &str) -> bool {
    err.contains("tracker: ")
}

/// How often a race retries a tracker that has not registered the torrent
/// yet, and for how long by default. Read from the code of the clients
/// trackers already accept: libtorrent 1.2 with `tracker_backoff = 0` retries
/// a refusing tracker every 5 s with no limit; autobrr retries qBittorrent
/// every 7 s for 50 attempts (~6 min). Hydranos takes the shorter interval and
/// the bounded window. Configurable per engine (`registration_retry_minutes`).
pub const REGISTRATION_RETRY: Duration = Duration::from_secs(5);
pub const REGISTRATION_WINDOW: Duration = Duration::from_secs(6 * 60);

/// How many refusals fit in a retry window.
pub fn registration_attempts(window: Duration) -> u8 {
    (window.as_secs() / REGISTRATION_RETRY.as_secs()).clamp(1, u8::MAX as u64) as u8
}

/// Whether a race should come back in seconds: some tracker is still refusing
/// a torrent it has not registered, it has not refused `max_attempts` times,
/// and no tracker has registered us (then peers arrive by themselves).
pub fn registration_pending(book: &[TrackerSlot], max_attempts: u8) -> bool {
    !book.iter().any(|s| s.started)
        && book
            .iter()
            .any(|s| !s.disabled && s.refusals > 0 && s.refusals < max_attempts)
}

/// Whether a skipped tracker still counts as "the tier has us" in hoard mode.
///
/// A tracker we are registered with and that asked for quiet is up: moving
/// to the next tier would register us twice. One that never answered is not
/// holding us, and the next tier is exactly what it is for.
pub fn holds_us(slot: &TrackerSlot, why: Why) -> bool {
    why == Why::Floor && slot.started
}

/// The earliest moment any live tracker of this torrent will accept an
/// announce, as a delay from `now`. Zero when one already does.
pub fn earliest_open(book: &[TrackerSlot], now: i64) -> Duration {
    book.iter()
        .filter(|s| !s.disabled)
        .map(|s| (s.not_before.max(s.hint_until) - now).max(0) as u64)
        .min()
        .map(Duration::from_secs)
        .unwrap_or(Duration::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(min_interval: u32, tracker_id: Option<&str>) -> Result<AnnounceResponse, String> {
        Ok(AnnounceResponse {
            interval: 1800,
            min_interval,
            peers: vec![],
            complete: 0,
            incomplete: 0,
            failure: None,
            tracker_id: tracker_id.map(String::from),
            warning: None,
        })
    }

    fn started() -> TrackerSlot {
        TrackerSlot { key: 1, started: true, last_ok: 1, ..Default::default() }
    }

    // --- the sequence of one session ---------------------------------------

    /// BEP 3: the first announce of a session to a tracker is `started`, and
    /// every later one carries no event.
    #[test]
    fn a_session_opens_with_started_and_continues_without_an_event() {
        let mut s = TrackerSlot::default();
        assert_eq!(step(&s, 100, false, false, false), Step::Send("started"));
        record(&mut s, "started", &ok(0, None), 100);
        assert_eq!(step(&s, 200, false, false, false), Step::Send(""));
    }

    /// A `started` the tracker never acknowledged is sent again, not assumed.
    #[test]
    fn an_unanswered_started_is_sent_again() {
        let mut s = TrackerSlot::default();
        record(&mut s, "started", &Err("http request: connect refused".into()), 100);
        assert_eq!(step(&s, 200, false, false, false), Step::Send("started"));
    }

    /// ⭐ `completed` goes to every tracker that saw us leeching -- not only
    /// the first one on the list.
    #[test]
    fn completed_is_owed_to_every_tracker_that_heard_started() {
        let mut book = vec![started(), TrackerSlot { key: 2, started: true, ..Default::default() }];
        file_owed(&mut book, ANNOUNCE_EVENT_COMPLETED, false);
        for s in &book {
            assert_eq!(step(s, 0, true, false, false), Step::Send("completed"), "tier or not");
        }
    }

    /// A tracker that never heard `started` is never told `completed` -- BEP 3
    /// forbids it for a download that was complete when the session began, and
    /// a tracker joining now joins a seed.
    #[test]
    fn a_tracker_that_never_heard_started_is_not_told_completed() {
        let mut book = vec![TrackerSlot { key: 3, ..Default::default() }];
        file_owed(&mut book, ANNOUNCE_EVENT_COMPLETED, false);
        assert_eq!(step(&book[0], 0, false, false, false), Step::Send("started"));
    }

    /// ⭐ A `completed` that meets a tracker that is down stays owed. It used
    /// to be taken before the send, and one failure lost the snatch for good.
    #[test]
    fn a_completed_that_fails_is_retried() {
        let mut s = started();
        s.completed_owed = true;
        record(&mut s, "completed", &Err("http request: timed out".into()), 10);
        assert_eq!(step(&s, 20, false, false, false), Step::Send("completed"));
        record(&mut s, "completed", &ok(0, None), 20);
        assert_eq!(step(&s, 30, false, false, false), Step::Send(""));
    }

    /// A stop is owed to every tracker that heard `started`, and only to them.
    #[test]
    fn stopped_is_owed_to_the_trackers_that_had_us() {
        let mut book = vec![started(), TrackerSlot { key: 9, ..Default::default() }];
        file_owed(&mut book, ANNOUNCE_EVENT_STOPPED, true);
        assert_eq!(step(&book[0], 0, true, true, false), Step::Send("stopped"));
        assert_eq!(step(&book[1], 0, false, true, false), Step::Skip(Why::Quiet), "never told started, nothing to take back");
    }

    /// Finished and stopped in one breath: the snatch first, then the
    /// departure. The stop used to overwrite the completion.
    #[test]
    fn completed_goes_out_before_stopped() {
        let mut s = started();
        let mut book = vec![s.clone()];
        file_owed(&mut book, ANNOUNCE_EVENT_COMPLETED | ANNOUNCE_EVENT_STOPPED, true);
        s = book.remove(0);
        assert_eq!(step(&s, 0, false, true, false), Step::Send("completed"));
        record(&mut s, "completed", &ok(0, None), 0);
        assert_eq!(step(&s, 0, false, true, false), Step::Send("stopped"));
        record(&mut s, "stopped", &ok(0, None), 0);
        assert_eq!(step(&s, 0, false, true, false), Step::Skip(Why::Quiet));
    }

    /// A departure is attempted once whatever the answer: retrying it would
    /// keep a stopped torrent announcing.
    #[test]
    fn a_failed_stopped_is_not_retried() {
        let mut s = started();
        s.stopped_owed = true;
        record(&mut s, "stopped", &Err("http request: timed out".into()), 0);
        assert_eq!(step(&s, 10, false, true, false), Step::Skip(Why::Quiet));
        assert!(!s.started);
    }

    /// A stop undone before the announcer ran owes nothing: the resume opened
    /// a new session already.
    #[test]
    fn a_stop_already_undone_owes_no_departure() {
        let mut book = vec![started()];
        file_owed(&mut book, ANNOUNCE_EVENT_STOPPED, false);
        assert!(!book[0].stopped_owed);
    }

    /// After a stop the next session starts with `started` again -- at once:
    /// the floor paced the session that ended, not the one that begins.
    #[test]
    fn after_a_stop_the_next_announce_is_started() {
        let mut s = started();
        s.stopped_owed = true;
        record(&mut s, "stopped", &ok(900, None), 0);
        assert_eq!(step(&s, 10, false, false, false), Step::Send("started"));
    }

    // --- floors ------------------------------------------------------------

    /// ⭐ `min interval` is a floor nothing crosses: not the periodic announce,
    /// not a bump, not a race.
    #[test]
    fn min_interval_is_a_hard_floor() {
        let mut s = TrackerSlot::default();
        record(&mut s, "started", &ok(300, None), 1000);
        assert_eq!(step(&s, 1299, false, false, false), Step::Skip(Why::Floor));
        assert_eq!(step(&s, 1300, false, false, false), Step::Send(""));
    }

    /// ⭐ A re-announce a person forces crosses `min interval`, as qBittorrent's
    /// does -- but never a floor the tracker asked for with `retry in` or
    /// `Retry-After`.
    #[test]
    fn a_forced_reannounce_crosses_min_interval_but_not_a_retry_hint() {
        let mut s = TrackerSlot::default();
        record(&mut s, "started", &ok(900, None), 1000);
        assert_eq!(step(&s, 1001, false, false, false), Step::Skip(Why::Floor));
        assert_eq!(step(&s, 1001, false, false, true), Step::Send(""), "forced: past min interval");
        record(&mut s, "", &Err("http 429 Too Many Requests: slow [retry-after 120s]".into()), 1001);
        assert_eq!(step(&s, 1002, false, false, true), Step::Skip(Why::Floor), "the tracker's own request holds");
        assert_eq!(step(&s, 1121, false, false, true), Step::Send(""));
    }

    /// A race retries a tracker that refuses an unregistered torrent, 50 times
    /// at most, and stops as soon as any tracker registers it.
    #[test]
    fn registration_retries_are_bounded_and_end_on_registration() {
        let refused = || Err::<AnnounceResponse, String>("tracker: Unregistered torrent".into());
        let max = registration_attempts(REGISTRATION_WINDOW);
        assert_eq!(max, 72, "six minutes at five seconds");
        let mut book = vec![TrackerSlot::default()];
        assert!(!registration_pending(&book, max), "nothing refused yet");
        record(&mut book[0], "started", &refused(), 0);
        assert!(registration_pending(&book, max));
        for i in 1..max as i64 {
            record(&mut book[0], "started", &refused(), i);
        }
        assert!(!registration_pending(&book, max), "the window is spent, then the ordinary schedule");

        let mut book = vec![TrackerSlot::default()];
        record(&mut book[0], "started", &refused(), 0);
        record(&mut book[0], "started", &ok(0, None), 7);
        assert!(!registration_pending(&book, max), "registered: the swarm finds us");
        assert_eq!(book[0].refusals, 0);
    }

    /// Not answering is not refusing: a tracker that is down is the breaker's
    /// business, not something to hammer every 7 seconds.
    #[test]
    fn a_tracker_that_does_not_answer_is_not_retried_in_seconds() {
        let mut book = vec![TrackerSlot::default()];
        record(&mut book[0], "started", &Err("http request: connect refused".into()), 0);
        assert!(!registration_pending(&book, registration_attempts(REGISTRATION_WINDOW)));
    }

    /// Events are exempt: `completed` and `stopped` are one-shot, and every
    /// client sends them when they happen.
    #[test]
    fn events_are_not_held_by_the_floor() {
        let mut s = started();
        s.not_before = 10_000;
        s.completed_owed = true;
        assert_eq!(step(&s, 0, false, false, false), Step::Send("completed"));
        s.completed_owed = false;
        s.stopped_owed = true;
        assert_eq!(step(&s, 0, false, true, false), Step::Send("stopped"));
    }

    /// BEP 31: `retry in` minutes sets the floor, `never` retires the tracker.
    #[test]
    fn bep31_retry_in_is_obeyed() {
        // (and a forced re-announce does not cross it either: see
        // a_forced_reannounce_crosses_min_interval_but_not_a_retry_hint)
        let mut s = TrackerSlot::default();
        record(&mut s, "started", &Err("tracker: busy [retry-in 5m]".into()), 1000);
        assert_eq!(step(&s, 1299, false, false, false), Step::Skip(Why::Floor));
        assert_eq!(step(&s, 1300, false, false, false), Step::Send("started"));
        record(&mut s, "started", &Err("tracker: banned client [retry-in never]".into()), 1300);
        assert_eq!(step(&s, 99_999, false, false, false), Step::Skip(Why::Disabled));
    }

    /// An HTTP `Retry-After` is the same floor, from the transport.
    #[test]
    fn retry_after_is_obeyed() {
        let mut s = started();
        record(&mut s, "", &Err("http 429 Too Many Requests: slow [retry-after 90s]".into()), 1000);
        assert_eq!(step(&s, 1089, false, false, false), Step::Skip(Why::Floor));
        assert_eq!(step(&s, 1090, false, false, false), Step::Send(""));
    }

    // --- tracker id and tiers ------------------------------------------------

    /// BEP 3: a `tracker id` is kept for the session and dropped with it.
    #[test]
    fn a_tracker_id_lives_as_long_as_the_session() {
        let mut s = TrackerSlot::default();
        record(&mut s, "started", &ok(0, Some("xyz")), 0);
        assert_eq!(s.tracker_id.as_deref(), Some("xyz"));
        record(&mut s, "", &ok(0, None), 10);
        assert_eq!(s.tracker_id.as_deref(), Some("xyz"), "an answer without one keeps the old one");
        s.stopped_owed = true;
        record(&mut s, "stopped", &ok(0, None), 20);
        assert_eq!(s.tracker_id, None);
    }

    /// Hoard: a tracker that has us and asked for quiet still holds the tier,
    /// so the next tier is not registered with twice. One that never answered
    /// does not, and failing over to the next tier is what tiers are for.
    #[test]
    fn a_quiet_tracker_holds_its_tier_and_a_dead_one_does_not() {
        assert!(holds_us(&started(), Why::Floor));
        assert!(!holds_us(&TrackerSlot::default(), Why::Floor));
        assert_eq!(step(&started(), 0, true, false, false), Step::Skip(Why::TierDone));
    }

    #[test]
    fn slots_are_found_again_by_url() {
        let mut book = Vec::new();
        slot_mut(&mut book, "https://a/announce").started = true;
        slot_mut(&mut book, "https://b/announce");
        assert!(slot_mut(&mut book, "https://a/announce").started);
        assert_eq!(book.len(), 2);
    }

    #[test]
    fn the_earliest_open_tracker_sets_the_wait() {
        let book = vec![
            TrackerSlot { key: 1, not_before: 500, ..Default::default() },
            TrackerSlot { key: 2, not_before: 200, ..Default::default() },
            TrackerSlot { key: 3, not_before: 50, disabled: true, ..Default::default() },
        ];
        assert_eq!(earliest_open(&book, 100), Duration::from_secs(100));
        assert_eq!(earliest_open(&book, 300), Duration::ZERO);
    }
}
