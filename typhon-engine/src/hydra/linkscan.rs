//! The link index, kept current by a thread of its own.
//!
//! A workflow on hardlinks used to stat the whole catalogue itself, on the
//! request that asked: at a million torrents that is some three million
//! `statx` on a pool serving torrents at the same time, and the dry-run button
//! sat for longer than anyone waits. Now this thread walks the catalogue at
//! its own pace and writes what it finds to the store (`link_index`); a pass
//! reads the store, and re-measures only the torrents it is about to act on.
//!
//! ⭐ Two things fall out of keeping the measurement: a restart resumes where
//! the last sweep stopped instead of starting over, and a torrent whose files
//! are gone is found by this walk -- not only when a peer asks us for a piece
//! we cannot serve.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::engines::EngineHost;
use crate::rulesrun::{self, CatalogueEntry, CatalogueFields};
use crate::store::{LinkRow, LinkRowMeta, StoreLock};

/// A measurement older than this is taken again. Hardlinks appear when the
/// media library imports something, which is not a per-minute event; a day
/// keeps a million-torrent catalogue current at a few `statx` a second.
pub const REFRESH_SECS: i64 = 24 * 3600;

/// Torrents measured between two writes. Small enough that the store lock is
/// held for a few tens of milliseconds, large enough that the write is not
/// the cost.
const BATCH: usize = 2000;

/// Before the first sweep: the catalogue must be loaded, or the thread would
/// measure half of it and call the rest removed.
const SETTLE: Duration = Duration::from_secs(180);

/// Between batches. The pool also serves torrents; the scanner yields.
const PAUSE: Duration = Duration::from_secs(1);

/// When nothing is due. A torrent added in the meantime is picked up then.
const IDLE: Duration = Duration::from_secs(300);

/// During a long sweep, how often the list's summary is brought up to date.
/// A first sweep of a million torrents takes hours; the column should not sit
/// on "never measured" for all of them until the last batch lands. Each
/// refresh reads every measurement and counts the catalogue (seconds of CPU,
/// the cost of one workflow pass), so not after every batch.
const SUMMARY_EVERY: Duration = Duration::from_secs(600);

/// Between the end of a sweep and the next look at the catalogue. Reading the
/// store and walking a million torrents is seconds of work; a sweep of thirty
/// torrents must not set it off again the moment it ends.
const ROUND_PAUSE: Duration = Duration::from_secs(5);

/// How often a round reads the whole link index and the whole catalogue: to
/// find the rows of removed torrents, which needs both side by side, and to
/// start the next stretch from what the store really holds. Between two of
/// them a round only looks at the torrents that changed. The whole read is
/// ~3 GiB of resident memory for 25-40 s at 1.85 M torrents; done on every
/// round it came back every few minutes, each time a handful of torrents were
/// added. A removed torrent's row waits at most this long to be dropped, and it
/// is harmless meanwhile (see `orphans`).
const FULL_EVERY: Duration = Duration::from_secs(30 * 60);

/// Whether the list's summary is recomputed now. A sweep always leaves it
/// stale, and a full refresh is seconds of reading and counting a million
/// rows: done after every small sweep it ran the scanner flat out, one round
/// every half minute for thirty torrents due. Stale summaries wait for
/// `SUMMARY_EVERY` since the last one.
fn summary_due(stale: bool, since_last: Duration) -> bool {
    stale && since_last >= SUMMARY_EVERY
}

/// Where the scanner is, for the status line. Plain atomics: written by one
/// thread, read by a request that only wants an approximate picture.
pub struct Progress {
    pub catalogue: AtomicI64,
    pub sweep_total: AtomicI64,
    pub sweep_done: AtomicI64,
    pub sweep_started: AtomicI64,
    pub last_batch_at: AtomicI64,
    pub files_per_sec: AtomicI64,
}

pub static PROGRESS: Progress = Progress {
    catalogue: AtomicI64::new(0),
    sweep_total: AtomicI64::new(0),
    sweep_done: AtomicI64::new(0),
    sweep_started: AtomicI64::new(0),
    last_batch_at: AtomicI64::new(0),
    files_per_sec: AtomicI64::new(0),
};

