//! The link index, kept current by a thread of its own.
//!
//! A workflow on hardlinks used to stat the whole catalogue itself, on the
//! request that asked: at a million torrents that is some three million
//! `statx` on a pool serving torrents at the same time, and the dry-run button
//! sat for longer than anyone waits. Now this thread measures the catalogue at
//! its own pace and writes what it finds to the store (`link_index`); a pass
//! reads the store, and re-measures only the torrents it is about to act on.
//!
//! ⭐ It is a walk, not rounds. It used to take a round: read the whole index
//! (`link_index_meta`) and the whole catalogue, sweep what was due, let go.
//! Measured on 1.04 M torrents, each whole round was +2 to 3.5 GiB of resident
//! memory and one core at 100 % for one to five minutes, and the daily
//! refresh (`REFRESH_SECS`, ~12 torrents a second) made almost every round a
//! whole one. Now three small cursors share the thread, none of which holds
//! anything proportional to the catalogue but a list of 20-byte ids:
//!
//! - the **walk** goes through the torrents in hash order, `WALK_BATCH` at a
//!   time, reads only those torrents' rows, measures the ones that are not
//!   current and writes the packet; it pauses so that one cycle lasts about a
//!   day (`walk_pause`);
//! - the **arrivals** queue (filled where a torrent is added or moved, see
//!   `TorrentManager::take_arrivals`) is measured ahead of the walk, within
//!   seconds;
//! - the **orphan** walk pages through the index and drops the rows of
//!   torrents the engine no longer holds, one cycle an hour.
//!
//! ⭐ Two things fall out of keeping the measurement: a restart resumes with
//! what the last walk wrote instead of starting over, and a torrent whose
//! files are gone is found by this walk -- not only when a peer asks us for a
//! piece we cannot serve.
//!
//! ⚠️ A torrent with no valid row -- never measured, waiting in the arrivals
//! queue, being measured, or just dropped as an orphan -- reads `NEVER` in
//! every link fact of a pass (`links_from_measured` leaves it out of the
//! store's answer, `rules::cmp_num` answers false to NaN for every operator),
//! so no rule that deletes on `external_links == 0` or `link_count == 1` can
//! match it. Nothing here is read by a pass: it reads the table.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::engines::EngineHost;
use crate::rulesrun::{self, CatalogueEntry};
use crate::store::{LinkRow, LinkRowMeta, StoreLock};

/// A measurement older than this is taken again. Hardlinks appear when the
/// media library imports something, which is not a per-minute event; a day
/// keeps a million-torrent catalogue current at a few `statx` a second.
pub const REFRESH_SECS: i64 = 24 * 3600;

/// How much younger than `REFRESH_SECS` a measurement may be and still be
/// taken again when the walk comes by. The walk paces itself to a cycle of
/// `REFRESH_SECS`, give or take its bounds and the time it spends working: a
/// row written at the start of one cycle and met at the end of the next by a
/// walk a few minutes quicker would be skipped, and wait for a third cycle.
const WALK_SLACK_SECS: i64 = 2 * 3600;

/// Torrents the walk looks at per step. A packet is a hundred indexed lookups
/// and, at most, a hundred torrents' files to stat: a few milliseconds of the
/// store and a short stretch of the disks, whatever the size of the catalogue.
const WALK_BATCH: usize = 100;

/// Before the first measuring: the catalogue must be loaded, or the thread
/// would measure half of it.
const SETTLE: Duration = Duration::from_secs(180);

/// The least and the most a pause between two packets may be, by the clock
/// alone. Under the floor a small catalogue would be walked in a burst; over
/// the ceiling the arrivals queue and the stop flag would be looked at too
/// rarely (they have their own poll, but the ceiling bounds the walk's
/// sleeps).
const WALK_MIN_PAUSE: Duration = Duration::from_millis(250);
const WALK_MAX_PAUSE: Duration = Duration::from_secs(300);

/// The walk spends at most 1 / `WALK_DUTY` of the time working: after a step
/// that took `w` it pauses at least `(WALK_DUTY - 1) * w`, whatever the clock
/// says. The pool also serves torrents, and a step that finds cold metadata
/// (43 ms a `statx`, measured) is not a step to follow at once.
const WALK_DUTY: u32 = 20;

/// Between two looks at the arrivals queue. A torrent added is measured within
/// about this plus the time of a packet: well under the minute asked.
const ARRIVAL_POLL: Duration = Duration::from_secs(5);

/// Arrivals measured per look. A bulk add of thousands of torrents is taken
/// fifty at a time, and what the queue loses (it is bounded) the walk finds.
const ARRIVAL_BATCH: usize = 50;

/// Looks at an arrival the store does not know yet (the engine holds a torrent
/// a moment before its row is written) before it is left to the walk: five
/// minutes at `ARRIVAL_POLL`.
const ARRIVAL_TRIES: u32 = 60;

/// Rows of the index the orphan walk looks at per step.
const ORPHAN_PAGE: usize = 500;

/// How long the orphan walk takes to go through the index once. The rows of
/// removed torrents are harmless to a pass (facts are built from the catalogue,
/// never from the table), they only keep growing the table and its counts.
const ORPHAN_CYCLE: Duration = Duration::from_secs(3600);

/// Bounds of the pause between two orphan pages, as for the walk's.
const ORPHAN_MIN_PAUSE: Duration = Duration::from_millis(200);
const ORPHAN_MAX_PAUSE: Duration = Duration::from_secs(60);

/// Rows read per page by the first count, at start. A page is a few
/// milliseconds on the read connection, let go between two.
const COUNT_PAGE: usize = 5000;

/// When the store could not be read: the same step is tried again after this.
const RETRY: Duration = Duration::from_secs(30);

/// The list's summary and the catalogue's space on disk are published again at
/// the end of every walk cycle, and, when arrivals have been measured since,
/// at most this often. Both are a join over every name of the catalogue
/// (`refresh_summary`): ~30 s and 2 to 3 GiB at 1 M torrents, which is
/// acceptable only because it is rare. Without it a torrent added reads "-"
/// in the Hardlinks column until a workflow pass happens to publish.
const SUMMARY_EVERY: Duration = Duration::from_secs(30 * 60);

/// A measurement that found files missing is taken again once it is this old,
/// whenever the walk or a recheck meets it. A torrent is measured seconds
/// after it is added, often before its data has been downloaded or moved into
/// place; a row saying "missing" must not then stand for a day.
const MISSING_RETRY_SECS: i64 = 5 * 60;

/// How long after an arrival was measured with files missing it is measured
/// again, step by step: 5 minutes, then 30, then 2 hours, after which the
/// normal rhythm (the walk, once a cycle) takes over.
const RECHECK_BACKOFF: [Duration; 3] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 3600),
];

/// Rechecks kept. Past it the new ones are left to the walk, which takes a
/// row with files missing as due.
const RECHECK_CAP: usize = 4096;

/// Arrivals waiting for the store to know them, kept. Past it the oldest go.
const WAIT_CAP: usize = 1024;

/// The pause after a walk step that took `work`, for a catalogue of
/// `torrents`: the day divided over the packets, less the time the step took,
/// held between `WALK_MIN_PAUSE` and `WALK_MAX_PAUSE`; and never less than
/// what keeps the walk under 1 / `WALK_DUTY` of the clock.
///
/// At 1 M torrents that is 10 000 packets and 8.6 s between two: ~12 torrents
/// a second, the rate the daily refresh needs, and a cycle of a day. A
/// catalogue of a thousand would get a pause of hours; the ceiling makes its
/// cycle a few minutes, which costs a hundred lookups and measures nothing
/// that is not due.
pub fn walk_pause(torrents: usize, work: Duration) -> Duration {
    let packets = torrents.div_ceil(WALK_BATCH).max(1) as f64;
    let slot = Duration::from_secs_f64(REFRESH_SECS as f64 / packets);
    let paced = slot
        .saturating_sub(work)
        .clamp(WALK_MIN_PAUSE, WALK_MAX_PAUSE);
    paced.max(work.saturating_mul(WALK_DUTY - 1))
}

