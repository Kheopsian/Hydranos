//! An in-memory ring of recent log lines, for the Logs tab and its SSE stream.
//!
//! 3.x serves the same thing from the Go side, and the endpoint is not
//! decoration: it is where an operator looks when a torrent will not start and
//! the UI says nothing useful. Answering an empty list would satisfy a parity
//! comparison that ignores the entries -- the exclusion exists because two
//! processes legitimately have different logs -- while quietly removing the
//! feature. So the ring is real.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer};

/// One line, in the shape the API publishes.
#[derive(Clone, serde::Serialize)]
pub struct Entry {
    pub ts: String,
    pub source: String,
    pub level: String,
    pub msg: String,
}

/// Kept small on purpose: this is a tail, not an archive. The durable log is
/// the file; holding more here would cost memory on a node whose whole point is
/// to have less of it.
const CAPACITY: usize = 500;

#[derive(Clone, Default)]
pub struct LogBuffer {
    entries: Arc<Mutex<VecDeque<Entry>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
        }
    }

    pub fn push(&self, entry: Entry) {
        let mut guard = match self.entries.lock() {
            Ok(g) => g,
            // A poisoned lock must not take the daemon down: losing a log line
            // is survivable, panicking inside the logger is not.
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.len() == CAPACITY {
            guard.pop_front();
        }
        guard.push_back(entry);
    }

    pub fn snapshot(&self) -> Vec<Entry> {
        match self.entries.lock() {
            Ok(g) => g.iter().cloned().collect(),
            Err(p) => p.into_inner().iter().cloned().collect(),
        }
    }
}

/// Pulls the `message` field out of an event.
#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            // Debug formatting of a &str quotes it; the API publishes the text.
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        }
    }
}

/// A tracing layer that copies every event into the ring.
pub struct LogLayer {
    pub buffer: LogBuffer,
}

impl<S: tracing::Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(Entry {
            ts: now_rfc3339(),
            // "rust", where 3.x writes "go": the field says which half of the
            // daemon spoke, and in 4.0.0 there is only one half.
            source: "rust".into(),
            level: event.metadata().level().to_string(),
            msg: visitor.message,
        });
    }
}

/// RFC 3339 to the second for a Unix timestamp, as Go marshals a time.Time
/// that carries no sub-second part.
///
/// The trackers tab renders this: an announce is timed to the second and a
/// nanosecond field there would only be noise.
pub(crate) fn rfc3339_at(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
    )
}

/// RFC 3339 with nanoseconds, as Go's time.Time marshals it.
fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let nanos = now.subsec_nanos();

    // Civil date from a Unix timestamp, without pulling in a date crate for one
    // format string.
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        year,
        month,
        day,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
        nanos
    )
}

/// Howard Hinnant's civil_from_days, the standard branch-free conversion.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_drops_the_oldest_line_and_keeps_order() {
        let buffer = LogBuffer::new();
        for i in 0..CAPACITY + 10 {
            buffer.push(Entry {
                ts: String::new(),
                source: "rust".into(),
                level: "INFO".into(),
                msg: format!("line {i}"),
            });
        }
        let snap = buffer.snapshot();
        assert_eq!(snap.len(), CAPACITY, "the ring must stay bounded");
        assert_eq!(snap[0].msg, "line 10", "the oldest lines are the ones dropped");
        assert_eq!(snap[CAPACITY - 1].msg, format!("line {}", CAPACITY + 9));
    }

    #[test]
    fn timestamps_are_rfc3339_with_nanoseconds() {
        let ts = now_rfc3339();
        assert_eq!(ts.len(), 30, "expected 2026-09-05T12:34:56.123456789Z: {ts}");
        assert!(ts.ends_with('Z') && ts.contains('T'), "{ts}");
    }

    // A known date, so a broken calendar conversion cannot pass unnoticed.
    #[test]
    fn the_epoch_converts_correctly() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }
}