/// Which copies need measuring, most urgent first.
///
/// Never measured -- or measured under another save path, or with another
/// number of files, which is the same thing -- comes before anything else: a
/// torrent nobody has looked at is invisible to every rule. Then the oldest
/// measurements past `REFRESH_SECS`. Fresh ones are not due.
pub fn due(
    cat: &[CatalogueEntry],
    meta: &HashMap<(String, String), LinkRowMeta>,
    now: i64,
) -> Vec<usize> {
    due_by(
        cat,
        |c| match meta.get(&(c.info_hash.clone(), c.session.to_string())) {
            Some(m) if *m.save_path == *c.save_path && m.files == c.files as i64 => {
                Some(m.measured_at)
            }
            _ => None,
        },
        now,
    )
}

/// `due`, with the measurement date of a copy's valid row (`None`: no row, or
/// one taken under another save path or file count) given by `at`.
fn due_by(
    cat: &[CatalogueEntry],
    at: impl Fn(&CatalogueEntry) -> Option<i64>,
    now: i64,
) -> Vec<usize> {
    let mut keyed: Vec<((u8, i64), usize)> = Vec::new();
    for (i, c) in cat.iter().enumerate() {
        match at(c) {
            Some(t) => {
                if now - t >= REFRESH_SECS {
                    keyed.push(((1, t), i));
                }
            }
            None => keyed.push(((0, 0), i)),
        }
    }
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, i)| i).collect()
}

/// What a round remembers of the link index from one round to the next: for
/// each valid row, when it was measured, under the name of a 64-bit
/// fingerprint of (hash, engine, save path, file count).
///
/// ⭐ Not the rows' bookkeeping themselves: those are two strings and a path a
/// row, ~0.5 GiB at 1.85 M torrents, held for good to save a read. 16 bytes a
/// row is ~40 MiB. A fingerprint collision would make an unmeasured torrent
/// look measured -- once in ~10^13 additions -- until the next whole read,
/// which rebuilds this from the rows themselves.
#[derive(Default)]
struct Known(HashMap<u64, i64>);

impl Known {
    fn key(info_hash: &str, session: &str, save_path: &str, files: i64) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (info_hash, session, save_path, files).hash(&mut h);
        h.finish()
    }

    fn at(&self, f: &CatalogueFields<'_>) -> Option<i64> {
        self.0
            .get(&Self::key(
                f.info_hash,
                f.session,
                f.save_path,
                f.files as i64,
            ))
            .copied()
    }

    /// What the store holds, minus the rows about to be dropped.
    fn from_meta(
        meta: &HashMap<(String, String), LinkRowMeta>,
        gone: &HashSet<&(String, String)>,
    ) -> Self {
        Known(
            meta.iter()
                .filter(|(k, _)| !gone.contains(k))
                .map(|((h, s), m)| (Self::key(h, s, &m.save_path, m.files), m.measured_at))
                .collect(),
        )
    }

    /// A row the sweep has just written.
    fn record(&mut self, r: &LinkRow) {
        self.0.insert(
            Self::key(&r.info_hash, &r.session, &r.save_path, r.files),
            r.measured_at,
        );
    }
}

/// Whether a torrent the engines hold is for a round to look at: no valid row
/// for it, or one old enough to be taken again.
fn needs_measuring(known: &Known, f: &CatalogueFields<'_>, now: i64) -> bool {
    known.at(f).is_none_or(|t| now - t >= REFRESH_SECS)
}

/// Rows whose torrent copy is no longer held.
///
/// They are harmless to a pass -- facts are built from the catalogue, never
/// from the table -- but they would keep growing the table and the counts.
pub fn orphans(
    cat: &[CatalogueEntry],
    meta: &HashMap<(String, String), LinkRowMeta>,
) -> Vec<(String, String)> {
    let held: std::collections::HashSet<(&str, &str)> = cat
        .iter()
        .map(|c| (c.info_hash.as_str(), &*c.session))
        .collect();
    meta.keys()
        .filter(|(h, s)| !held.contains(&(h.as_str(), s.as_str())))
        .cloned()
        .collect()
}