/// The pause between two orphan pages for an index of about `rows`.
fn orphan_pause(rows: usize) -> Duration {
    let pages = rows.div_ceil(ORPHAN_PAGE).max(1) as u32;
    (ORPHAN_CYCLE / pages).clamp(ORPHAN_MIN_PAUSE, ORPHAN_MAX_PAUSE)
}

/// What the index holds, counted: the figures the status line shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub measured: i64,
    pub files: i64,
    /// Torrents none of whose files could be read.
    pub data_missing: i64,
    /// Torrents some, but not all, of whose files could be read.
    pub partly_missing: i64,
    /// The oldest measurement; 0 when there is none.
    pub oldest: i64,
}

impl Tally {
    fn add(&mut self, m: &LinkRowMeta) {
        self.measured += 1;
        self.files += m.files;
        self.data_missing += (m.files > 0 && m.missing == m.files) as i64;
        self.partly_missing += (m.missing > 0 && m.missing < m.files) as i64;
        if self.oldest == 0 || m.measured_at < self.oldest {
            self.oldest = m.measured_at;
        }
    }
}

/// The published counters: kept by deltas (a row written or dropped is
/// subtracted as it was and added as it is) and set to a recount at the end of
/// every walk cycle, which wipes whatever the deltas got wrong.
///
/// ⚠️ The deltas are not exact, and the cycle's recount is what makes them
/// safe to use: `oldest` can only go down by delta (the row that was the oldest
/// being replaced leaves it too low until the recount), a row dropped as an
/// orphan that the last recount did not include is taken off once too often,
/// and a torrent added while a cycle runs is in the deltas but not in that
/// cycle's recount. None of it lasts longer than a cycle.
pub struct Counts {
    measured: AtomicI64,
    files: AtomicI64,
    data_missing: AtomicI64,
    partly_missing: AtomicI64,
    oldest: AtomicI64,
}

impl Counts {
    const fn new() -> Self {
        Counts {
            measured: AtomicI64::new(0),
            files: AtomicI64::new(0),
            data_missing: AtomicI64::new(0),
            partly_missing: AtomicI64::new(0),
            oldest: AtomicI64::new(0),
        }
    }

    pub fn get(&self) -> Tally {
        Tally {
            measured: self.measured.load(Ordering::Relaxed),
            files: self.files.load(Ordering::Relaxed),
            data_missing: self.data_missing.load(Ordering::Relaxed),
            partly_missing: self.partly_missing.load(Ordering::Relaxed),
            oldest: self.oldest.load(Ordering::Relaxed),
        }
    }

    fn set(&self, t: &Tally) {
        self.measured.store(t.measured, Ordering::Relaxed);
        self.files.store(t.files, Ordering::Relaxed);
        self.data_missing.store(t.data_missing, Ordering::Relaxed);
        self.partly_missing
            .store(t.partly_missing, Ordering::Relaxed);
        self.oldest.store(t.oldest, Ordering::Relaxed);
    }

    /// A row that was `old` is now `new` (either may be absent).
    fn replace(&self, old: Option<&LinkRowMeta>, new: Option<&LinkRowMeta>) {
        let mut d = Tally::default();
        for (m, sign) in [(old, -1), (new, 1)] {
            let Some(m) = m else { continue };
            d.measured += sign;
            d.files += sign * m.files;
            d.data_missing += sign * (m.files > 0 && m.missing == m.files) as i64;
            d.partly_missing += sign * (m.missing > 0 && m.missing < m.files) as i64;
        }
        for (field, by) in [
            (&self.measured, d.measured),
            (&self.files, d.files),
            (&self.data_missing, d.data_missing),
            (&self.partly_missing, d.partly_missing),
        ] {
            // A count below zero is a delta that outran its basis (see the
            // note above), never a fact.
            field.fetch_add(by, Ordering::Relaxed);
            field.fetch_max(0, Ordering::Relaxed);
        }
        if let Some(n) = new {
            let oldest = self.oldest.load(Ordering::Relaxed);
            if oldest == 0 || n.measured_at < oldest {
                self.oldest.store(n.measured_at, Ordering::Relaxed);
            }
        }
    }
}

/// Where the scanner is, for the status line. Plain atomics: written by one
/// thread, read by a request that only wants an approximate picture.
pub struct Progress {
    /// Torrents the engines held when the walk's cycle began.
    pub catalogue: AtomicI64,
    /// This cycle of the walk: torrents in it, torrents behind the cursor, and
    /// when it began.
    pub walk_total: AtomicI64,
    pub walk_done: AtomicI64,
    pub walk_started: AtomicI64,
    /// Unix time of the last packet written, and the files a second it took.
    pub last_batch_at: AtomicI64,
    pub files_per_sec: AtomicI64,
    /// Walk cycles finished since the start, and the last one's length.
    pub cycles: AtomicI64,
    pub last_cycle_secs: AtomicI64,
    /// Arrivals waiting to be measured.
    pub queued: AtomicI64,
    /// Orphan cycles finished since the start, and rows dropped by them.
    pub orphan_cycles: AtomicI64,
    pub orphans_dropped: AtomicI64,
    /// When the counters were last set whole (start, end of a cycle): 0 until
    /// the first count has been made.
    pub counted_at: AtomicI64,
    pub counts: Counts,
}

impl Progress {
    const fn new() -> Self {
        Progress {
            catalogue: AtomicI64::new(0),
            walk_total: AtomicI64::new(0),
            walk_done: AtomicI64::new(0),
            walk_started: AtomicI64::new(0),
            last_batch_at: AtomicI64::new(0),
            files_per_sec: AtomicI64::new(0),
            cycles: AtomicI64::new(0),
            last_cycle_secs: AtomicI64::new(0),
            queued: AtomicI64::new(0),
            orphan_cycles: AtomicI64::new(0),
            orphans_dropped: AtomicI64::new(0),
            counted_at: AtomicI64::new(0),
            counts: Counts::new(),
        }
    }
}

pub static PROGRESS: Progress = Progress::new();

type Key = (String, String);

/// A torrent in the walk's list: 20 bytes of hash and which engine holds it.
/// Hash first, so that sorting the list is sorting by info hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Id {
    hash: [u8; 20],
    engine: u8,
}

fn key(c: &CatalogueEntry) -> Key {
    (c.info_hash.clone(), c.session.to_string())
}

/// Whether a row describes the torrent as it is now: measured under its save
/// path, with its number of files. Anything else is as good as no row.
fn valid(m: &LinkRowMeta, c: &CatalogueEntry) -> bool {
    *m.save_path == *c.save_path && m.files == c.files as i64
}

/// Whether the walk measures this torrent when it comes by: no valid row, or
/// one old enough to be taken again.
fn due(row: Option<&LinkRowMeta>, c: &CatalogueEntry, now: i64) -> bool {
    match row {
        // Files missing: measured too early, perhaps (see `MISSING_RETRY_SECS`).
        Some(m) if valid(m, c) && m.missing > 0 => now - m.measured_at >= MISSING_RETRY_SECS,
        Some(m) if valid(m, c) => now - m.measured_at >= REFRESH_SECS - WALK_SLACK_SECS,
        _ => true,
    }
}

fn meta_of(r: &LinkRow) -> LinkRowMeta {
    LinkRowMeta {
        save_path: r.save_path.clone(),
        measured_at: r.measured_at,
        files: r.files,
        missing: r.missing,
    }
}

/// From here on, a torrent that is added or moved is queued for the scanner.
/// Not before: the restore at start goes through the same door. Called before
/// the watched folders and the RSS feeds start adding, so that what they bring
/// in the first minutes is not left to the slow walk.
pub fn arm(engines: &EngineHost) {
    for e in engines.engines() {
        e.manager.arm_arrivals();
    }
}

pub fn spawn(engines: Arc<EngineHost>, store: Arc<StoreLock>) {
    arm(&engines);
    let spawned = std::thread::Builder::new()
        .name("link-scan".into())
        .spawn(move || run(engines, store));
    if let Err(e) = spawned {
        tracing::warn!("no link scanner, hardlink rules will match nothing: {e}");
    }
}

