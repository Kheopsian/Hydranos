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
    /// The tracing target, `hydranos::announce::runner` style: what the
    /// module filter of the Logs tab matches on.
    pub module: String,
    pub msg: String,
    /// Position in the stream since the process started, never reused. The
    /// live tail resumes after the last one it sent; counting the ring's
    /// length instead went silent for good once the ring was full.
    pub seq: u64,
    /// Unix seconds, for the period filter. The published `ts` is text.
    #[serde(skip)]
    pub unix: i64,
}

/// Kept small on purpose: this is a tail, not an archive. The durable log is
/// the file; holding more here would cost memory on a node whose whole point is
/// to have less of it. A few hundred KB: enough for the "last hour" filter to
/// mean something on a quiet node.
const CAPACITY: usize = 2000;

#[derive(Default)]
struct Ring {
    entries: VecDeque<Entry>,
    next_seq: u64,
}

#[derive(Clone, Default)]
pub struct LogBuffer {
    ring: Arc<Mutex<Ring>>,
}

/// What a reader asks for. Every field is optional; an empty filter matches
/// every line.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Minimum level: `INFO` keeps INFO, WARN and ERROR.
    pub level: Option<u8>,
    /// Case-insensitive substring of the module path.
    pub module: Option<String>,
    /// Case-insensitive substring of the message, fields included.
    pub text: Option<String>,
    /// Lines logged at or after this unix second.
    pub since: Option<i64>,
    /// Lines after this sequence number.
    pub after: Option<u64>,
}

impl Filter {
    /// From a query string: `level`, `module`, `q` (or `contains`), `since`
    /// (`5m`, `1h`, `24h`, `2d`, or unix seconds) and `after` (a `seq`).
    /// A value it cannot read is an error, not a filter quietly dropped: an
    /// operator who typed `since=1week` must not get every line and believe
    /// it is the last week.
    pub fn from_query(query: &str, now: i64) -> Result<Self, String> {
        let mut f = Filter::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let v = crate::api::percent_decode(v);
            let v = v.trim();
            if v.is_empty() {
                continue;
            }
            match k {
                "level" => f.level = Some(level_rank(v).ok_or_else(|| format!("unknown level {v:?}"))?),
                "module" | "source" => {
                    // The 3.x tab sent a "source" of go/gin/engine:*; there is
                    // one process now, and every line is "rust". A module is
                    // what tells two lines apart.
                    if k == "source" && v.eq_ignore_ascii_case("rust") {
                        continue;
                    }
                    f.module = Some(v.to_lowercase())
                }
                "q" | "contains" => f.text = Some(v.to_lowercase()),
                "since" => f.since = Some(parse_since(v, now).ok_or_else(|| format!("unreadable since {v:?}"))?),
                "after" => f.after = Some(v.parse().map_err(|_| format!("unreadable after {v:?}"))?),
                _ => {}
            }
        }
        Ok(f)
    }

    pub fn matches(&self, e: &Entry) -> bool {
        if let Some(min) = self.level {
            if level_rank(&e.level).unwrap_or(0) < min {
                return false;
            }
        }
        if let Some(after) = self.after {
            if e.seq <= after {
                return false;
            }
        }
        if let Some(since) = self.since {
            if e.unix < since {
                return false;
            }
        }
        if let Some(m) = &self.module {
            if !e.module.to_lowercase().contains(m.as_str()) {
                return false;
            }
        }
        if let Some(t) = &self.text {
            if !e.msg.to_lowercase().contains(t.as_str()) {
                return false;
            }
        }
        true
    }
}

/// TRACE < DEBUG < INFO < WARN < ERROR.
pub fn level_rank(l: &str) -> Option<u8> {
    match l.to_ascii_uppercase().as_str() {
        "TRACE" => Some(0),
        "DEBUG" => Some(1),
        "INFO" => Some(2),
        "WARN" | "WARNING" => Some(3),
        "ERROR" => Some(4),
        _ => None,
    }
}