pub fn spawn(engines: Arc<EngineHost>, store: Arc<StoreLock>) {
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

/// The whole catalogue and the whole index's bookkeeping, as `Io::read_all`
/// returns them.
type WholeRead = (Vec<CatalogueEntry>, HashMap<(String, String), LinkRowMeta>);

/// Everything a round asks of the outside world, so that a test can count
/// the whole reads it makes.
trait Io {
    /// The whole catalogue and the whole index's bookkeeping. The expensive
    /// read (`FULL_EVERY`). `None` when the store could not be read.
    fn read_all(&self) -> Option<WholeRead>;
    /// Only the torrents `needs_measuring`, that the store knows. `None` when
    /// there are more than a sweep's worth: the caller reads everything then.
    fn changed(&self, known: &Known, now: i64) -> Option<Vec<CatalogueEntry>>;
    fn drop_rows(&self, keys: &[(String, String)]);
    /// `None`: the store is gone, stop. `Some(false)`: the batch was not
    /// written.
    fn put_rows(&self, rows: &[LinkRow]) -> Option<bool>;
    fn publish_summary(&self);
}

struct Live<'a> {
    engines: &'a EngineHost,
    store: &'a StoreLock,
}

impl Io for Live<'_> {
    fn read_all(&self) -> Option<WholeRead> {
        // Read a page at a time, the connection let go between two pages.
        let stored = rulesrun::stored_copies(self.engines, self.store);
        let meta = self.store.link_index_meta().ok()?;
        // Walked after the connection is released.
        let cat = rulesrun::catalogue_from(self.engines, &stored);
        Some((cat, meta))
    }

    fn changed(&self, known: &Known, now: i64) -> Option<Vec<CatalogueEntry>> {
        let mut seen = 0usize;
        let mut cat = rulesrun::catalogue_where(
            self.engines,
            |_, _| true,
            |f| {
                if !needs_measuring(known, f, now) {
                    return false;
                }
                seen += 1;
                seen <= BATCH
            },
        );
        if seen > BATCH {
            return None;
        }
        // The store's say on the few that are left, not a read of its million.
        let mut held: HashMap<String, HashSet<String>> = HashMap::new();
        for c in &cat {
            held.entry(c.session.to_string()).or_default();
        }
        for (session, set) in held.iter_mut() {
            let hashes: Vec<&str> = cat
                .iter()
                .filter(|c| *c.session == **session)
                .map(|c| c.info_hash.as_str())
                .collect();
            *set = self.store.held_among(session, &hashes).ok()?;
        }
        cat.retain(|c| held[&*c.session].contains(&c.info_hash));
        Some(cat)
    }

    fn drop_rows(&self, keys: &[(String, String)]) {
        if let Ok(s) = self.store.lock() {
            if let Err(e) = s.drop_link_rows(keys) {
                tracing::warn!("link index: could not drop removed torrents: {e}");
            }
        }
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

    fn publish_summary(&self) {
        refresh_summary(self.engines, self.store);
    }
}

/// What the scanner carries from one round to the next.
struct State {
    known: Known,
    /// When the index was last read whole; `None` before the first time, so
    /// that the first round after a start always is one.
    full_at: Option<std::time::Instant>,
    full_every: Duration,
}

impl State {
    fn new() -> Self {
        State {
            known: Known::default(),
            full_at: None,
            full_every: FULL_EVERY,
        }
    }
}

enum Round {
    /// The store could not be read; try again later.
    Failed,
    Done {
        /// Rows of removed torrents dropped.
        dropped: usize,
        /// Torrents the sweep was asked to measure.
        swept: usize,
    },
}

/// One look at the catalogue, and a sweep of what it found due.
///
/// ⭐ A round reads the whole index and the whole catalogue only the first
/// time, then every `FULL_EVERY`: that is when the rows of removed torrents
/// are found, and `State::known` is rebuilt from the store. In between it
/// walks the engines (no store, no allocation kept) for the torrents that are
/// not in `known` or are due again, asks the store about those few alone, and
/// sweeps them; `sweep` adds what it writes to `known`.
fn round(io: &dyn Io, st: &mut State, pause: Duration) -> Round {
    let now = crate::store::now_secs();
    let full = st.full_at.is_none_or(|at| at.elapsed() >= st.full_every);
    let light = if full {
        None
    } else {
        io.changed(&st.known, now)
    };
    let (cat, order, dropped) = match light {
        Some(cat) => {
            let order = due_by(&cat, |c| st.known.at(&fields(c)), now);
            (cat, order, 0)
        }
        None => {
            let Some((cat, meta)) = io.read_all() else {
                return Round::Failed;
            };
            PROGRESS
                .catalogue
                .store(cat.len() as i64, Ordering::Relaxed);
            let gone = orphans(&cat, &meta);
            for chunk in gone.chunks(BATCH) {
                io.drop_rows(chunk);
            }
            st.known = Known::from_meta(&meta, &gone.iter().collect());
            st.full_at = Some(std::time::Instant::now());
            let order = due(&cat, &meta, now);
            (cat, order, gone.len())
        }
    };
    if !order.is_empty() {
        sweep(&cat, &order, io, &mut st.known, pause);
    }
    Round::Done {
        dropped,
        swept: order.len(),
    }
}