/// Recompute the hoard list's hardlink summary from the store, and publish it.
///
/// ⭐ Through `rulesrun::links_from_store`, the function a workflow pass reads
/// its `external_links` from: same rows, same catalogue resolution, same
/// count. The column and the condition can only differ by WHEN they were
/// computed, never by HOW. Returns how many torrents it answers for.
pub fn refresh_summary(engines: &EngineHost, store: &StoreLock) -> Option<usize> {
    // The read connection, for the reads only; released before the counting.
    let known = rulesrun::stored_copies(engines, store);
    let cat = rulesrun::catalogue_from(engines, &known);
    drop(known);
    let measured = rulesrun::read_measured(&cat, store).ok()?;
    let got = rulesrun::links_from_measured(&cat, measured);
    let summary = rulesrun::link_summary(&cat, &got.origin, &got.measured_at, &got.links);
    let n = summary.len();
    engines.publish_link_summary(summary);
    // Here and not in `links_from_store`: a workflow pass reads that one too,
    // and has no use for a second walk over every file.
    engines.publish_catalogue_usage(
        crate::linkindex::usage(&got.entries),
        crate::store::now_secs(),
    );
    Some(n)
}

/// Everything the scanner asks of the outside world, so that a test can count
/// what each step reads. Nothing here returns the whole catalogue or the whole
/// index: the largest answer is a packet or a page.
trait Io {
    /// The ids of every torrent the engines hold. Taken once a cycle: 20 bytes
    /// a torrent, ~24 MB at 1 M, and no allocation per torrent.
    fn ids(&self) -> Vec<Id>;
    /// Up to `max` torrents added or moved since the last call.
    fn arrivals(&self, max: usize) -> Vec<Id>;
    /// The ones of `ids` the engine still holds and the store knows, as the
    /// catalogue would list them. A copy the store does not know is not part of
    /// any pass.
    fn entries(&self, ids: &[Id]) -> Vec<(Id, CatalogueEntry)>;
    /// The store's rows for these copies, and no others. `None`: unreadable.
    fn rows(&self, entries: &[&CatalogueEntry]) -> Option<HashMap<Key, LinkRowMeta>>;
    /// A page of the index after `after`, in key order. `None`: unreadable.
    fn meta_page(&self, after: &Key, limit: usize) -> Option<Vec<(Key, LinkRowMeta)>>;
    /// Whether the engine of `session` holds this torrent.
    fn holds(&self, session: &str, info_hash: &str) -> bool;
    /// Whether the rows were dropped.
    fn drop_rows(&self, keys: &[Key]) -> bool;
    /// `None`: the store is gone, stop. `Some(false)`: the batch was not
    /// written.
    fn put_rows(&self, rows: &[LinkRow]) -> Option<bool>;
    /// Whether the arrivals queue dropped some since the last call.
    fn overflowed(&self) -> bool;
    /// The ids that have a row in the index, read by pages and returned as a
    /// list of 21 bytes an id: ~21 MB at 1 M, let go by the caller after use.
    /// `None`: unreadable.
    fn measured_ids(&self) -> Option<Vec<Id>>;
    /// Publish the list's summary and the catalogue's space on disk.
    fn publish_summary(&self);
}

/// Engines an `Id` can name.
const MAX_ENGINES: usize = 256;

struct Live<'a> {
    engines: &'a EngineHost,
    store: &'a StoreLock,
}

impl Io for Live<'_> {
    fn ids(&self) -> Vec<Id> {
        let mut out =
            Vec::with_capacity(self.engines.engines().iter().map(|e| e.manager.len()).sum());
        for (i, e) in self.engines.engines().iter().enumerate().take(MAX_ENGINES) {
            e.manager.for_each(|t| {
                out.push(Id {
                    hash: t.info_hash,
                    engine: i as u8,
                })
            });
        }
        out
    }

    fn arrivals(&self, max: usize) -> Vec<Id> {
        let mut out = Vec::new();
        for (i, e) in self.engines.engines().iter().enumerate().take(MAX_ENGINES) {
            let room = max.saturating_sub(out.len());
            if room == 0 {
                break;
            }
            out.extend(e.manager.take_arrivals(room).into_iter().map(|hash| Id {
                hash,
                engine: i as u8,
            }));
        }
        out
    }

    fn entries(&self, ids: &[Id]) -> Vec<(Id, CatalogueEntry)> {
        let mut out = Vec::with_capacity(ids.len());
        for (i, e) in self.engines.engines().iter().enumerate().take(MAX_ENGINES) {
            let session: Arc<str> = e.id.as_str().into();
            let found: Vec<(Id, CatalogueEntry)> = ids
                .iter()
                .filter(|id| id.engine as usize == i)
                .filter_map(|id| {
                    let t = e.manager.get(&id.hash)?;
                    Some((*id, rulesrun::entry_for(&session, &t)?))
                })
                .collect();
            if found.is_empty() {
                continue;
            }
            let hashes: Vec<&str> = found.iter().map(|(_, c)| c.info_hash.as_str()).collect();
            let Ok(held) = self.store.held_among(&e.id, &hashes) else {
                continue;
            };
            out.extend(
                found
                    .into_iter()
                    .filter(|(_, c)| held.contains(&c.info_hash)),
            );
        }
        out
    }

    fn rows(&self, entries: &[&CatalogueEntry]) -> Option<HashMap<Key, LinkRowMeta>> {
        let mut by_session: HashMap<&str, Vec<&str>> = HashMap::new();
        for c in entries {
            by_session.entry(&c.session).or_default().push(&c.info_hash);
        }
        let mut out = HashMap::new();
        for (session, hashes) in by_session {
            for (h, m) in self.store.link_meta_among(session, &hashes).ok()? {
                out.insert((h, session.to_string()), m);
            }
        }
        Some(out)
    }

    fn meta_page(&self, after: &Key, limit: usize) -> Option<Vec<(Key, LinkRowMeta)>> {
        self.store.link_meta_page(after, limit as i64).ok()
    }

    fn holds(&self, session: &str, info_hash: &str) -> bool {
        let (Some(e), Some(raw)) = (self.engines.get(session), crate::store::hex20(info_hash))
        else {
            return false;
        };
        e.manager.has(&raw)
    }

    fn drop_rows(&self, keys: &[Key]) -> bool {
        let Ok(s) = self.store.lock() else {
            return false;
        };
        match s.drop_link_rows(keys) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("link index: could not drop removed torrents: {e}");
                false
            }
        }
    }

    fn overflowed(&self) -> bool {
        self.engines
            .engines()
            .iter()
            .fold(false, |any, e| e.manager.take_arrivals_overflow() | any)
    }

    fn measured_ids(&self) -> Option<Vec<Id>> {
        let engines = self.engines.engines();
        let mut out = Vec::new();
        let mut after = Key::default();
        loop {
            if crate::shutdown::stopping() {
                return None;
            }
            let page = self.meta_page(&after, COUNT_PAGE)?;
            for ((hash, session), _) in &page {
                let (Some(raw), Some(engine)) = (
                    crate::store::hex20(hash),
                    engines.iter().position(|e| e.id == *session),
                ) else {
                    continue;
                };
                if engine < MAX_ENGINES {
                    out.push(Id {
                        hash: raw,
                        engine: engine as u8,
                    });
                }
            }
            match page.last() {
                Some((k, _)) if page.len() >= COUNT_PAGE => after = k.clone(),
                _ => return Some(out),
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn publish_summary(&self) {
        refresh_summary(self.engines, self.store);
    }

    fn put_rows(&self, rows: &[LinkRow]) -> Option<bool> {
        match self.store.lock() {
            Ok(s) => Some(match s.put_link_rows(rows) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("link index: batch not written: {e}");
                    false
                }
            }),
            Err(_) => None,
        }
    }
}

/// What looking at a packet came to.
#[derive(Default)]
struct Looked {
    /// Of the ids, those the engine holds and the store knows.
    resolved: HashSet<Id>,
    measured: usize,
    files: usize,
    /// Of those measured now, the ones with files missing.
    partial: Vec<Id>,
}

