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

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::engines::EngineHost;
use crate::rulesrun::{self, CatalogueEntry};
use crate::store::{LinkRowMeta, StoreLock};

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
    let mut keyed: Vec<((u8, i64), usize)> = Vec::new();
    for (i, c) in cat.iter().enumerate() {
        match meta.get(&(c.info_hash.clone(), c.session.clone())) {
            Some(m) if m.save_path == c.save_path && m.files == c.paths.len() as i64 => {
                if now - m.measured_at >= REFRESH_SECS {
                    keyed.push(((1, m.measured_at), i));
                }
            }
            _ => keyed.push(((0, 0), i)),
        }
    }
    keyed.sort_by_key(|(k, _)| *k);
    keyed.into_iter().map(|(_, i)| i).collect()
}

/// Rows whose torrent copy is no longer held.
///
/// They are harmless to a pass -- facts are built from the catalogue, never
/// from the table -- but they would keep growing the table and the counts.
pub fn orphans(
    cat: &[CatalogueEntry],
    meta: &HashMap<(String, String), LinkRowMeta>,
) -> Vec<(String, String)> {
    let held: std::collections::HashSet<(&str, &str)> =
        cat.iter().map(|c| (c.info_hash.as_str(), c.session.as_str())).collect();
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

fn run(engines: Arc<EngineHost>, store: Arc<StoreLock>) {
    std::thread::sleep(SETTLE);
    loop {
        // One snapshot per sweep, not per batch: reading a million rows and
        // resolving a million torrents' paths is seconds, not something to
        // redo every two thousand torrents.
        let snapshot = store.read().ok().map(|s| {
            (rulesrun::catalogue(&engines, &s), s.link_index_meta().unwrap_or_default())
        });
        let Some((cat, meta)) = snapshot else {
            std::thread::sleep(IDLE);
            continue;
        };
        PROGRESS.catalogue.store(cat.len() as i64, Ordering::Relaxed);

        let gone = orphans(&cat, &meta);
        for chunk in gone.chunks(BATCH) {
            if let Ok(s) = store.lock() {
                if let Err(e) = s.drop_link_rows(chunk) {
                    tracing::warn!("link index: could not drop removed torrents: {e}");
                }
            }
        }

        let order = due(&cat, &meta, crate::store::now_secs());
        drop(meta);
        if order.is_empty() {
            std::thread::sleep(IDLE);
            continue;
        }
        sweep(&cat, &order, &store);
    }
}

fn sweep(cat: &[CatalogueEntry], order: &[usize], store: &StoreLock) {
    let started = std::time::Instant::now();
    PROGRESS.sweep_total.store(order.len() as i64, Ordering::Relaxed);
    PROGRESS.sweep_done.store(0, Ordering::Relaxed);
    PROGRESS.sweep_started.store(crate::store::now_secs(), Ordering::Relaxed);
    let (mut files, mut missing, mut done) = (0usize, 0usize, 0usize);

    for chunk in order.chunks(BATCH) {
        let t0 = std::time::Instant::now();
        let plan = chunk
            .iter()
            .map(|&i| (cat[i].info_hash.clone(), cat[i].paths.clone()))
            .collect();
        let measured = rulesrun::stat_plan_with(plan, rulesrun::scan_threads());
        let now = crate::store::now_secs();
        let mut batch_files = 0usize;
        let rows: Vec<_> = chunk
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
        match store.lock() {
            Ok(s) => {
                if let Err(e) = s.put_link_rows(&rows) {
                    tracing::warn!("link index: batch not written: {e}");
                }
            }
            Err(_) => return,
        }
        files += batch_files;
        done += chunk.len();
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        PROGRESS.sweep_done.store(done as i64, Ordering::Relaxed);
        PROGRESS.last_batch_at.store(now, Ordering::Relaxed);
        PROGRESS.files_per_sec.store((batch_files as f64 / secs) as i64, Ordering::Relaxed);
        std::thread::sleep(PAUSE);
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
        CatalogueEntry {
            info_hash: hash.into(),
            session: "hoard".into(),
            save_path: save_path.into(),
            paths: (0..files).map(|i| std::path::PathBuf::from(format!("{save_path}/{i}"))).collect(),
        }
    }

    fn m(save_path: &str, measured_at: i64, files: i64) -> LinkRowMeta {
        LinkRowMeta { save_path: save_path.into(), measured_at, files }
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

        let order: Vec<&str> = due(&cat, &meta, now).iter().map(|&i| cat[i].info_hash.as_str()).collect();
        assert_eq!(&order[3..], &["older", "old"], "stale ones after, oldest first");
        let mut first: Vec<&str> = order[..3].to_vec();
        first.sort();
        assert_eq!(first, vec!["moved", "never", "regrown"], "anything without a valid row first");
        assert!(!order.contains(&"fresh"));
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
            vec![("kept".to_string(), "race".to_string()), ("removed".to_string(), "hoard".to_string())],
            "the copy in another engine is its own row"
        );
    }
}