fn fields(c: &CatalogueEntry) -> CatalogueFields<'_> {
    CatalogueFields {
        info_hash: &c.info_hash,
        session: &c.session,
        save_path: &c.save_path,
        files: c.files,
    }
}

fn run(engines: Arc<EngineHost>, store: Arc<StoreLock>) {
    // What the store holds from before the restart is published NOW, not after
    // the settle: it is a read, and the catalogue is loaded by the time this
    // thread is spawned. Waiting left the Hardlinks and Checked columns at "-"
    // for every torrent for four minutes after each start (rc32, 2026-10-03).
    // Only the measuring waits for the disks to settle.
    let mut summary_stale = refresh_summary(&engines, &store).is_none();
    let mut summarised = (!summary_stale).then(std::time::Instant::now);
    std::thread::sleep(SETTLE);
    let io = Live {
        engines: &engines,
        store: &store,
    };
    let mut state = State::new();
    loop {
        // Between two passes, and between two batches in `sweep`: a batch
        // that has started is written whole.
        if crate::shutdown::stopping() {
            return;
        }
        let (dropped, swept) = match round(&io, &mut state, PAUSE) {
            Round::Failed => {
                std::thread::sleep(IDLE);
                continue;
            }
            Round::Done { dropped, swept } => (dropped, swept),
        };
        if crate::shutdown::stopping() {
            return;
        }
        // A removed torrent's names leave the count: another torrent that
        // shared its files may now have an external link. And a sweep always
        // leaves the summary stale.
        summary_stale |= dropped > 0 || swept > 0;
        if summary_due(
            summary_stale,
            summarised.map_or(Duration::MAX, |at| at.elapsed()),
        ) {
            refresh_summary(&engines, &store);
            summary_stale = false;
            summarised = Some(std::time::Instant::now());
        }
        std::thread::sleep(if swept == 0 { IDLE } else { ROUND_PAUSE });
    }
}