/// Look at `ids`: read their rows and no others, measure those that are not
/// current, write them, bring the counters along. `tally` receives the rows
/// that stand afterwards, for the walk's recount. `None`: the store could not
/// be read, nothing was measured.
fn look(io: &dyn Io, progress: &Progress, ids: &[Id], tally: Option<&mut Tally>) -> Option<Looked> {
    let found = io.entries(ids);
    let entries: Vec<&CatalogueEntry> = found.iter().map(|(_, c)| c).collect();
    let rows = io.rows(&entries)?;
    let now = crate::store::now_secs();

    let todo: Vec<&CatalogueEntry> = entries
        .iter()
        .copied()
        .filter(|c| due(rows.get(&key(c)), c, now))
        .collect();
    // Paths for this packet only, dropped with it. A torrent removed since the
    // id was listed has none, and is not measured.
    let (live, plan): (Vec<&CatalogueEntry>, Vec<_>) = todo
        .iter()
        .filter_map(|&c| Some((c, (c.info_hash.clone(), c.paths()?))))
        .unzip();
    let measured = if plan.is_empty() {
        Vec::new()
    } else {
        rulesrun::stat_plan_with(plan, rulesrun::scan_threads())
    };
    let mut files = 0usize;
    let new_rows: Vec<LinkRow> = live
        .iter()
        .zip(&measured)
        .map(|(&c, (_, fs))| {
            let stats: Vec<_> = fs.iter().map(|(_, st)| *st).collect();
            files += stats.len();
            rulesrun::link_row(c, &stats, now)
        })
        .collect();
    let written: HashMap<Key, LinkRowMeta> = if new_rows.is_empty() {
        HashMap::new()
    } else {
        match io.put_rows(&new_rows) {
            Some(true) => new_rows
                .iter()
                .map(|r| ((r.info_hash.clone(), r.session.clone()), meta_of(r)))
                .collect(),
            Some(false) => HashMap::new(),
            None => return None,
        }
    };
    for (k, m) in &written {
        progress.counts.replace(rows.get(k), Some(m));
    }
    if let Some(t) = tally {
        for c in &entries {
            let k = key(c);
            if let Some(m) = written.get(&k).or_else(|| rows.get(&k)) {
                t.add(m);
            }
        }
    }
    Some(Looked {
        resolved: found.iter().map(|(id, _)| *id).collect(),
        measured: written.len(),
        files,
        partial: found
            .iter()
            .filter(|(_, c)| written.get(&key(c)).is_some_and(|m| m.missing > 0))
            .map(|(id, _)| *id)
            .collect(),
    })
}

/// A cycle of the walk in progress.
struct Cycle {
    started: Instant,
    tally: Tally,
    measured: usize,
    files: usize,
}

/// What one step of the walk did.
struct Walked {
    /// Torrents in the cycle's list, for the pause.
    torrents: usize,
    /// Read by the tests, which have no clock to judge a step by.
    #[cfg_attr(not(test), allow(dead_code))]
    measured: usize,
    work: Duration,
}

#[derive(Default)]
struct OrphanCycle {
    started: Option<Instant>,
    rows: usize,
    dropped: usize,
}

/// What the scanner carries from one step to the next: the cursors, and
/// nothing about the catalogue but the list of ids the walk is going through.
struct Scanner {
    progress: &'static Progress,
    ids: Vec<Id>,
    pos: usize,
    cycle: Option<Cycle>,
    /// Arrivals the store did not know yet, with how many looks they had.
    waiting: Vec<(Id, u32)>,
    /// Arrivals measured with files missing, to be measured again: which
    /// backoff step they are at, and when.
    rechecks: Vec<(Id, usize, Instant)>,
    /// Whether the next list of ids puts the torrents with no row in the
    /// index first. True at start, and again when the arrivals queue dropped
    /// some: the walk's order is by hash, and a torrent added during a stop or
    /// lost by the queue would wait for its turn, up to a day.
    want_priority: bool,
    /// Arrivals have been measured since the summary was last published.
    summary_dirty: bool,
    published_at: Instant,
    orphan_after: Key,
    orphan: OrphanCycle,
}

impl Scanner {
    fn new(progress: &'static Progress) -> Self {
        Scanner {
            progress,
            ids: Vec::new(),
            pos: 0,
            cycle: None,
            waiting: Vec::new(),
            rechecks: Vec::new(),
            want_priority: true,
            summary_dirty: false,
            published_at: Instant::now(),
            orphan_after: Key::default(),
            orphan: OrphanCycle::default(),
        }
    }