/// `5m`, `1h`, `24h`, `2d`, `90s`, or plain unix seconds.
fn parse_since(v: &str, now: i64) -> Option<i64> {
    if let Ok(n) = v.parse::<i64>() {
        return Some(n);
    }
    let (num, unit) = v.split_at(v.len().checked_sub(1)?);
    let n: i64 = num.parse().ok().filter(|n| *n >= 0)?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => return None,
    };
    Some(now - secs)
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            ring: Arc::new(Mutex::new(Ring {
                entries: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ring> {
        // A poisoned lock must not take the daemon down: losing a log line
        // is survivable, panicking inside the logger is not.
        self.ring.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Append a line. Its `seq` is assigned here, whatever the caller set.
    pub fn push(&self, mut entry: Entry) {
        let mut ring = self.lock();
        entry.seq = ring.next_seq;
        ring.next_seq += 1;
        if ring.entries.len() == CAPACITY {
            ring.entries.pop_front();
        }
        ring.entries.push_back(entry);
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<Entry> {
        self.lock().entries.iter().cloned().collect()
    }

    /// The newest `limit` lines that match, oldest first.
    pub fn query(&self, filter: &Filter, limit: usize) -> Vec<Entry> {
        let ring = self.lock();
        let mut out: Vec<Entry> = ring
            .entries
            .iter()
            .rev()
            .filter(|e| filter.matches(e))
            .take(limit)
            .cloned()
            .collect();
        out.reverse();
        out
    }

    /// The sequence number of the newest line, 0 before the first.
    pub fn last_seq(&self) -> u64 {
        self.lock().next_seq.saturating_sub(1)
    }

    /// The modules present in the ring, for the filter's list.
    pub fn modules(&self) -> Vec<String> {
        let ring = self.lock();
        let set: std::collections::BTreeSet<&str> = ring.entries.iter().map(|e| e.module.as_str()).collect();
        set.into_iter().map(str::to_string).collect()
    }
}

/// Pulls the `message` field out of an event, and the structured fields
/// after it as ` key=value`, the way the console prints them: `engines up`
/// alone does not say how many torrents.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push_str(&format!(" {}={}", field.name(), value));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
            // Debug formatting of a &str quotes it; the API publishes the text.
            if self.message.starts_with('"') && self.message.ends_with('"') && self.message.len() >= 2 {
                self.message = self.message[1..self.message.len() - 1].to_string();
            }
        } else {
            self.fields.push_str(&format!(" {}={:?}", field.name(), value));
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
        let (ts, unix) = now_rfc3339();
        self.buffer.push(Entry {
            ts,
            // "rust", where 3.x writes "go": the field says which half of the
            // daemon spoke, and in 4.0.0 there is only one half.
            source: "rust".into(),
            level: event.metadata().level().to_string(),
            module: event.metadata().target().to_string(),
            msg: visitor.message + &visitor.fields,
            seq: 0,
            unix,
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

/// RFC 3339 with nanoseconds, as Go's time.Time marshals it, and the unix
/// second it stands for.
fn now_rfc3339() -> (String, i64) {
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
    let ts = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        year,
        month,
        day,
        time_of_day / 3600,
        (time_of_day % 3600) / 60,
        time_of_day % 60,
        nanos
    );
    (ts, secs)
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

    fn line(level: &str, module: &str, msg: &str, unix: i64) -> Entry {
        Entry {
            ts: String::new(),
            source: "rust".into(),
            level: level.into(),
            module: module.into(),
            msg: msg.into(),
            seq: 0,
            unix,
        }
    }

    #[test]
    fn the_ring_drops_the_oldest_line_and_keeps_order() {
        let buffer = LogBuffer::new();
        for i in 0..CAPACITY + 10 {
            buffer.push(line("INFO", "m", &format!("line {i}"), 0));
        }
        let snap = buffer.snapshot();
        assert_eq!(snap.len(), CAPACITY, "the ring must stay bounded");
        assert_eq!(snap[0].msg, "line 10", "the oldest lines are the ones dropped");
        assert_eq!(snap[CAPACITY - 1].msg, format!("line {}", CAPACITY + 9));
    }

    /// The bug behind a silent Live: the tail counted the ring's length, and
    /// a full ring never grows. Sequence numbers keep moving past it.
    #[test]
    fn sequence_numbers_keep_moving_once_the_ring_is_full() {
        let buffer = LogBuffer::new();
        for i in 0..CAPACITY {
            buffer.push(line("INFO", "m", &format!("old {i}"), 0));
        }
        let cursor = buffer.last_seq();
        buffer.push(line("WARN", "m", "new", 0));
        let f = Filter { after: Some(cursor), ..Default::default() };
        let fresh = buffer.query(&f, 100);
        assert_eq!(fresh.len(), 1, "exactly the line logged after the cursor");
        assert_eq!(fresh[0].msg, "new");
        assert_eq!(fresh[0].seq, cursor + 1);
    }

    #[test]
    fn filters_apply_on_the_server() {
        let buffer = LogBuffer::new();
        buffer.push(line("DEBUG", "hydranos::store", "noise", 1_000));
        buffer.push(line("INFO", "hydranos::announce::runner", "tracker warning tracker=t.example", 1_000));
        buffer.push(line("WARN", "hydranos::announce::runner", "announce failed", 5_000));
        buffer.push(line("ERROR", "hydranos::store", "store held long", 5_000));

        let q = |s: &str| buffer.query(&Filter::from_query(s, 6_000).unwrap(), 100);
        assert_eq!(q("").len(), 4);
        assert_eq!(q("level=WARN").len(), 2, "minimum level");
        assert_eq!(q("level=error")[0].msg, "store held long");
        assert_eq!(q("module=announce").len(), 2);
        assert_eq!(q("q=TRACKER%20WARNING").len(), 1, "case-insensitive, URL-decoded");
        assert_eq!(q("q=t.example").len(), 1, "structured fields are searchable");
        assert_eq!(q("since=1h").len(), 2, "only what was logged in the last hour");
        assert_eq!(q("since=1h&level=ERROR&module=store").len(), 1, "filters combine");
        // The 3.x select sends source=rust for "everything".
        assert_eq!(q("source=rust").len(), 4);
    }

    #[test]
    fn an_unreadable_filter_is_refused_not_ignored() {
        assert!(Filter::from_query("since=1week", 0).is_err());
        assert!(Filter::from_query("level=LOUD", 0).is_err());
        assert!(Filter::from_query("after=x", 0).is_err());
    }

    #[test]
    fn the_newest_lines_are_kept_under_a_limit() {
        let buffer = LogBuffer::new();
        for i in 0..10 {
            buffer.push(line("INFO", "m", &format!("l{i}"), 0));
        }
        let got = buffer.query(&Filter::default(), 3);
        let msgs: Vec<&str> = got.iter().map(|e| e.msg.as_str()).collect();
        assert_eq!(msgs, ["l7", "l8", "l9"], "newest three, oldest first");
        assert_eq!(buffer.modules(), ["m"]);
    }

    #[test]
    fn timestamps_are_rfc3339_with_nanoseconds() {
        let (ts, unix) = now_rfc3339();
        assert_eq!(ts.len(), 30, "expected 2026-09-05T12:34:56.123456789Z: {ts}");
        assert!(ts.ends_with('Z') && ts.contains('T'), "{ts}");
        assert!(unix > 1_700_000_000);
    }

    // A known date, so a broken calendar conversion cannot pass unnoticed.
    #[test]
    fn the_epoch_converts_correctly() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }
}