fn sweep(cat: &[CatalogueEntry], order: &[usize], io: &dyn Io, known: &mut Known, pause: Duration) {
    let started = std::time::Instant::now();
    let mut summarised = std::time::Instant::now();
    PROGRESS
        .sweep_total
        .store(order.len() as i64, Ordering::Relaxed);
    PROGRESS.sweep_done.store(0, Ordering::Relaxed);
    PROGRESS
        .sweep_started
        .store(crate::store::now_secs(), Ordering::Relaxed);
    let (mut files, mut missing, mut done) = (0usize, 0usize, 0usize);

    for chunk in order.chunks(BATCH) {
        let t0 = std::time::Instant::now();
        // Paths for this batch only, dropped with it. A torrent removed since
        // the catalogue was taken has none, and is not measured.
        let (live, plan): (Vec<usize>, Vec<_>) = chunk
            .iter()
            .filter_map(|&i| Some((i, (cat[i].info_hash.clone(), cat[i].paths()?))))
            .unzip();
        let measured = rulesrun::stat_plan_with(plan, rulesrun::scan_threads());
        let now = crate::store::now_secs();
        let mut batch_files = 0usize;
        let rows: Vec<_> = live
            .iter()
            .zip(&measured)
            .map(|(&i, (_, fs))| {
                let stats: Vec<_> = fs.iter().map(|(_, st)| *st).collect();
                batch_files += stats.len();
                if !stats.is_empty() && stats.iter().all(Option::is_none) {
                    missing += 1;
                }
                rulesrun::link_row(&cat[i], &stats, now)
            })
            .collect();
        match io.put_rows(&rows) {
            Some(true) => rows.iter().for_each(|r| known.record(r)),
            Some(false) => {}
            None => return,
        }
        files += batch_files;
        done += chunk.len();
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        PROGRESS.sweep_done.store(done as i64, Ordering::Relaxed);
        PROGRESS.last_batch_at.store(now, Ordering::Relaxed);
        PROGRESS
            .files_per_sec
            .store((batch_files as f64 / secs) as i64, Ordering::Relaxed);
        if summarised.elapsed() >= SUMMARY_EVERY {
            io.publish_summary();
            summarised = std::time::Instant::now();
        }
        if crate::shutdown::stopping() {
            return;
        }
        std::thread::sleep(pause);
    }

    tracing::info!(
        torrents = done,
        files,
        data_missing = missing,
        secs = started.elapsed().as_secs(),
        "link index sweep complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn m(save_path: &str, measured_at: i64, files: i64) -> LinkRowMeta {
        LinkRowMeta {
            save_path: save_path.into(),
            measured_at,
            files,
        }
    }

    #[test]
    fn the_never_measured_go_first_then_the_oldest_and_fresh_ones_wait() {
        let now = 10 * REFRESH_SECS;
        let cat = vec![
            c("fresh", "/d", 1),
            c("old", "/d", 1),
            c("older", "/d", 1),
            c("never", "/d", 1),
            c("moved", "/new", 1),
            c("regrown", "/d", 2),
        ];
        let mut meta = HashMap::new();
        let k = |h: &str| (h.to_string(), "hoard".to_string());
        meta.insert(k("fresh"), m("/d", now - 60, 1));
        meta.insert(k("old"), m("/d", now - REFRESH_SECS - 5, 1));
        meta.insert(k("older"), m("/d", now - 3 * REFRESH_SECS, 1));
        meta.insert(k("moved"), m("/old", now - 60, 1));
        meta.insert(k("regrown"), m("/d", now - 60, 1));

        let order: Vec<&str> = due(&cat, &meta, now)
            .iter()
            .map(|&i| cat[i].info_hash.as_str())
            .collect();
        assert_eq!(
            &order[3..],
            &["older", "old"],
            "stale ones after, oldest first"
        );
        let mut first: Vec<&str> = order[..3].to_vec();
        first.sort();
        assert_eq!(
            first,
            vec!["moved", "never", "regrown"],
            "anything without a valid row first"
        );
        assert!(!order.contains(&"fresh"));
    }

    /// ⭐ A sweep always leaves the summary stale; recomputing it every time
    /// ran the scanner flat out, a full read of the store every half minute
    /// for a handful of torrents due.
    #[test]
    fn a_small_sweep_does_not_recompute_the_summary_at_once() {
        let soon = Duration::from_secs(40);
        assert!(!summary_due(true, soon), "stale, but computed 40 s ago");
        assert!(!summary_due(true, SUMMARY_EVERY - Duration::from_secs(1)));
        assert!(summary_due(true, SUMMARY_EVERY), "stale long enough");
        assert!(summary_due(true, Duration::MAX), "never computed");
        assert!(!summary_due(false, Duration::MAX), "nothing changed");
    }

    #[test]
    fn rows_of_removed_torrents_are_found_and_held_ones_are_not() {
        let cat = vec![c("kept", "/d", 1)];
        let mut meta = HashMap::new();
        meta.insert(("kept".to_string(), "hoard".to_string()), m("/d", 0, 1));
        meta.insert(("kept".to_string(), "race".to_string()), m("/d", 0, 1));
        meta.insert(("removed".to_string(), "hoard".to_string()), m("/d", 0, 1));
        let mut gone = orphans(&cat, &meta);
        gone.sort();
        assert_eq!(
            gone,
            vec![
                ("kept".to_string(), "race".to_string()),
                ("removed".to_string(), "hoard".to_string())
            ],
            "the copy in another engine is its own row"
        );
    }

    /// The engines and the store, in memory, counting the whole reads.
    struct Fake {
        held: std::cell::RefCell<Vec<CatalogueEntry>>,
        table: std::cell::RefCell<HashMap<(String, String), LinkRowMeta>>,
        whole_reads: std::cell::Cell<usize>,
    }

    impl Fake {
        fn new(hashes: &[&str]) -> Self {
            Fake {
                held: std::cell::RefCell::new(hashes.iter().map(|h| c(h, "/d", 1)).collect()),
                table: Default::default(),
                whole_reads: Default::default(),
            }
        }

        fn add(&self, hash: &str) {
            self.held.borrow_mut().push(c(hash, "/d", 1));
        }

        fn remove(&self, hash: &str) {
            self.held.borrow_mut().retain(|e| e.info_hash != hash);
        }

        fn measured(&self, hash: &str) -> bool {
            self.table
                .borrow()
                .contains_key(&(hash.to_string(), "hoard".to_string()))
        }
    }

    impl Io for Fake {
        fn read_all(&self) -> Option<WholeRead> {
            self.whole_reads.set(self.whole_reads.get() + 1);
            Some((self.held.borrow().clone(), self.table.borrow().clone()))
        }

        fn changed(&self, known: &Known, now: i64) -> Option<Vec<CatalogueEntry>> {
            Some(
                self.held
                    .borrow()
                    .iter()
                    .filter(|e| needs_measuring(known, &fields(e), now))
                    .cloned()
                    .collect(),
            )
        }

        fn drop_rows(&self, keys: &[(String, String)]) {
            for k in keys {
                self.table.borrow_mut().remove(k);
            }
        }

        fn put_rows(&self, rows: &[LinkRow]) -> Option<bool> {
            for r in rows {
                self.table.borrow_mut().insert(
                    (r.info_hash.clone(), r.session.clone()),
                    LinkRowMeta {
                        save_path: r.save_path.clone(),
                        measured_at: r.measured_at,
                        files: r.files,
                    },
                );
            }
            Some(true)
        }

        fn publish_summary(&self) {}
    }

    fn swept(r: Round) -> usize {
        match r {
            Round::Done { swept, .. } => swept,
            Round::Failed => panic!("the round failed"),
        }
    }

    /// ⭐ At 1.85 M torrents every round read the whole index and the whole
    /// catalogue (~3 GiB of resident memory for 30 s) to measure the three to
    /// ten torrents that had been added. After the first round, one that finds
    /// a few new torrents must measure them and read nothing whole.
    #[test]
    fn rounds_after_the_first_measure_the_added_without_reading_everything() {
        let io = Fake::new(&["a", "b", "c", "d"]);
        let mut st = State::new();
        assert_eq!(swept(round(&io, &mut st, Duration::ZERO)), 4);
        assert_eq!(io.whole_reads.get(), 1, "the first round reads everything");

        for (n, added) in [["e", "f"], ["g", "h"], ["i", "j"]].iter().enumerate() {
            for h in added {
                io.add(h);
            }
            assert_eq!(swept(round(&io, &mut st, Duration::ZERO)), 2);
            assert!(added.iter().all(|h| io.measured(h)), "added are measured");
            assert_eq!(io.whole_reads.get(), 1, "round {n} read nothing whole");
        }
        assert_eq!(
            swept(round(&io, &mut st, Duration::ZERO)),
            0,
            "nothing changed, nothing to sweep"
        );
        assert_eq!(io.whole_reads.get(), 1);
    }

    #[test]
    fn a_copy_moved_to_another_save_path_is_measured_again_without_a_whole_read() {
        let io = Fake::new(&["a", "b"]);
        let mut st = State::new();
        round(&io, &mut st, Duration::ZERO);
        io.held.borrow_mut()[0] = c("a", "/elsewhere", 1);
        assert_eq!(swept(round(&io, &mut st, Duration::ZERO)), 1);
        assert_eq!(
            io.table.borrow()[&("a".into(), "hoard".into())].save_path,
            "/elsewhere"
        );
        assert_eq!(io.whole_reads.get(), 1);
    }

    /// A removed torrent's row is harmless, but it must not stay for ever: the
    /// whole round, run every `FULL_EVERY`, drops it and leaves the others.
    #[test]
    fn a_removed_torrent_leaves_the_index_at_the_next_whole_round() {
        let io = Fake::new(&["a", "b", "c"]);
        let mut st = State::new();
        round(&io, &mut st, Duration::ZERO);
        io.remove("b");
        assert_eq!(swept(round(&io, &mut st, Duration::ZERO)), 0);
        assert!(
            io.measured("b"),
            "an ordinary round does not look for orphans"
        );

        st.full_every = Duration::ZERO;
        let dropped = match round(&io, &mut st, Duration::ZERO) {
            Round::Done { dropped, .. } => dropped,
            Round::Failed => panic!("the round failed"),
        };
        assert_eq!(dropped, 1);
        assert!(!io.measured("b"));
        assert!(io.measured("a") && io.measured("c"));
        assert_eq!(io.whole_reads.get(), 2);

        // The same torrent back: it is measured again, not taken for known.
        io.add("b");
        st.full_every = FULL_EVERY;
        assert_eq!(swept(round(&io, &mut st, Duration::ZERO)), 1);
        assert!(io.measured("b"));
    }
}