    /// Count the index by pages, at start: the figures the status line shows
    /// are right as soon as this ends, and the table is never held. Returns
    /// whether it could be read.
    fn count_table(&self, io: &dyn Io) -> bool {
        let mut t = Tally::default();
        let mut after = Key::default();
        loop {
            if crate::shutdown::stopping() {
                return false;
            }
            let Some(page) = io.meta_page(&after, COUNT_PAGE) else {
                return false;
            };
            page.iter().for_each(|(_, m)| t.add(m));
            match page.last() {
                Some((k, _)) if page.len() >= COUNT_PAGE => after = k.clone(),
                _ => break,
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.progress.counts.set(&t);
        self.progress
            .counted_at
            .store(crate::store::now_secs(), Ordering::Relaxed);
        true
    }

    /// Measure the torrents that were added or moved, ahead of the walk. Fifty
    /// at most per look: up to half of them retries of arrivals the store did
    /// not know yet (the rest keep their place and cannot hold up new ones),
    /// the others new. Returns how many it measured.
    fn arrivals_step(&mut self, io: &dyn Io, now: Instant) -> usize {
        if io.overflowed() {
            // Some arrivals were lost: start the walk's list again, with the
            // torrents that have no row first (`walk_step`).
            self.want_priority = true;
            self.cycle = None;
        }
        let retry = self.waiting.len().min(ARRIVAL_BATCH / 2);
        let retried: Vec<(Id, u32)> = self.waiting.drain(..retry).collect();
        let mut ids: Vec<Id> = retried.iter().map(|(id, _)| *id).collect();
        ids.extend(io.arrivals(ARRIVAL_BATCH - retried.len()));
        ids.sort_unstable();
        ids.dedup();
        let tries: HashMap<Id, u32> = retried.into_iter().collect();
        let looked = look(io, self.progress, &ids, None);
        // Resolved ones are done. The rest wait for their row in the store (or
        // for the engine to let go of them), a while, then are left to the walk.
        for id in &ids {
            if looked.as_ref().is_some_and(|l| l.resolved.contains(id)) {
                continue;
            }
            let n = tries.get(id).copied().unwrap_or(0) + 1;
            if n < ARRIVAL_TRIES {
                self.waiting.push((*id, n));
            }
        }
        if self.waiting.len() > WAIT_CAP {
            self.waiting.drain(..self.waiting.len() - WAIT_CAP);
        }
        self.progress
            .queued
            .store(self.waiting.len() as i64, Ordering::Relaxed);
        let Some(l) = looked else { return 0 };
        for id in l.partial {
            self.schedule_recheck(id, 0, now);
        }
        self.summary_dirty |= l.measured > 0;
        l.measured
    }

    fn schedule_recheck(&mut self, id: Id, step: usize, now: Instant) {
        if step < RECHECK_BACKOFF.len() && self.rechecks.len() < RECHECK_CAP {
            self.rechecks.push((id, step, now + RECHECK_BACKOFF[step]));
        }
    }

    /// Measure again the arrivals that were measured with files missing and
    /// whose time has come; one that still has files missing goes on to the
    /// next, longer, backoff. Returns how many it measured.
    fn recheck_step(&mut self, io: &dyn Io, now: Instant) -> usize {
        let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.rechecks)
            .into_iter()
            .partition(|(_, _, at)| *at <= now);
        let take = due.len().min(ARRIVAL_BATCH);
        let mut due = due;
        let rest = due.split_off(take);
        self.rechecks = later;
        self.rechecks.extend(rest);
        if due.is_empty() {
            return 0;
        }
        let ids: Vec<Id> = due.iter().map(|(id, _, _)| *id).collect();
        let Some(l) = look(io, self.progress, &ids, None) else {
            self.rechecks.extend(due);
            return 0;
        };
        for (id, step, _) in due {
            if l.partial.contains(&id) {
                self.schedule_recheck(id, step + 1, now);
            }
        }
        self.summary_dirty |= l.measured > 0;
        l.measured
    }

    /// Publish the summary again if arrivals have been measured since the last
    /// time and `SUMMARY_EVERY` has gone by.
    fn summary_step(&mut self, io: &dyn Io, now: Instant) -> bool {
        if !self.summary_dirty || now.saturating_duration_since(self.published_at) < SUMMARY_EVERY {
            return false;
        }
        io.publish_summary();
        self.published_at = now;
        self.summary_dirty = false;
        true
    }

    /// One packet of the walk. `None` when the store could not be read: the
    /// cursor stays where it is.
    fn walk_step(&mut self, io: &dyn Io) -> Option<Walked> {
        let t0 = Instant::now();
        let p = self.progress;
        if self.cycle.is_none() {
            let mut ids = io.ids();
            ids.sort_unstable();
            if self.want_priority {
                // The torrents with no row at all first, each group in hash
                // order. The set of measured ids is dropped with this block.
                if let Some(mut measured) = io.measured_ids() {
                    measured.sort_unstable();
                    ids.sort_by_key(|id| measured.binary_search(id).is_ok());
                    self.want_priority = false;
                }
            }
            p.catalogue.store(ids.len() as i64, Ordering::Relaxed);
            p.walk_total.store(ids.len() as i64, Ordering::Relaxed);
            p.walk_done.store(0, Ordering::Relaxed);
            p.walk_started
                .store(crate::store::now_secs(), Ordering::Relaxed);
            self.ids = ids;
            self.pos = 0;
            self.cycle = Some(Cycle {
                started: Instant::now(),
                tally: Tally::default(),
                measured: 0,
                files: 0,
            });
        }
        let torrents = self.ids.len();
        let end = (self.pos + WALK_BATCH).min(torrents);
        let packet = self.ids[self.pos..end].to_vec();
        let cycle = self.cycle.as_mut()?;
        let looked = look(io, p, &packet, Some(&mut cycle.tally))?;
        cycle.measured += looked.measured;
        cycle.files += looked.files;
        self.pos = end;
        p.walk_done.store(end as i64, Ordering::Relaxed);
        let work = t0.elapsed();
        if looked.measured > 0 {
            p.last_batch_at
                .store(crate::store::now_secs(), Ordering::Relaxed);
            p.files_per_sec.store(
                (looked.files as f64 / work.as_secs_f64().max(0.001)) as i64,
                Ordering::Relaxed,
            );
        }
        if end >= torrents {
            self.end_cycle(io);
        }
        Some(Walked {
            torrents,
            measured: looked.measured,
            work,
        })
    }

    /// The walk has been through every torrent of its list: what it counted on
    /// the way replaces the counters, and the list is let go.
    fn end_cycle(&mut self, io: &dyn Io) {
        let Some(c) = self.cycle.take() else { return };
        let p = self.progress;
        p.counts.set(&c.tally);
        p.counted_at
            .store(crate::store::now_secs(), Ordering::Relaxed);
        p.cycles.fetch_add(1, Ordering::Relaxed);
        let secs = c.started.elapsed().as_secs();
        p.last_cycle_secs.store(secs as i64, Ordering::Relaxed);
        tracing::info!(
            torrents = self.ids.len(),
            measured = c.measured,
            files = c.files,
            rows = c.tally.measured,
            data_missing = c.tally.data_missing,
            partly_missing = c.tally.partly_missing,
            secs,
            "link index measure cycle complete"
        );
        self.ids = Vec::new();
        self.pos = 0;
        // The cycle's rows are all new to the summary: publish it again.
        io.publish_summary();
        self.published_at = Instant::now();
        self.summary_dirty = false;
    }

    /// One page of the orphan walk: the rows of torrents the engine of their
    /// session no longer holds are dropped. Returns the rows it looked at, or
    /// `None` when the store could not be read.
    fn orphan_step(&mut self, io: &dyn Io) -> Option<usize> {
        let p = self.progress;
        let page = io.meta_page(&self.orphan_after, ORPHAN_PAGE)?;
        let started = *self.orphan.started.get_or_insert_with(Instant::now);
        let gone: Vec<&(Key, LinkRowMeta)> =
            page.iter().filter(|((h, s), _)| !io.holds(s, h)).collect();
        if !gone.is_empty() {
            let keys: Vec<Key> = gone.iter().map(|(k, _)| k.clone()).collect();
            if io.drop_rows(&keys) {
                for (_, m) in &gone {
                    p.counts.replace(Some(m), None);
                }
                self.orphan.dropped += gone.len();
            }
        }
        self.orphan.rows += page.len();
        match page.last() {
            Some((k, _)) if page.len() >= ORPHAN_PAGE => self.orphan_after = k.clone(),
            _ => {
                p.orphan_cycles.fetch_add(1, Ordering::Relaxed);
                p.orphans_dropped
                    .fetch_add(self.orphan.dropped as i64, Ordering::Relaxed);
                tracing::info!(
                    rows = self.orphan.rows,
                    dropped = self.orphan.dropped,
                    secs = started.elapsed().as_secs(),
                    "link index orphan cycle complete"
                );
                self.orphan = OrphanCycle::default();
                self.orphan_after = Key::default();
            }
        }
        Some(page.len())
    }
}

fn run(engines: Arc<EngineHost>, store: Arc<StoreLock>) {
    // What the store holds from before the restart is published NOW, not after
    // the settle: it is a read, and the catalogue is loaded by the time this
    // thread is spawned. Waiting left the Hardlinks and Checked columns at "-"
    // for every torrent for four minutes after each start (rc32, 2026-10-03).
    // Only the measuring waits for the disks to settle.
    //
    // ⚠️ The one whole read left, once, at start: the list's per-torrent
    // `external_links` is a join over every name of the catalogue (a name held
    // by another torrent of ours is not external), which no packet can answer.
    // After it, the list is brought up to date by the workflow passes that
    // already compute it (`rulesapi::decide`), not by this thread.
    refresh_summary(&engines, &store);
    let io = Live {
        engines: &engines,
        store: &store,
    };
    let mut sc = Scanner::new(&PROGRESS);
    // The counters are right as soon as this is done, a page at a time.
    sc.count_table(&io);
    // In seconds, so that a stop is not waited on for the length of the settle.
    for _ in 0..SETTLE.as_secs() {
        if crate::shutdown::stopping() {
            return;
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let (mut next_arrivals, mut next_walk, mut next_orphans) =
        (Instant::now(), Instant::now(), Instant::now());
    loop {
        // Between two steps: a packet that has started is written whole.
        if crate::shutdown::stopping() {
            return;
        }
        let now = Instant::now();
        if now >= next_arrivals {
            sc.arrivals_step(&io, now);
            sc.recheck_step(&io, now);
            sc.summary_step(&io, now);
            next_arrivals = Instant::now() + ARRIVAL_POLL;
        }
        if now >= next_walk {
            next_walk = Instant::now()
                + match sc.walk_step(&io) {
                    Some(w) => walk_pause(w.torrents, w.work),
                    None => RETRY,
                };
        }
        if now >= next_orphans {
            let rows = PROGRESS.counts.get().measured.max(0) as usize;
            next_orphans = Instant::now()
                + match sc.orphan_step(&io) {
                    Some(_) => orphan_pause(rows),
                    None => RETRY,
                };
        }
        if crate::shutdown::stopping() {
            return;
        }
        let next = next_arrivals.min(next_walk).min(next_orphans);
        std::thread::sleep(
            next.saturating_duration_since(Instant::now())
                .clamp(Duration::from_millis(10), ARRIVAL_POLL),
        );
    }
}

/// The scanner over a live engine host and store, one step of the arrivals
/// queue: for the tests of the workflows' side, which have an engine and no
/// thread.
#[cfg(test)]
pub(crate) fn measure_arrivals_once(engines: &EngineHost, store: &StoreLock) -> usize {
    let io = Live { engines, store };
    let progress: &'static Progress = Box::leak(Box::new(Progress::new()));
    Scanner::new(progress).arrivals_step(&io, Instant::now())
}

/// One packet of the walk, from a scanner that has just started: how many
/// torrents it measured.
#[cfg(test)]
pub(crate) fn walk_once(engines: &EngineHost, store: &StoreLock) -> usize {
    let io = Live { engines, store };
    let progress: &'static Progress = Box::leak(Box::new(Progress::new()));
    let mut sc = Scanner::new(progress);
    sc.walk_step(&io).expect("the store reads").measured
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, VecDeque};

    fn id_of(hash: &str) -> Id {
        let mut h = [0u8; 20];
        h[..hash.len()].copy_from_slice(hash.as_bytes());
        Id { hash: h, engine: 0 }
    }

    fn name(n: usize) -> String {
        format!("t{n:05}")
    }

    fn c(hash: &str, save_path: &str, files: usize) -> CatalogueEntry {
        CatalogueEntry::fixed(
            hash,
            "hoard",
            save_path,
            (0..files)
                .map(|i| std::path::PathBuf::from(format!("{save_path}/{i}")))
                .collect(),
        )
    }

    fn m(save_path: &str, measured_at: i64, files: i64, missing: i64) -> LinkRowMeta {
        LinkRowMeta {
            save_path: save_path.into(),
            measured_at,
            files,
            missing,
        }
    }

    fn k(hash: &str) -> Key {
        (hash.to_string(), "hoard".to_string())
    }

    /// Its own counters: the statics are shared by every test of the process.
    fn progress() -> &'static Progress {
        Box::leak(Box::new(Progress::new()))
    }

    /// The engines and the store, in memory, counting what each step reads.
    #[derive(Default)]
    struct Fake {
        held: RefCell<Vec<CatalogueEntry>>,
        /// Held by the engine but not (yet) known to the store.
        hidden: RefCell<HashSet<String>>,
        table: RefCell<BTreeMap<Key, LinkRowMeta>>,
        queue: RefCell<VecDeque<Id>>,
        /// Calls to `ids`: the list is taken once a cycle.
        id_calls: Cell<usize>,
        /// The most torrents asked about, or rows answered, in one call.
        widest: Cell<usize>,
        /// The most rows of the index answered in one page.
        widest_page: Cell<usize>,
        puts: Cell<usize>,
        overflow: Cell<bool>,
        published: Cell<usize>,
    }

    impl Fake {
        fn new(n: usize) -> Self {
            let f = Fake::default();
            for i in 0..n {
                f.held.borrow_mut().push(c(&name(i), "/d", 1));
            }
            f
        }

        fn widen(&self, n: usize) {
            self.widest.set(self.widest.get().max(n));
        }

        fn add(&self, hash: &str) {
            self.held.borrow_mut().push(c(hash, "/d", 1));
            self.queue.borrow_mut().push_back(id_of(hash));
        }

        fn remove(&self, hash: &str) {
            self.held.borrow_mut().retain(|e| e.info_hash != hash);
        }

        fn measured(&self, hash: &str) -> bool {
            self.table.borrow().contains_key(&k(hash))
        }

        fn at(&self, hash: &str) -> i64 {
            self.table.borrow()[&k(hash)].measured_at
        }

        /// What the counters must read, counted off the table.
        fn recount(&self) -> Tally {
            let mut t = Tally::default();
            self.table.borrow().values().for_each(|m| t.add(m));
            t
        }
    }

    impl Io for Fake {
        fn ids(&self) -> Vec<Id> {
            self.id_calls.set(self.id_calls.get() + 1);
            self.held
                .borrow()
                .iter()
                .map(|e| id_of(&e.info_hash))
                .collect()
        }

        fn arrivals(&self, max: usize) -> Vec<Id> {
            let mut q = self.queue.borrow_mut();
            let n = max.min(q.len());
            q.drain(..n).collect()
        }

        fn entries(&self, ids: &[Id]) -> Vec<(Id, CatalogueEntry)> {
            self.widen(ids.len());
            self.held
                .borrow()
                .iter()
                .filter(|e| !self.hidden.borrow().contains(&e.info_hash))
                .filter(|e| ids.contains(&id_of(&e.info_hash)))
                .map(|e| (id_of(&e.info_hash), e.clone()))
                .collect()
        }

        fn rows(&self, entries: &[&CatalogueEntry]) -> Option<HashMap<Key, LinkRowMeta>> {
            self.widen(entries.len());
            let t = self.table.borrow();
            Some(
                entries
                    .iter()
                    .filter_map(|e| Some((key(e), t.get(&key(e))?.clone())))
                    .collect(),
            )
        }

        fn meta_page(&self, after: &Key, limit: usize) -> Option<Vec<(Key, LinkRowMeta)>> {
            let page: Vec<_> = self
                .table
                .borrow()
                .range::<Key, _>((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                .take(limit)
                .map(|(k, m)| (k.clone(), m.clone()))
                .collect();
            self.widest_page.set(self.widest_page.get().max(page.len()));
            Some(page)
        }

        fn holds(&self, session: &str, info_hash: &str) -> bool {
            session == "hoard" && self.held.borrow().iter().any(|e| e.info_hash == info_hash)
        }

        fn drop_rows(&self, keys: &[Key]) -> bool {
            for key in keys {
                self.table.borrow_mut().remove(key);
            }
            true
        }

        fn overflowed(&self) -> bool {
            self.overflow.replace(false)
        }

        fn measured_ids(&self) -> Option<Vec<Id>> {
            Some(self.table.borrow().keys().map(|(h, _)| id_of(h)).collect())
        }

        fn publish_summary(&self) {
            self.published.set(self.published.get() + 1);
        }

        fn put_rows(&self, rows: &[LinkRow]) -> Option<bool> {
            self.puts.set(self.puts.get() + 1);
            for r in rows {
                self.table
                    .borrow_mut()
                    .insert((r.info_hash.clone(), r.session.clone()), meta_of(r));
            }
            Some(true)
        }
    }

    /// Steps of the walk until a cycle ends (and never more than a sane number).
    fn walk_cycle(sc: &mut Scanner, io: &Fake) -> usize {
        let before = sc.progress.cycles.load(Ordering::Relaxed);
        for steps in 1..=10_000 {
            sc.walk_step(io).expect("the store reads");
            if sc.progress.cycles.load(Ordering::Relaxed) > before {
                return steps;
            }
        }
        panic!("the cycle never ended");
    }

    fn never_more_than_a_packet(io: &Fake) {
        assert!(
            io.widest.get() <= WALK_BATCH,
            "a step read more than a packet: {} torrents or rows in one call (packet {WALK_BATCH})",
            io.widest.get()
        );
    }

    /// What a round did before this walk, over the same `Io`: the whole
    /// catalogue's entries in one call, the whole index in one page, then a
    /// sweep of what was due. Kept as the thing the guard below must catch.
    fn old_round(io: &Fake, progress: &Progress) {
        let all = io.ids();
        let _everything = io.entries(&all);
        let _whole_index = io.meta_page(&Key::default(), usize::MAX).unwrap();
        let _ = progress;
    }

    /// ⭐ At 1.04 M torrents each whole round was +2 to 3.5 GiB and a core at
    /// 100 % for minutes. In steady state a step now reads one packet: not the
    /// catalogue, not the index, and the list of ids once per cycle.
    #[test]
    fn a_step_reads_at_most_a_packet_and_never_the_catalogue_or_the_index() {
        let io = Fake::new(350);
        let mut sc = Scanner::new(progress());
        assert_eq!(walk_cycle(&mut sc, &io), 4, "350 torrents, 100 a packet");
        assert_eq!(io.table.borrow().len(), 350, "the first cycle measures all");
        never_more_than_a_packet(&io);
        assert_eq!(io.id_calls.get(), 1);

        let puts = io.puts.get();
        assert_eq!(walk_cycle(&mut sc, &io), 4);
        assert_eq!(io.puts.get(), puts, "nothing is due: nothing is written");
        never_more_than_a_packet(&io);
        assert_eq!(io.id_calls.get(), 2, "the id list is taken once a cycle");
        assert!(io.widest_page.get() == 0, "the walk never pages the index");
    }

    /// The guard of the test above is not vacuous: the old round, through the
    /// same `Io`, trips it.
    #[test]
    #[should_panic(expected = "a step read more than a packet")]
    fn the_old_whole_round_trips_the_guard() {
        let io = Fake::new(350);
        old_round(&io, progress());
        never_more_than_a_packet(&io);
    }

    /// ⭐ A torrent added is measured by the next look at the queue, not when
    /// the walk gets to it.
    #[test]
    fn an_added_torrent_is_measured_at_the_next_step_before_the_cycle_ends() {
        let io = Fake::new(350);
        let mut sc = Scanner::new(progress());
        sc.walk_step(&io).unwrap();
        assert_eq!(sc.pos, WALK_BATCH, "the walk is a quarter through");

        io.add("zz-added");
        assert!(!io.measured("zz-added"));
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 1);
        assert!(io.measured("zz-added"), "measured by the queue");
        assert_eq!(sc.pos, WALK_BATCH, "the cursor has not moved for it");
        never_more_than_a_packet(&io);
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 0, "and only once");

        // A move is an arrival too: the row under the old path is not valid.
        io.held.borrow_mut()[0] = c(&name(0), "/elsewhere", 1);
        io.queue.borrow_mut().push_back(id_of(&name(0)));
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 1);
        assert_eq!(io.table.borrow()[&k(&name(0))].save_path, "/elsewhere");
    }

    /// The engine holds a torrent a moment before the store has its row: it
    /// waits in the queue, and is measured as soon as the store knows it.
    #[test]
    fn an_arrival_the_store_does_not_know_yet_waits_and_is_then_measured() {
        let io = Fake::new(3);
        let mut sc = Scanner::new(progress());
        io.hidden.borrow_mut().insert("late".into());
        io.add("late");
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 0);
        assert_eq!(sc.progress.queued.load(Ordering::Relaxed), 1);
        io.hidden.borrow_mut().clear();
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 1);
        assert!(io.measured("late"));
        assert_eq!(sc.progress.queued.load(Ordering::Relaxed), 0);
    }

    /// A measurement older than `REFRESH_SECS` is taken again when the walk
    /// comes by; a recent one is not.
    #[test]
    fn the_walk_measures_again_what_is_old_and_leaves_what_is_recent() {
        let io = Fake::new(0);
        let now = crate::store::now_secs();
        for n in ["old", "recent", "moved"] {
            io.held.borrow_mut().push(c(n, "/d", 1));
        }
        io.held.borrow_mut()[2] = c("moved", "/new", 1);
        {
            let mut t = io.table.borrow_mut();
            t.insert(k("old"), m("/d", now - REFRESH_SECS - 5, 1, 0));
            t.insert(k("recent"), m("/d", now - 60, 1, 0));
            t.insert(k("moved"), m("/old", now - 60, 1, 0));
        }
        let mut sc = Scanner::new(progress());
        walk_cycle(&mut sc, &io);
        assert!(io.at("old") >= now, "old: measured again");
        assert_eq!(io.at("recent"), now - 60, "recent: left alone");
        assert_eq!(io.table.borrow()[&k("moved")].save_path, "/new");
    }

    /// A removed torrent's row goes within one cycle of the orphan walk, the
    /// others stay, and the walk pages (here three pages of the index).
    #[test]
    fn an_orphan_is_dropped_within_one_orphan_cycle() {
        let io = Fake::new(1200);
        let p = progress();
        let mut sc = Scanner::new(p);
        for i in 0..1200 {
            io.table
                .borrow_mut()
                .insert(k(&name(i)), m("/d", 100, 1, 1));
        }
        // Three torrents gone from the engine: one in each page.
        for i in [10, 600, 1190] {
            io.remove(&name(i));
        }
        // And a row of a session no engine has.
        io.table
            .borrow_mut()
            .insert(("t00001".into(), "race".into()), m("/d", 100, 1, 1));
        assert!(sc.count_table(&io));
        let before = p.counts.get().measured;
        io.widest_page.set(0);

        let mut pages = 0;
        while p.orphan_cycles.load(Ordering::Relaxed) == 0 {
            sc.orphan_step(&io).expect("reads");
            pages += 1;
            assert!(pages < 50, "the cycle never ended");
        }
        assert_eq!(pages, 3, "1201 rows, 500 a page");
        assert!(io.widest_page.get() <= ORPHAN_PAGE);
        for i in [10, 600, 1190] {
            assert!(!io.measured(&name(i)), "{} is dropped", name(i));
        }
        assert!(!io
            .table
            .borrow()
            .contains_key(&("t00001".into(), "race".into())));
        assert!(io.measured(&name(11)) && io.measured(&name(1)));
        assert_eq!(io.table.borrow().len(), 1197);
        assert_eq!(p.orphans_dropped.load(Ordering::Relaxed), 4);
        assert_eq!(
            p.counts.get().measured,
            before - 4,
            "and the counters follow"
        );
    }

    /// ⭐ The counters are kept by deltas, and the end of a walk cycle sets them
    /// to what the walk counted: whatever the deltas got wrong is gone.
    #[test]
    fn the_end_of_a_cycle_resets_the_counters_to_the_recount() {
        let io = Fake::new(250);
        let p = progress();
        let mut sc = Scanner::new(p);
        assert!(sc.count_table(&io));
        assert_eq!(p.counts.get(), Tally::default());

        // Deltas alone: a first walk writes 250 rows and counts them by delta
        // before the cycle's recount replaces them. Stop short of its end.
        sc.walk_step(&io).unwrap();
        sc.walk_step(&io).unwrap();
        assert_eq!(p.counts.get().measured, 200, "by delta, mid-cycle");

        // Wrong on purpose, in every field.
        p.counts.measured.fetch_add(777, Ordering::Relaxed);
        p.counts.files.fetch_add(-5, Ordering::Relaxed);
        p.counts.data_missing.fetch_add(31, Ordering::Relaxed);
        p.counts.partly_missing.fetch_add(9, Ordering::Relaxed);
        p.counts.oldest.store(1, Ordering::Relaxed);
        assert_ne!(p.counts.get(), io.recount());

        walk_cycle(&mut sc, &io);
        let want = io.recount();
        assert_eq!(want.measured, 250);
        assert_eq!(p.counts.get(), want, "the recount is what is published");
        assert!(p.counted_at.load(Ordering::Relaxed) > 0);
    }

    /// The first count reads the index a page at a time and keeps none of it.
    #[test]
    fn the_first_count_reads_pages_and_gets_the_totals_right() {
        let io = Fake::new(0);
        for i in 0..(2 * COUNT_PAGE + 1) {
            let (files, missing) = match i % 3 {
                0 => (2, 2),
                1 => (3, 1),
                _ => (1, 0),
            };
            io.table
                .borrow_mut()
                .insert(k(&name(i)), m("/d", 1000 + i as i64, files, missing));
        }
        let p = progress();
        assert_eq!(p.counted_at.load(Ordering::Relaxed), 0);
        assert!(Scanner::new(p).count_table(&io));
        assert!(io.widest_page.get() <= COUNT_PAGE);
        assert_eq!(p.counts.get(), io.recount());
        assert_eq!(p.counts.get().measured, 2 * COUNT_PAGE as i64 + 1);
        assert_eq!(p.counts.get().oldest, 1000);
    }

    /// ⭐ The pause makes a cycle last a day at a million torrents, and stays
    /// inside its bounds whatever the catalogue and however slow a step is.
    #[test]
    fn the_pause_makes_a_cycle_last_a_day_at_a_million_torrents_within_bounds() {
        let work = Duration::from_millis(50);
        let n = 1_000_000;
        let pause = walk_pause(n, work);
        let packets = n.div_ceil(WALK_BATCH) as f64;
        let cycle = packets * (pause + work).as_secs_f64();
        assert!(
            (cycle - REFRESH_SECS as f64).abs() < 0.01 * REFRESH_SECS as f64,
            "a cycle of {cycle} s, wanted ~{REFRESH_SECS}"
        );
        assert!((pause.as_secs_f64() - 8.59).abs() < 0.05, "{pause:?}");

        // Bounds.
        assert_eq!(
            walk_pause(0, Duration::ZERO),
            WALK_MAX_PAUSE,
            "tiny: ceiling"
        );
        assert_eq!(walk_pause(1_000, Duration::ZERO), WALK_MAX_PAUSE);
        assert_eq!(
            walk_pause(2_000_000_000, Duration::ZERO),
            WALK_MIN_PAUSE,
            "huge: floor"
        );
        for n in [0, 1, 99, 100, 101, 10_000, 1_000_000, 50_000_000] {
            for ms in [0, 1, 50, 2_000, 60_000] {
                let work = Duration::from_millis(ms);
                let pause = walk_pause(n, work);
                assert!(pause >= WALK_MIN_PAUSE, "{n} {work:?}");
                // Working never takes more than 1 / WALK_DUTY of the time...
                assert!(
                    work.as_secs_f64() * WALK_DUTY as f64
                        <= (work + pause).as_secs_f64() * 1.0001 + 1e-9,
                    "{n} {work:?}: {pause:?} leaves the walk over its share"
                );
                // ...and past the ceiling only because of that.
                assert!(
                    pause <= WALK_MAX_PAUSE || pause == work * (WALK_DUTY - 1),
                    "{n} {work:?}: {pause:?}"
                );
            }
        }
        // The orphan walk, as well: an hour at a million rows.
        let p = orphan_pause(1_000_000);
        assert!((p.as_secs_f64() * 2000.0 - 3600.0).abs() < 60.0, "{p:?}");
        assert_eq!(orphan_pause(0), ORPHAN_MAX_PAUSE);
        assert_eq!(orphan_pause(2_000_000_000), ORPHAN_MIN_PAUSE);
    }

    /// ⭐ Arrivals measured are published again, at the latest after
    /// `SUMMARY_EVERY` and not before; the end of a cycle publishes too.
    #[test]
    fn the_summary_is_published_again_after_arrivals_and_at_the_end_of_a_cycle() {
        let io = Fake::new(5);
        let mut sc = Scanner::new(progress());
        let t0 = Instant::now();
        sc.published_at = t0;
        assert!(
            !sc.summary_step(&io, t0 + SUMMARY_EVERY * 3),
            "nothing to say"
        );

        io.add("zz-new");
        assert_eq!(sc.arrivals_step(&io, t0), 1);
        assert!(!sc.summary_step(&io, t0 + SUMMARY_EVERY - Duration::from_secs(1)));
        assert_eq!(io.published.get(), 0, "not before the delay");
        assert!(sc.summary_step(&io, t0 + SUMMARY_EVERY));
        assert_eq!(io.published.get(), 1, "at the delay");
        assert!(!sc.summary_step(&io, t0 + SUMMARY_EVERY * 5));
        assert_eq!(io.published.get(), 1, "and once");

        walk_cycle(&mut sc, &io);
        assert_eq!(io.published.get(), 2, "the end of a cycle publishes");
    }

    /// ⭐ An arrival is measured seconds after it is added, often before its
    /// data is in place: with files missing it is measured again, 5 minutes
    /// later, and ends with the right count.
    #[test]
    fn an_arrival_measured_before_its_files_exist_is_measured_again() {
        let dir = std::env::temp_dir().join(format!("linkscan-late-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sp = dir.to_str().unwrap().to_string();
        let io = Fake::new(0);
        io.held.borrow_mut().push(c("late", &sp, 1));
        io.queue.borrow_mut().push_back(id_of("late"));
        let p = progress();
        let mut sc = Scanner::new(p);
        let t0 = Instant::now();

        assert_eq!(sc.arrivals_step(&io, t0), 1);
        assert_eq!(
            io.table.borrow()[&k("late")].missing,
            1,
            "nothing there yet"
        );
        assert_eq!(p.counts.get().data_missing, 1);
        assert_eq!(sc.rechecks.len(), 1);
        assert_eq!(
            sc.recheck_step(&io, t0 + Duration::from_secs(60)),
            0,
            "too soon"
        );

        std::fs::write(dir.join("0"), b"x").unwrap();
        // The clock of the rows is the wall clock: age the row by 6 minutes.
        io.table
            .borrow_mut()
            .get_mut(&k("late"))
            .unwrap()
            .measured_at -= 360;
        assert_eq!(sc.recheck_step(&io, t0 + RECHECK_BACKOFF[0]), 1);
        assert_eq!(
            io.table.borrow()[&k("late")].missing,
            0,
            "found at the second look"
        );
        assert_eq!(p.counts.get().data_missing, 0, "and counted so");
        assert!(sc.rechecks.is_empty(), "and no third look");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A torrent whose files never come is looked at three times, 5 minutes,
    /// 30 and 2 hours apart, then left to the walk.
    #[test]
    fn the_rechecks_back_off_and_stop() {
        let io = Fake::new(0);
        io.held.borrow_mut().push(c("never-there", "/nowhere", 1));
        io.queue.borrow_mut().push_back(id_of("never-there"));
        let mut sc = Scanner::new(progress());
        let mut now = Instant::now();
        sc.arrivals_step(&io, now);
        for (n, wait) in RECHECK_BACKOFF.iter().enumerate() {
            assert_eq!(sc.rechecks.len(), 1, "look {n} is scheduled");
            assert_eq!(sc.rechecks[0].2, now + *wait);
            now += *wait;
            io.table
                .borrow_mut()
                .get_mut(&k("never-there"))
                .unwrap()
                .measured_at -= 400;
            assert_eq!(sc.recheck_step(&io, now), 1);
        }
        assert!(sc.rechecks.is_empty(), "then the normal rhythm");
    }

    #[test]
    fn a_row_with_files_missing_is_due_again_soon_and_a_whole_one_is_not() {
        let now = 1_000_000;
        let e = c("x", "/d", 1);
        assert!(
            !due(Some(&m("/d", now - 60, 1, 1)), &e, now),
            "just measured"
        );
        assert!(
            due(Some(&m("/d", now - 400, 1, 1)), &e, now),
            "missing, 6 min"
        );
        assert!(
            !due(Some(&m("/d", now - 3600, 1, 0)), &e, now),
            "whole, 1 h"
        );
    }

    /// ⭐ The torrents with no row go first, then the walk goes on in hash
    /// order. Without the priority the first packet is the first in hash order.
    #[test]
    fn the_never_measured_go_first_then_the_walk_goes_on() {
        let now = crate::store::now_secs();
        let build = || {
            let io = Fake::new(150);
            for i in 0..30 {
                io.table
                    .borrow_mut()
                    .insert(k(&name(i)), m("/d", now - 60, 1, 0));
            }
            io
        };
        let io = build();
        let mut sc = Scanner::new(progress());
        sc.walk_step(&io).unwrap();
        assert_eq!(io.table.borrow().len(), 130, "a packet of never-measured");
        assert!(io.measured(&name(129)) && !io.measured(&name(130)));
        assert!(!sc.want_priority);
        assert_eq!(io.at(&name(5)), now - 60, "the fresh are left alone");

        let io = build();
        let mut sc = Scanner::new(progress());
        sc.want_priority = false;
        sc.walk_step(&io).unwrap();
        assert!(
            io.measured(&name(99)) && !io.measured(&name(100)),
            "hash order"
        );
    }

    /// ⭐ A queue that dropped arrivals (a bulk add past its bound) sends the
    /// walk back to the start of a new list, the torrents with no row first.
    #[test]
    fn a_queue_that_overflowed_puts_the_lost_arrivals_first() {
        let now = crate::store::now_secs();
        let io = Fake::new(250);
        for i in 0..250 {
            io.table
                .borrow_mut()
                .insert(k(&name(i)), m("/d", now - 60, 1, 0));
        }
        let mut sc = Scanner::new(progress());
        sc.walk_step(&io).unwrap();
        assert_eq!(sc.pos, WALK_BATCH);

        // 300 torrents the engine holds that the queue never told us about.
        for i in 0..300 {
            io.held.borrow_mut().push(c(&format!("z{i:05}"), "/d", 1));
        }
        io.overflow.set(true);
        sc.arrivals_step(&io, Instant::now());
        assert!(sc.want_priority && sc.cycle.is_none());
        sc.walk_step(&io).unwrap();
        assert!(io.measured("z00099") && !io.measured("z00100"));
    }

    /// Fifty arrivals waiting for the store do not stop the next one from
    /// being measured (they used to take the whole look for five minutes).
    #[test]
    fn waiting_arrivals_do_not_block_new_ones() {
        let io = Fake::new(0);
        let mut sc = Scanner::new(progress());
        for i in 0..ARRIVAL_BATCH {
            let h = format!("h{i:03}");
            io.hidden.borrow_mut().insert(h.clone());
            io.add(&h);
        }
        sc.arrivals_step(&io, Instant::now());
        assert_eq!(sc.waiting.len(), ARRIVAL_BATCH);
        io.add("ok");
        assert_eq!(sc.arrivals_step(&io, Instant::now()), 1);
        assert!(io.measured("ok"));
    }
}
