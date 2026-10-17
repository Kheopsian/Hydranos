//! The job runner: one task, one job at a time.
//!
//! The `jobs` table, its three routes and the Jobs tab have existed since the
//! V4 port with nothing ever writing a row. This is the half that was missing.
//!
//! ⚠ ONE job at a time, deliberately. Measured on this machine, /race to the
//! ZFS pool runs at ~520 MB/s with one copy and ~570 MB/s with four: the pool
//! is saturated on writes, not on concurrency. Running four graduations at
//! once buys 10% and puts four torrents in a seeding gap instead of one.
//!
//! ⚠ The store mutex is taken for the bookkeeping and RELEASED for the copy.
//! A long operation holding it freezes every route that touches the store --
//! which is exactly what a recheck was observed doing.

use std::sync::Arc;
use std::time::Duration;

use crate::api::AppState;

/// Queue a graduation: move a torrent's data to another engine's storage.
pub fn queue_graduation(
    state: &AppState,
    hash: &str,
    name: &str,
    from_engine: &str,
    to_engine: &str,
    to_category: &str,
    save_path: &str,
    total_bytes: i64,
) -> Option<String> {
    queue_graduation_allowing(state, hash, name, from_engine, to_engine, to_category, save_path, false, total_bytes)
}

/// `queue_graduation`, for an operator who agreed to break hardlinks.
pub fn queue_graduation_allowing(
    state: &AppState,
    hash: &str,
    name: &str,
    from_engine: &str,
    to_engine: &str,
    to_category: &str,
    save_path: &str,
    allow_breaking_hardlinks: bool,
    total_bytes: i64,
) -> Option<String> {
    // `name` and `target` are the two keys the Jobs tab reads (jobName() and
    // the Destination column). Without them a row says "graduate" against a
    // bare hash and an empty destination -- true, and useless to look at.
    let params = serde_json::json!({
        "name": name,
        "target": save_path,
        "from_engine": from_engine,
        "to_engine": to_engine,
        "to_category": to_category,
        "save_path": save_path,
        "allow_breaking_hardlinks": allow_breaking_hardlinks,
    })
    .to_string();
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    if store.job_pending_for("graduate", hash) || store.job_pending_for("move_data", hash) {
        return None;
    }
    store.create_job("graduate", hash, &params, total_bytes).ok()
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        {
            let store = match state.store.lock() {
                Ok(s) => s,
                Err(e) => e.into_inner(),
            };
            let n = store.requeue_running_jobs();
            if n > 0 {
                tracing::warn!(count = n, "jobs were running when the process last stopped; queued again");
            }
        }
        loop {
            let job = {
                let store = match state.store.lock() {
                    Ok(s) => s,
                    Err(e) => e.into_inner(),
                };
                store.claim_next_job()
            };
            let Some(job) = job else {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            let id = job.id.clone();
            tracing::info!(job = %id, kind = %job.kind, hash = %job.info_hash, "job started");
            // Off the async runtime: this is minutes of blocking file I/O, and
            // leaving it on a worker thread would stall every other task.
            let st = state.clone();
            let outcome = tokio::task::spawn_blocking(move || run_job(&st, &job))
                .await
                .unwrap_or_else(|e| Err(format!("job panicked: {e}")));
            let err = match &outcome {
                Ok(()) => String::new(),
                Err(e) => e.clone(),
            };
            {
                let store = match state.store.lock() {
                    Ok(s) => s,
                    Err(e) => e.into_inner(),
                };
                let _ = store.job_finish(&id, &err);
            }
            if err.is_empty() {
                tracing::info!(job = %id, "job done");
            } else {
                tracing::warn!(job = %id, error = %err, "job failed");
            }
        }
    });
}

pub(crate) fn run_job(state: &AppState, job: &crate::store::Job) -> Result<(), String> {
    match job.kind.as_str() {
        "graduate" => graduate(state, job),
        "move_data" => move_data(state, job),
        other => Err(format!("unknown job type {other}")),
    }
}

/// Each file of a graduating torrent: where it is, where it goes.
fn graduation_files(
    meta: &typhon_engine::torrent::meta::TorrentMeta,
    old_root: &std::path::Path,
    dest_root: &str,
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    meta.files
        .iter()
        .map(|f| {
            let rel: std::path::PathBuf = if meta.multi_file {
                std::path::Path::new(&meta.name).join(&f.path)
            } else {
                std::path::PathBuf::from(&f.path)
            };
            (old_root.join(&rel), std::path::Path::new(dest_root).join(&rel))
        })
        .collect()
}

#[cfg(test)]
thread_local! {
    /// Ends the next graduation or move on this thread right after the
    /// torrent left its engine, with nothing undone: what a kill or a power
    /// cut leaves behind.
    static STOP_AFTER_LEAVING_SOURCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Move a torrent and its data to another engine's storage, keeping it seeding.
///
/// The existing `POST /api/torrents/:hash/engine` reassigns an engine WITHOUT
/// touching the files -- deliberately, it says so. Graduation is the other
/// case: the whole point is to get the bytes off the race disk.
fn graduate(state: &AppState, job: &crate::store::Job) -> Result<(), String> {
    let p: serde_json::Value = serde_json::from_str(&job.params).map_err(|e| e.to_string())?;
    let from = p.get("from_engine").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let to = p.get("to_engine").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let category = p.get("to_category").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let dest_root = p.get("save_path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let allow = p.get("allow_breaking_hardlinks").and_then(|v| v.as_bool()).unwrap_or(false);
    if to.is_empty() || dest_root.is_empty() {
        return Err("graduation needs a target engine and a save path".into());
    }
    let hash = job.info_hash.clone();

    let src = state.engines.get(&from).ok_or("source engine is gone")?;
    let ih = typhon_engine::torrent::hex_decode(&hash).map_err(|e| e)?;
    let dst = state.engines.get(&to).ok_or("target engine is gone")?;
    // The metainfo must be reachable before anything moves: the target
    // adopts by reading it from the store.
    let metainfo = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.torrent_blob(&hash).ok().flatten()
    }
    .ok_or("no metainfo in the store; nothing was moved")?;

    // ⭐ Between leaving its source engine and reaching the target, the
    // torrent is in neither, for as long as its files take to copy. A process
    // stopped in that window -- a kill, an OOM, a power cut -- used to come
    // back to a job that failed on "the source engine no longer holds it",
    // with the files split over two disks and the record gone. The record now
    // rides in the job, written before the torrent leaves, and a job run again
    // finishes the move from it.
    let carried: Option<typhon_engine::torrent::fastresume::ResumeData> =
        p.get("record").and_then(|v| serde_json::from_value(v.clone()).ok());
    let (original, old_root, files, resumed) = match src.manager.get(&ih) {
        Some(t) => {
            // The same checks a move inside an engine makes, before anything
            // stops: 4.4 renamed each file onto its destination unseen, so the
            // drain could overwrite another torrent's file of the same name in
            // the hoard folder.
            let plan = plan_move_checked(state, &t, std::path::Path::new(&dest_root));
            if let Some((_, why)) = plan.refusal() {
                return Err(format!("{why}; nothing was moved"));
            }
            // The torrent's own resume record, captured BEFORE it leaves the
            // engine: pieces, edited trackers, byte counters, seed time,
            // stopped state. 4.3 re-added the .torrent in seed mode instead,
            // which marked incomplete data complete, dropped tracker edits,
            // reset the counters and restarted a stopped torrent.
            let original = src
                .manager
                .export_state(&ih)
                .ok_or("the source engine no longer holds it")?;
            let old_root = t.save_path.read().clone();
            let files = graduation_files(&t.meta, &old_root, &dest_root);
            let mut carrying = p.clone();
            carrying["record"] = serde_json::to_value(&original).map_err(|e| e.to_string())?;
            {
                let store = match state.store.lock() {
                    Ok(s) => s,
                    Err(e) => e.into_inner(),
                };
                store
                    .set_job_params(&job.id, &carrying.to_string())
                    .map_err(|e| format!("cannot write down the torrent's record: {e}; nothing was moved"))?;
            }
            (original, old_root, files, false)
        }
        None => {
            let Some(original) = carried else {
                return Err("the source engine no longer holds it".into());
            };
            let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&metainfo)?;
            let old_root = std::path::PathBuf::from(&original.save_path);
            let files = graduation_files(&meta, &old_root, &dest_root);
            tracing::warn!(hash = %hash, from = %from, to = %to,
                           "graduation interrupted after leaving its engine; finishing it");
            (original, old_root, files, true)
        }
    };
    let seeded = original.seed_secs;

    if !resumed {
        // Out of the engine first, KEEPING the data: moving files under a
        // running torrent is how a seed starts serving bytes that are no
        // longer there.
        src.manager
            .remove_torrent(&ih, true)
            .map_err(|e| format!("the source engine refused to release it: {e}"))?;
        src.announce_cache.forget(&hash);
    }
    #[cfg(test)]
    if STOP_AFTER_LEAVING_SOURCE.with(|c| c.replace(false)) {
        return Err("stopped where a crash would".into());
    }

    // From here, any failure puts the files back and the torrent back in its
    // source engine, as it was. 4.3 had no way back: an error after the
    // removal left the torrent in no engine.
    let put_back = |moved: &[(std::path::PathBuf, std::path::PathBuf)], why: String| -> String {
        let mut problems = Vec::new();
        for (from_path, to_path) in moved.iter().rev() {
            if let Err(e) = crate::jobs::run_move_allowing(to_path, from_path, true) {
                problems.push(format!("{}: {e}", to_path.display()));
            }
        }
        match src.manager.import_state_bytes(&original, &metainfo) {
            Ok(_) if problems.is_empty() => format!("{why}; rolled back, the torrent is in {from} as before"),
            Ok(_) => format!("{why}; back in {from}, but these files did not move back: {}", problems.join("; ")),
            Err(e) => format!("{why}; ROLLBACK FAILED, re-add it by hand at {}: {e}", old_root.display()),
        }
    };

    let mut done: i64 = 0;
    let mut moved: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for (from_path, to_path) in &files {
        if !from_path.exists() {
            // Moved before the interruption: it goes back with the rest if
            // this run has to undo.
            if resumed && to_path.exists() {
                moved.push((from_path.clone(), to_path.clone()));
            }
            continue;
        }
        // A destination that appeared while the job was stopped is not ours
        // to overwrite: give the move up and put everything back.
        // A graduation copies in place after the torrent left its engine,
        // so a copy of ours can be cut short: a prefix of the original.
        if resumed && to_path.exists() && !is_our_copy(from_path, to_path, false) {
            return Err(put_back(&moved, format!("{} already holds another file", to_path.display())));
        }
        let size = std::fs::metadata(from_path).map(|m| m.len()).unwrap_or(0) as i64;
        // A cancel is honoured between two files, and undone like a failure.
        let cancelling = {
            let store = match state.store.lock() {
                Ok(s) => s,
                Err(e) => e.into_inner(),
            };
            store.job_cancelling(&job.id)
        };
        if cancelling {
            return Err(put_back(&moved, "cancelled".to_string()));
        }
        if let Err(e) = crate::jobs::run_move_allowing(from_path, to_path, allow) {
            return Err(put_back(&moved, format!("moving {}: {e}", from_path.display())));
        }
        moved.push((from_path.clone(), to_path.clone()));
        done += size;
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.job_progress(&job.id, done);
    }

    let mut record = original.clone();
    record.save_path = dest_root.clone();
    // Interrupted after the target adopted it: only the store is left to do.
    let adopted = resumed && dst.manager.get(&ih).is_some();
    if !adopted {
        if let Err(e) = dst.manager.import_state_bytes(&record, &metainfo) {
            return Err(put_back(&moved, format!("the target engine refused it: {e}")));
        }
    }
    if dst.manager.get(&ih).is_none() {
        return Err(put_back(&moved, "the target engine accepted it and does not hold it".into()));
    }
    {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.set_session(&hash, &from, &to);
        // The payload MOVED. Forgetting this leaves the row pointing at the
        // directory the bytes left.
        let _ = store.set_save_path(&hash, &dest_root);
        if !category.is_empty() {
            // The row moved to `to` a few lines up; the category belongs to that
            // copy and to no other.
            let _ = store.set_category_in(&hash, &to, &category);
        }
    }
    {
        // Done: the job no longer needs its copy of the record.
        let mut finished = p.clone();
        if let Some(m) = finished.as_object_mut() {
            m.remove("record");
        }
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.set_job_params(&job.id, &finished.to_string());
    }
    tracing::info!(hash = %hash, from = %from, to = %to, moved_bytes = done, seed_secs = seeded,
                   "graduated");
    Ok(())
}

// ---------------------------------------------------------------------------
// Moving a torrent's data inside its engine
// ---------------------------------------------------------------------------

/// One file of a planned move.
#[derive(Debug, Clone)]
pub struct MoveFile {
    pub from: std::path::PathBuf,
    pub to: std::path::PathBuf,
    pub size: u64,
    /// On disk at the source. A file never downloaded has nothing to move.
    pub present: bool,
    /// Same filesystem: a rename, instant, and a hardlink survives it.
    pub rename: bool,
    /// Needs a copy AND has another name on disk: the copy breaks the link.
    pub hardlinked: bool,
}

/// Everything a move will do, worked out before anything is touched.
#[derive(Debug, Clone)]
pub struct MovePlan {
    pub old_root: std::path::PathBuf,
    pub new_root: std::path::PathBuf,
    pub files: Vec<MoveFile>,
    /// A path from the metainfo that is not a plain relative path (`..`, an
    /// absolute component). Joined onto a root it points OUTSIDE it, so a
    /// move would rename or delete a file that is not this torrent's.
    pub unsafe_path: Option<String>,
    /// Another torrent reads one of these same files. Renaming it away would
    /// leave that torrent serving bytes that are no longer there.
    pub shared_with: Vec<String>,
    /// Files already at the destination. `rename(2)` replaces its target
    /// without a word, so moving onto them would destroy whatever they are --
    /// another torrent's file, most likely, under the same name.
    pub occupied: Vec<String>,
}

impl MovePlan {
    pub fn is_noop(&self) -> bool {
        self.old_root == self.new_root
    }
    /// Why this move must not run at all, whatever the operator agrees to.
    pub fn refusal(&self) -> Option<(&'static str, String)> {
        if let Some(p) = &self.unsafe_path {
            return Some(("unsafe_path", format!("the torrent names a file outside its folder ({p}); refusing to move it")));
        }
        if !self.shared_with.is_empty() {
            return Some(("shared", format!(
                "{} other torrent(s) read the same files ({}); moving them would leave those serving nothing",
                self.shared_with.len(),
                self.shared_with.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        if !self.occupied.is_empty() {
            return Some(("destination_taken", format!(
                "{} file(s) already exist at the destination ({}); moving onto them would overwrite them",
                self.occupied.len(),
                self.occupied.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        None
    }
    fn present(&self) -> impl Iterator<Item = &MoveFile> {
        self.files.iter().filter(|f| f.present)
    }
    pub fn copy_bytes(&self) -> u64 {
        self.present().filter(|f| !f.rename).map(|f| f.size).sum()
    }
    pub fn hardlinked(&self) -> (usize, u64) {
        let h: Vec<&MoveFile> = self.present().filter(|f| f.hardlinked).collect();
        (h.len(), h.iter().map(|f| f.size).sum())
    }
    /// What the preview route and a refusal both show.
    pub fn summary(&self) -> serde_json::Value {
        let (hl_files, hl_bytes) = self.hardlinked();
        let free = crate::jobs::free_space_near(&self.new_root);
        let copy = self.copy_bytes();
        serde_json::json!({
            "from": self.old_root.display().to_string(),
            "to": self.new_root.display().to_string(),
            "files": self.files.len(),
            "bytes": self.files.iter().map(|f| f.size).sum::<u64>(),
            "missing_files": self.files.iter().filter(|f| !f.present).count(),
            "rename_files": self.present().filter(|f| f.rename).count(),
            "copy_files": self.present().filter(|f| !f.rename).count(),
            "copy_bytes": copy,
            "hardlinked_files": hl_files,
            "hardlinked_bytes": hl_bytes,
            "free_bytes": free,
            "enough_space": free.map(|f| f >= copy).unwrap_or(true),
            "unsafe_path": self.unsafe_path,
            "shared_with": self.shared_with,
            "occupied": self.occupied,
        })
    }
}

/// The paths of `t`'s files relative to its root, as the engine builds them.
fn rel_paths(t: &typhon_engine::torrent::meta::TorrentState) -> Vec<std::path::PathBuf> {
    let multi = t.meta.multi_file;
    t.meta
        .files
        .iter()
        .map(|f| {
            if multi {
                std::path::Path::new(&t.meta.name).join(&f.path)
            } else {
                f.path.clone()
            }
        })
        .collect()
}

/// Above this many torrents the shared-file scan is split across threads.
#[cfg(not(test))]
const SHARED_SCAN_PARALLEL_FROM: usize = 20_000;
/// Zero under test, so the move tests go through the slices and their merge.
#[cfg(test)]
const SHARED_SCAN_PARALLEL_FROM: usize = 0;

/// The first component every one of `t`'s paths starts with, relative to its
/// save path: its name for a multi-file torrent, its file for a single one.
/// None when that is not one thing (a multi-file torrent with no name).
fn top_entry(t: &typhon_engine::torrent::meta::TorrentState) -> Option<&std::ffi::OsStr> {
    if t.meta.multi_file {
        if t.meta.name.is_empty() {
            None
        } else {
            std::path::Path::new(&t.meta.name).components().next().map(|c| c.as_os_str())
        }
    } else {
        t.meta.files.first().and_then(|f| f.path.components().next()).map(|c| c.as_os_str())
    }
}

/// Only plain names: no `..`, no root, no prefix, no `.`.
fn is_plain_relative(p: &std::path::Path) -> bool {
    p.components().count() > 0
        && p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Plan moving `t`'s files to `new_root`, and find the torrents of any engine
/// that read one of the same files.
///
/// Every operation of a move is PER FILE, on the files this torrent names:
/// nothing is copied or deleted by directory. A single file sitting directly
/// in a category folder among other torrents' files moves alone; the folder,
/// and everything else in it, stays.
pub fn plan_move_checked(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
    new_root: &std::path::Path,
) -> MovePlan {
    let mut plan = plan_move(t, new_root);
    if plan.unsafe_path.is_some() {
        return plan;
    }
    let mine: std::collections::HashSet<std::path::PathBuf> =
        plan.files.iter().map(|f| f.from.clone()).collect();
    // This torrent's copy in another engine counts: seeded from the same path
    // (POST /copy), it would be left serving files moved out from under it.
    let (hashes, _) = scan_shared(state, t, &plan.old_root, &mine, false);
    plan.shared_with = hashes;
    plan
}

/// The files of `t` that another torrent -- in any engine -- reads too.
///
/// What a deletion must leave on disk: two torrents pointed at one path (a
/// cross-seed, an upload seeded from the files it was made from) read the same
/// bytes, and deleting them with the first torrent leaves the other one
/// announcing an empty folder, complete by its own account.
///
/// A copy of `t` in another engine counts as another torrent: it reads the
/// same files only if it points at the same path. One seeded from a payload of
/// its own (a race grab the *arr also fetched into the hoard) reads none of
/// these, and keeping them for it is how /race filled with orphans.
pub fn files_read_by_others(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
) -> std::collections::HashSet<std::path::PathBuf> {
    let root = t.save_path.read().clone();
    let mine: std::collections::HashSet<std::path::PathBuf> =
        rel_paths(t).into_iter().map(|p| root.join(p)).collect();
    scan_shared(state, t, &root, &mine, false).1
}

/// `files_read_by_others`, not counting the copies of `t` in other engines:
/// for a removal that takes every copy at once.
pub fn files_read_by_other_hashes(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
) -> std::collections::HashSet<std::path::PathBuf> {
    let root = t.save_path.read().clone();
    let mine: std::collections::HashSet<std::path::PathBuf> =
        rel_paths(t).into_iter().map(|p| root.join(p)).collect();
    scan_shared(state, t, &root, &mine, true).1
}

/// Every other torrent that reads one of `mine` (files of `t`, under `root`),
/// and which of those files they read. `skip_copies` leaves out the copies of
/// `t` held by other engines; `t` itself is always left out.
fn scan_shared(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
    root: &std::path::Path,
    mine: &std::collections::HashSet<std::path::PathBuf>,
    skip_copies: bool,
) -> (Vec<String>, std::collections::HashSet<std::path::PathBuf>) {
    let skip = |other: &typhon_engine::torrent::meta::TorrentState| {
        std::ptr::eq(other, t) || (skip_copies && other.meta.info_hash == t.meta.info_hash)
    };
    // Compared by `path_key` (case-folded on Windows), answered with the
    // caller's own spelling of each path: the caller looks its files up in
    // what comes back.
    let by_key: std::collections::HashMap<std::path::PathBuf, &std::path::PathBuf> =
        mine.iter().map(|p| (crate::pathclaim::path_key(p), p)).collect();
    let keys: std::collections::HashSet<std::path::PathBuf> = by_key.keys().cloned().collect();
    let mut hashes: Vec<String> = Vec::new();
    let mut files: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    for (other, hit) in scan_claims(state, root, top_entry(t), &keys, &skip) {
        let h = typhon_engine::torrent::hex_encode(&other.meta.info_hash);
        if !hashes.contains(&h) {
            hashes.push(h);
        }
        files.extend(
            hit.into_iter()
                .filter_map(|(p, _)| by_key.get(&crate::pathclaim::path_key(&p)).map(|m| (*m).clone())),
        );
    }
    (hashes, files)
}

/// Every torrent of every engine that names one of `mine` -- absolute paths
/// under the save path `root`, whose first component under it is `my_top` --
/// with each file it names and the size IT expects there. `skip` leaves
/// torrents out.
///
/// The same lookup serves a move, a deletion, and an add checking whether its
/// paths are already somebody's (`pathclaim`). On Windows `mine` holds
/// `pathclaim::path_key`s, so a name differing only by case still matches.
///
/// The candidates come from the store's `save_path` index rather than a walk
/// of every engine: the walk cost ~190 ms per add at a million torrents,
/// measured on a copy of production. A torrent can only name one of our
/// files if its save path is an ancestor of our root (or our root itself) or
/// lies under it -- and that holds whichever way the store spells the path:
/// for most torrents the engine's save path, for an Internet Archive item its
/// root. Each candidate is then checked against the engine's live paths.
/// `scan_claims_full` is the walk, kept under test to hold the two to the
/// same answer.
pub fn scan_claims(
    state: &AppState,
    root: &std::path::Path,
    my_top: Option<&std::ffi::OsStr>,
    mine: &std::collections::HashSet<std::path::PathBuf>,
    skip: &(dyn Fn(&typhon_engine::torrent::meta::TorrentState) -> bool + Sync),
) -> Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, Vec<(std::path::PathBuf, u64)>)> {
    let my_root = match my_top {
        Some(t) => root.join(t),
        None => root.to_path_buf(),
    };
    let exact: Vec<String> = my_root.ancestors().map(|a| a.to_string_lossy().into_owned()).collect();
    let rows = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.rows_by_save_path(&exact, &my_root.to_string_lossy(), std::path::MAIN_SEPARATOR)
    };
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            // A store that cannot answer must not read as "nobody holds these
            // files": that is the overwrite this check exists for.
            tracing::warn!("path claim: store lookup failed ({e}), walking every torrent instead");
            return scan_claims_walk(state, root, my_top, mine, skip);
        }
    };
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<(String, [u8; 20])> = std::collections::HashSet::new();
    for (hash, session) in rows {
        let Some(engine) = state.engines.get(&session) else { continue };
        let Some(ih) = crate::store::hex20(&hash) else { continue };
        let Some(other) = engine.manager.get(&ih) else { continue };
        if !seen.insert((session, ih)) {
            continue;
        }
        if let Some(hit) = claim_of(&other, root, my_top, mine, skip) {
            out.push((other, hit));
        }
    }
    out
}

/// The files of `mine` that `other` names, with the size it expects, or None.
/// `root`/`my_top` as in `scan_claims`: a prefix test first, the file list
/// only for a torrent on the same branch.
fn claim_of(
    other: &typhon_engine::torrent::meta::TorrentState,
    root: &std::path::Path,
    my_top: Option<&std::ffi::OsStr>,
    mine: &std::collections::HashSet<std::path::PathBuf>,
    skip: &(dyn Fn(&typhon_engine::torrent::meta::TorrentState) -> bool + Sync),
) -> Option<Vec<(std::path::PathBuf, u64)>> {
    if skip(other) {
        return None;
    }
    // Only a torrent whose save path is on the same branch can name the same
    // files: a prefix test first, the file list only then. Read in place: a
    // PathBuf clone per torrent would be an allocation for the ones that fail.
    let r = {
        let r = other.save_path.read();
        if !(root.starts_with(&*r) || r.starts_with(root)) {
            return None;
        }
        // Same folder, different top-level entry: no file can be shared, since
        // every path of each starts with its own entry.
        if *r == *root {
            if let (Some(a), Some(b)) = (my_top, top_entry(other)) {
                if a != b {
                    return None;
                }
            }
        }
        r.clone()
    };
    let hit: Vec<(std::path::PathBuf, u64)> = rel_paths(other)
        .into_iter()
        .zip(other.meta.files.iter().map(|f| f.length))
        .map(|(p, len)| (r.join(p), len))
        .filter(|(p, _)| mine.contains(&crate::pathclaim::path_key(p)))
        .collect();
    if hit.is_empty() { None } else { Some(hit) }
}

/// `scan_claims` by walking every torrent of every engine: the reference the
/// tests hold the lookup to.
#[cfg(test)]
pub fn scan_claims_full(
    state: &AppState,
    root: &std::path::Path,
    my_top: Option<&std::ffi::OsStr>,
    mine: &std::collections::HashSet<std::path::PathBuf>,
    skip: &(dyn Fn(&typhon_engine::torrent::meta::TorrentState) -> bool + Sync),
) -> Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, Vec<(std::path::PathBuf, u64)>)> {
    scan_claims_walk(state, root, my_top, mine, skip)
}

/// Every torrent of every engine, checked one by one: what `scan_claims`
/// falls back on when the store cannot answer.
fn scan_claims_walk(
    state: &AppState,
    root: &std::path::Path,
    my_top: Option<&std::ffi::OsStr>,
    mine: &std::collections::HashSet<std::path::PathBuf>,
    skip: &(dyn Fn(&typhon_engine::torrent::meta::TorrentState) -> bool + Sync),
) -> Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, Vec<(std::path::PathBuf, u64)>)> {
    let shared_by = |other: &typhon_engine::torrent::meta::TorrentState| claim_of(other, root, my_top, mine, skip);
    let pair = |o: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>| {
        shared_by(o).map(|files| (o.clone(), files))
    };
    let mut out: Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, Vec<(std::path::PathBuf, u64)>)> = Vec::new();
    // Over every torrent of every engine, in slices on their own threads: the
    // prefix test alone is a second per move at a million torrents, and a
    // move of a hundred torrents runs it a hundred times.
    for engine in state.engines.engines().iter() {
        let all = engine.manager.all();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 16);
        let found: Vec<Vec<(Arc<typhon_engine::torrent::meta::TorrentState>, Vec<(std::path::PathBuf, u64)>)>> = if threads == 1 || all.len() < SHARED_SCAN_PARALLEL_FROM {
            vec![all.iter().filter_map(pair).collect()]
        } else {
            let per = all.len().div_ceil(threads).max(1);
            std::thread::scope(|sc| {
                let pair = &pair;
                let handles: Vec<_> = all
                    .chunks(per)
                    .map(|slice| sc.spawn(move || slice.iter().filter_map(pair).collect::<Vec<_>>()))
                    .collect();
                handles.into_iter().map(|h| h.join().expect("shared-file scan slice")).collect()
            })
        };
        out.extend(found.into_iter().flatten());
    }
    out
}

/// Plan moving `t`'s files from where it reads them to `new_root`.
pub fn plan_move(
    t: &typhon_engine::torrent::meta::TorrentState,
    new_root: &std::path::Path,
) -> MovePlan {
    let old_root = t.save_path.read().clone();
    let unsafe_path = rel_paths(t)
        .into_iter()
        .find(|p| !is_plain_relative(p))
        .map(|p| p.display().to_string());
    let multi = t.meta.multi_file;
    let name = t.meta.name.clone();
    let files: Vec<MoveFile> = t
        .meta
        .files
        .iter()
        .map(|f| {
            let rel: std::path::PathBuf = if multi {
                std::path::Path::new(&name).join(&f.path)
            } else {
                f.path.clone()
            };
            let from = old_root.join(&rel);
            let to = new_root.join(&rel);
            let meta = std::fs::metadata(&from).ok();
            let present = meta.is_some();
            let rename = present && crate::jobs::same_filesystem(&from, &to);
            let hardlinked = present && !rename && crate::jobs::link_count(&from) > 1;
            MoveFile {
                size: meta.map(|m| m.len()).unwrap_or(f.length),
                from,
                to,
                present,
                rename,
                hardlinked,
            }
        })
        .collect();
    let occupied = files
        .iter()
        .filter(|f| f.present && f.from != f.to && f.to.exists() && !same_file(&f.from, &f.to))
        .map(|f| f.to.display().to_string())
        .collect();
    MovePlan { old_root, new_root: new_root.to_path_buf(), files, unsafe_path, shared_with: Vec::new(), occupied }
}

/// Two names of one file: renaming one onto the other loses nothing.
fn same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
    }
}

/// Queue moving a torrent's data to `save_path` without leaving its engine.
pub fn queue_move(
    state: &AppState,
    hash: &str,
    name: &str,
    engine: &str,
    category: &str,
    save_path: &str,
    allow_breaking_hardlinks: bool,
    total_bytes: i64,
) -> Option<String> {
    let params = serde_json::json!({
        "name": name,
        "target": save_path,
        "engine": engine,
        "category": category,
        "save_path": save_path,
        "allow_breaking_hardlinks": allow_breaking_hardlinks,
    })
    .to_string();
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    // One move at a time per torrent, whichever kind: a graduation and a move
    // of the same files racing each other is how both end up half-done.
    if store.job_pending_for("move_data", hash) || store.job_pending_for("graduate", hash) {
        return None;
    }
    store.create_job("move_data", hash, &params, total_bytes).ok()
}

/// Put a torrent back where it was after a move that could not finish.
///
/// Undo the renames, drop the copies, re-add at the old root. Every step is
/// attempted even if one fails: a partial undo still leaves less to repair by
/// hand than none.
fn roll_back(
    engine: &crate::engines::Engine,
    metainfo: &[u8],
    original: &typhon_engine::torrent::fastresume::ResumeData,
    renamed: &[(std::path::PathBuf, std::path::PathBuf)],
    copied: &[std::path::PathBuf],
) -> String {
    let old_root = std::path::Path::new(&original.save_path);
    let mut problems = Vec::new();
    for (from, to) in renamed.iter().rev() {
        if let Err(e) = std::fs::rename(to, from) {
            problems.push(format!("could not put {} back: {e}", from.display()));
        }
    }
    for c in copied {
        let _ = std::fs::remove_file(c);
    }
    if let Err(e) = engine.manager.import_state_bytes(original, metainfo) {
        problems.push(format!("could not re-add it at {}: {e}", old_root.display()));
    }
    if problems.is_empty() {
        "rolled back: the torrent is where it was".into()
    } else {
        format!("ROLLBACK INCOMPLETE: {}", problems.join("; "))
    }
}

/// Move a torrent's data to another directory of the SAME engine.
///
/// The Go daemon did this for "Move to category"; the Rust port answered the
/// same request with a relabel and a 200, so for weeks a move changed the
/// label and left every byte where it was.
///
/// Order matters, and it is chosen so the torrent never serves bytes that are
/// not there:
///  1. files on ANOTHER filesystem are copied while the torrent keeps seeding
///     from the originals -- that is the long part, and nothing stops for it;
///  2. the torrent leaves the engine (data kept), same-filesystem files are
///     renamed, and its own resume record is re-imported at the new root --
///     pieces, edited trackers, counters, seed time and stopped state with it.
///     4.4 re-added the .torrent in seed mode instead, which marked an
///     incomplete torrent complete and dropped every tracker edit;
///  3. only then are the originals of the copied files deleted.
/// A failure in 1 deletes the copies; a failure in 2 undoes the renames and
/// re-adds the torrent where it was. Nothing is ever deleted before the new
/// copy is the one being served.
fn move_data(state: &AppState, job: &crate::store::Job) -> Result<(), String> {
    let p: serde_json::Value = serde_json::from_str(&job.params).map_err(|e| e.to_string())?;
    let s = |k: &str| p.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let (engine_id, category, save_path) = (s("engine"), s("category"), s("save_path"));
    let allow = p.get("allow_breaking_hardlinks").and_then(|v| v.as_bool()).unwrap_or(false);
    if engine_id.is_empty() || save_path.is_empty() {
        return Err("a move needs an engine and a save path".into());
    }
    let hash = job.info_hash.clone();
    let engine = state.engines.get(&engine_id).ok_or("the engine is gone")?;
    let ih = typhon_engine::torrent::hex_decode(&hash)?;
    let Some(t) = engine.manager.get(&ih) else {
        return finish_interrupted_move(state, job, engine, &p, &save_path, &category, allow);
    };
    let plan = plan_move_checked(state, &t, std::path::Path::new(&save_path));
    if let Some((_, why)) = plan.refusal() {
        return Err(format!("{why}; nothing was moved"));
    }

    let set_labels = |root: Option<&str>| {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        if let Some(r) = root {
            let _ = store.set_save_path(&hash, r);
        }
        if !category.is_empty() {
            let _ = store.set_category_in(&hash, &engine_id, &category);
        }
    };
    if plan.is_noop() {
        set_labels(None);
        return Ok(());
    }
    // Checked again here, not only when the request came in: a file can gain
    // a second name between the two, and the answer given then is stale now.
    let (hl, _) = plan.hardlinked();
    if hl > 0 && !allow {
        return Err(format!(
            "{hl} file(s) are hardlinked elsewhere and the target is on another filesystem; \
             nothing was moved (allow_breaking_hardlinks was not given)"
        ));
    }
    let need = plan.copy_bytes();
    if let Some(free) = crate::jobs::free_space_near(&plan.new_root) {
        if free < need {
            return Err(format!("{need} bytes to copy, {free} free on the target; nothing was moved"));
        }
    }
    // Without the metainfo nothing could re-add the torrent: find out now,
    // while stopping is still optional.
    let metainfo = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.torrent_blob(&hash).ok().flatten()
    }
    .ok_or("no metainfo in the store to re-add it with; nothing was moved")?;

    let progress = |done: i64| {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.job_progress(&job.id, done);
    };

    // 1. Copies, while it seeds.
    let mut copied: Vec<std::path::PathBuf> = Vec::new();
    let mut done: i64 = 0;
    for f in plan.files.iter().filter(|f| f.present && !f.rename) {
        if let Err(e) = crate::jobs::copy_only(&f.from, &f.to) {
            for c in &copied {
                let _ = std::fs::remove_file(c);
            }
            let _ = std::fs::remove_file(&f.to);
            return Err(format!("copying {}: {e}; the copies were removed, nothing moved", f.from.display()));
        }
        copied.push(f.to.clone());
        done += f.size as i64;
        progress(done);
    }

    // 2. Stop, rename, re-import. The record is taken now, after the copies,
    // so it holds every piece fetched while they ran.
    drop(t);
    let original = match engine.manager.export_state(&ih) {
        Some(rd) => rd,
        None => {
            for c in &copied {
                let _ = std::fs::remove_file(c);
            }
            return Err("the engine no longer holds it; the copies were removed, nothing moved".into());
        }
    };
    // Written down before it leaves the engine: a process stopped between
    // here and the re-import finishes the move from it when the job runs
    // again, instead of failing on a torrent no engine holds.
    if let Err(e) = set_carried_record(state, job, &p, Some(&original)) {
        for c in &copied {
            let _ = std::fs::remove_file(c);
        }
        return Err(format!("cannot write down the torrent's record: {e}; the copies were removed, nothing moved"));
    }
    if let Err(e) = engine.manager.remove_torrent(&ih, true) {
        for c in &copied {
            let _ = std::fs::remove_file(c);
        }
        return Err(format!("the engine refused to release it: {e}; nothing moved"));
    }
    engine.announce_cache.forget(&hash);
    #[cfg(test)]
    if STOP_AFTER_LEAVING_SOURCE.with(|c| c.replace(false)) {
        return Err("stopped where a crash would".into());
    }

    let mut renamed: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for f in plan.files.iter().filter(|f| f.present && f.rename) {
        let r = f
            .to
            .parent()
            .map(std::fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|_| std::fs::rename(&f.from, &f.to));
        if let Err(e) = r {
            let undo = roll_back(engine, &metainfo, &original, &renamed, &copied);
            return Err(format!("renaming {}: {e}; {undo}", f.from.display()));
        }
        renamed.push((f.from.clone(), f.to.clone()));
        done += f.size as i64;
        progress(done);
    }

    let new_root = plan.new_root.to_string_lossy().to_string();
    let mut record = original.clone();
    record.save_path = new_root.clone();
    // `import_state_bytes` refuses a metainfo that is not this torrent's.
    if let Err(e) = engine.manager.import_state_bytes(&record, &metainfo) {
        let undo = roll_back(engine, &metainfo, &original, &renamed, &copied);
        return Err(format!("the engine refused it at {new_root}: {e}; {undo}"));
    }
    set_labels(Some(&new_root));

    // 3. The originals of what was copied. The torrent reads the new ones now.
    for f in plan.files.iter().filter(|f| f.present && !f.rename) {
        if let Err(e) = std::fs::remove_file(&f.from) {
            tracing::warn!(file = %f.from.display(), error = %e, "moved, but the original could not be removed");
        }
    }
    prune_empty_dirs(&plan);
    let _ = set_carried_record(state, job, &p, None);
    tracing::info!(hash = %hash, engine = %engine_id, from = %plan.old_root.display(),
                   to = %new_root, copied_bytes = need, "moved");
    Ok(())
}

/// Store the job's params with `record` carried in them, or without it.
fn set_carried_record(
    state: &AppState,
    job: &crate::store::Job,
    p: &serde_json::Value,
    record: Option<&typhon_engine::torrent::fastresume::ResumeData>,
) -> Result<(), String> {
    let mut params = p.clone();
    let Some(m) = params.as_object_mut() else {
        return Err("job params are not an object".into());
    };
    match record {
        Some(rd) => {
            m.insert("record".into(), serde_json::to_value(rd).map_err(|e| e.to_string())?);
        }
        None => {
            m.remove("record");
        }
    }
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    store.set_job_params(&job.id, &params.to_string()).map_err(|e| e.to_string())
}

/// A move stopped after its torrent left the engine: put each file where
/// the move was taking it, hand the torrent back to the engine there, then
/// drop the originals of what had been copied.
fn finish_interrupted_move(
    state: &AppState,
    job: &crate::store::Job,
    engine: &crate::engines::Engine,
    p: &serde_json::Value,
    save_path: &str,
    category: &str,
    allow: bool,
) -> Result<(), String> {
    let original: typhon_engine::torrent::fastresume::ResumeData = p
        .get("record")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .ok_or("the engine no longer holds it")?;
    let hash = job.info_hash.clone();
    let ih = typhon_engine::torrent::hex_decode(&hash)?;
    let metainfo = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.torrent_blob(&hash).ok().flatten()
    }
    .ok_or("the engine no longer holds it, and there is no metainfo to re-add it with")?;
    let meta = typhon_engine::torrent::metainfo::parse_torrent_bytes(&metainfo)?;
    let old_root = std::path::PathBuf::from(&original.save_path);
    let files = graduation_files(&meta, &old_root, save_path);
    tracing::warn!(hash = %hash, engine = %engine.id, "move interrupted after leaving its engine; finishing it");
    // ⭐ A file found at a destination while its original is still there is
    // either our copy or a file that appeared there while the job was
    // stopped. A move copies everything, synced, before the torrent leaves
    // its engine, so a copy of ours is always whole: an empty or shorter file
    // is somebody else's, and so is any other content. Only a whole copy lets
    // the original go; anything else gives the move up, with every file back
    // where it was and the torrent back in its engine there.
    if let Some((_, to)) = files
        .iter()
        .find(|(from, to)| from != to && from.exists() && to.exists() && !is_our_copy(from, to, true))
    {
        let undo = undo_interrupted_move(engine, &files, &original, &metainfo);
        return Err(format!("{} already holds another file; {undo}", to.display()));
    }
    for (from, to) in &files {
        if from != to && !to.exists() && from.exists() {
            crate::jobs::run_move_allowing(from, to, allow)
                .map_err(|e| format!("moving {}: {e}", from.display()))?;
        }
    }
    let mut record = original.clone();
    record.save_path = save_path.to_string();
    if engine.manager.get(&ih).is_none() {
        engine
            .manager
            .import_state_bytes(&record, &metainfo)
            .map_err(|e| format!("the engine refused it at {save_path}: {e}"))?;
    }
    // The originals of the files that had been copied: the torrent reads the
    // new ones now.
    for (from, to) in &files {
        if from != to && from.exists() && to.exists() {
            let _ = std::fs::remove_file(from);
        }
    }
    {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.set_save_path(&hash, save_path);
        if !category.is_empty() {
            let _ = store.set_category_in(&hash, &engine.id, category);
        }
    }
    let _ = set_carried_record(state, job, p, None);
    Ok(())
}

/// Is `to` our own copy of `from`: the bytes it holds are the first bytes of
/// `from`, and all of them when `whole`. Anything else at that path is
/// another file.
fn is_our_copy(from: &std::path::Path, to: &std::path::Path, whole: bool) -> bool {
    use std::io::Read;
    if same_file(from, to) {
        return true;
    }
    let (Ok(a), Ok(b)) = (std::fs::metadata(from), std::fs::metadata(to)) else {
        return false;
    };
    if b.len() > a.len() || (whole && b.len() != a.len()) {
        return false;
    }
    let (Ok(mut fa), Ok(mut fb)) = (std::fs::File::open(from), std::fs::File::open(to)) else {
        return false;
    };
    let mut left = b.len();
    let (mut ba, mut bb) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    while left > 0 {
        let n = left.min(ba.len() as u64) as usize;
        if fa.read_exact(&mut ba[..n]).is_err() || fb.read_exact(&mut bb[..n]).is_err() || ba[..n] != bb[..n] {
            return false;
        }
        left -= n as u64;
    }
    true
}

/// Give up an interrupted move: what had moved goes back, our copies go,
/// and the torrent returns to its engine where it was.
fn undo_interrupted_move(
    engine: &crate::engines::Engine,
    files: &[(std::path::PathBuf, std::path::PathBuf)],
    original: &typhon_engine::torrent::fastresume::ResumeData,
    metainfo: &[u8],
) -> String {
    let mut problems = Vec::new();
    for (from, to) in files {
        if from == to {
            continue;
        }
        if !from.exists() && to.exists() {
            if let Err(e) = crate::jobs::run_move_allowing(to, from, true) {
                problems.push(format!("could not put {} back: {e}", from.display()));
            }
        } else if from.exists() && to.exists() && is_our_copy(from, to, true) && !same_file(from, to) {
            let _ = std::fs::remove_file(to);
        }
    }
    if let Err(e) = engine.manager.import_state_bytes(original, metainfo) {
        problems.push(format!("could not re-add it at {}: {e}", original.save_path));
    }
    if problems.is_empty() {
        format!("the move was given up; the torrent is back at {}", original.save_path)
    } else {
        format!("ROLLBACK INCOMPLETE: {}", problems.join("; "))
    }
}

/// Remove the directories the move emptied, deepest first, never the old root
/// itself: that is a category directory other torrents may share.
fn prune_empty_dirs(plan: &MovePlan) {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for f in &plan.files {
        let mut cur = f.from.parent();
        while let Some(d) = cur {
            if d == plan.old_root || !d.starts_with(&plan.old_root) {
                break;
            }
            if !dirs.iter().any(|x| x == d) {
                dirs.push(d.to_path_buf());
            }
            cur = d.parent();
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = std::fs::remove_dir(&d);
    }
}

#[cfg(test)]
mod graduate_tests {
    use super::*;
    use crate::api::testing::{state_from, TestState};

    fn torrent_bytes(name: &str) -> Vec<u8> {
        let info = format!(
            "d6:lengthi16384e4:name{}:{name}12:piece lengthi16384e6:pieces20:{}e",
            name.len(),
            "G".repeat(20)
        );
        let announce = "https://tracker.example/announce";
        format!("d8:announce{}:{announce}4:info{info}e", announce.len()).into_bytes()
    }

    fn setup(tag: &str) -> (TestState, String, std::path::PathBuf, std::path::PathBuf) {
        let s = state_from(tag, "");
        let src = s.dir.join("race-data");
        let dst = s.dir.join("hoard-data");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("grad"), vec![7u8; 16384]).unwrap();
        let (hash, _) = crate::api::add_torrent_bytes(
            &s.state, &torrent_bytes("grad"), "", &src.display().to_string(), "", true, true, "race",
        )
        .expect("added");
        (s, hash, src, dst)
    }

    #[test]
    fn a_move_onto_a_file_that_is_already_there_is_refused() {
        // rename(2) replaces its target silently: moving onto another
        // torrent's file of the same name would destroy it.
        let (s, hash, src, dst) = setup("move-occupied");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("grad"), b"somebody else's grad").unwrap();
        let e = s.state.engines.get("race").unwrap();
        let t = e.manager.get(&crate::store::hex20(&hash).unwrap()).unwrap();
        let plan = plan_move(&t, &dst);
        assert_eq!(plan.refusal().map(|r| r.0), Some("destination_taken"));

        // Another name of the very same file loses nothing when renamed onto.
        std::fs::remove_file(dst.join("grad")).unwrap();
        std::fs::hard_link(src.join("grad"), dst.join("grad")).unwrap();
        assert!(plan_move(&t, &dst).refusal().is_none());
    }

    fn job(hash: &str, dest: &std::path::Path) -> crate::store::Job {
        crate::store::Job {
            id: "j1".into(),
            kind: "graduate".into(),
            state: "running".into(),
            info_hash: hash.into(),
            params: serde_json::json!({
                "from_engine": "race", "to_engine": "hoard", "to_category": "",
                "save_path": dest.display().to_string(),
            })
            .to_string(),
            progress_bytes: 0,
            total_bytes: 16384,
            error: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// The torrent arrives with its own state: the stopped flag and an edited
    /// tracker list survive the move, and the data is where the record says.
    #[test]
    fn a_graduation_carries_the_torrent_as_it_was() {
        let (s, hash, src, dst) = setup("grad-ok");
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        let race = s.state.engines.get("race").unwrap();
        race.manager.set_trackers(&ih, vec![vec!["https://edited.example/announce".into()]]).unwrap();

        graduate(&s.state, &job(&hash, &dst)).expect("graduated");

        assert!(race.manager.get(&ih).is_none());
        let t = s.state.engines.get("hoard").unwrap().manager.get(&ih).expect("in hoard");
        assert_eq!(*t.live_trackers.read(), vec![vec!["https://edited.example/announce".to_string()]]);
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "a stopped torrent stays stopped");
        assert!(dst.join("grad").exists() && !src.join("grad").exists());
    }

    /// ⭐ A move inside the engine keeps the torrent as it was. 4.4 re-added
    /// it in seed mode: an incomplete torrent came out "complete", its tracker
    /// edits were gone and its counters were back to zero.
    #[test]
    fn a_move_carries_the_torrent_as_it_was() {
        let s = state_from("move-keeps", "");
        let src = s.dir.join("race-data");
        let dst = s.dir.join("elsewhere");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("grad"), vec![7u8; 16384]).unwrap();
        // NOT seed mode, and never checked: incomplete. Straight into the
        // engine, because the API's add starts a recheck on the runtime.
        let blob = torrent_bytes("grad");
        let root = src.display().to_string();
        let race = s.state.engines.get("race").unwrap();
        let (ih, _) = race.manager.add_torrent_bytes(&blob, &root, true, false).expect("added");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        s.state.store.lock().unwrap().insert_torrent(&hash, "race", &blob, &root, "", 0.0, true, "").unwrap();
        race.manager.set_trackers(&ih, vec![vec!["https://edited.example/announce".into()]]).unwrap();
        race.manager.get(&ih).unwrap().restore_lifetime(5_000, 700);

        let mut j = job(&hash, &dst);
        j.kind = "move_data".into();
        j.params = serde_json::json!({
            "engine": "race", "category": "", "save_path": dst.display().to_string(),
        })
        .to_string();
        move_data(&s.state, &j).expect("moved");

        let t = race.manager.get(&ih).expect("still in race");
        assert!(!t.seed_mode, "an incomplete torrent is not made a seed by moving it");
        assert_eq!(*t.live_trackers.read(), vec![vec!["https://edited.example/announce".to_string()]]);
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "a stopped torrent stays stopped");
        assert_eq!(t.total_uploaded.load(std::sync::atomic::Ordering::Relaxed), 5_000);
        assert_eq!(*t.save_path.read(), dst);
        assert!(dst.join("grad").exists() && !src.join("grad").exists());
    }

    /// ⭐ A move stopped between taking the torrent out of its engine and
    /// putting it back is finished when its job runs again: the torrent is
    /// back in its engine, at the new place, as it was.
    #[test]
    fn a_move_interrupted_after_leaving_its_engine_is_finished_when_run_again() {
        let s = state_from("move-interrupted", "");
        let src = s.dir.join("race-data");
        let dst = s.dir.join("elsewhere");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("grad"), vec![7u8; 16384]).unwrap();
        let blob = torrent_bytes("grad");
        let root = src.display().to_string();
        let race = s.state.engines.get("race").unwrap();
        let (ih, _) = race.manager.add_torrent_bytes(&blob, &root, true, false).expect("added");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        race.manager.get(&ih).unwrap().restore_lifetime(5_000, 700);
        let params = serde_json::json!({
            "engine": "race", "category": "", "save_path": dst.display().to_string(),
        })
        .to_string();
        let running = {
            let store = s.state.store.lock().unwrap();
            store.insert_torrent(&hash, "race", &blob, &root, "", 0.0, true, "").unwrap();
            store.create_job("move_data", &hash, &params, 16384).unwrap();
            store.claim_next_job().expect("claimed")
        };
        STOP_AFTER_LEAVING_SOURCE.with(|c| c.set(true));
        move_data(&s.state, &running).expect_err("stopped where a crash would");
        assert!(race.manager.get(&ih).is_none(), "out of its engine");
        let again = {
            let store = s.state.store.lock().unwrap();
            store.requeue_running_jobs();
            store.claim_next_job().expect("claimed again")
        };

        move_data(&s.state, &again).expect("finished");
        let t = race.manager.get(&ih).expect("back in race");
        assert_eq!(*t.save_path.read(), dst);
        assert_eq!(t.total_uploaded.load(std::sync::atomic::Ordering::Relaxed), 5_000);
        assert!(dst.join("grad").exists() && !src.join("grad").exists());
    }

    /// ⭐ A file that appears at a graduation's destination while it is
    /// stopped is somebody else's: the resumed run neither overwrites it nor
    /// loses the original, and the torrent goes back where it was.
    #[test]
    fn a_resumed_graduation_leaves_a_file_that_appeared_at_its_destination() {
        let (s, hash, src, dst, again) = interrupted("grad-collision");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("grad"), b"somebody else's grad").unwrap();

        let err = graduate(&s.state, &again).expect_err("given up");
        assert!(err.contains("already holds another file"), "{err}");
        assert_eq!(std::fs::read(dst.join("grad")).unwrap(), b"somebody else's grad");
        assert_eq!(std::fs::read(src.join("grad")).unwrap(), vec![7u8; 16384]);
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        let back = s.state.engines.get("race").unwrap().manager.get(&ih).expect("back in race");
        assert_eq!(*back.save_path.read(), src);
    }

    /// The same for a move: the original is not deleted on the strength of a
    /// destination that is not its copy.
    #[test]
    fn a_resumed_move_leaves_a_file_that_appeared_at_its_destination() {
        resumed_move_meets(b"somebody else's grad", "move-collision");
    }

    /// ⭐ An empty file, or one holding the original's first bytes, is not a
    /// move's copy: a move copies everything before it takes the torrent out
    /// of its engine. Taking one for the copy deleted the original.
    #[test]
    fn a_resumed_move_does_not_take_an_empty_or_short_file_for_its_copy() {
        resumed_move_meets(b"", "move-empty");
        resumed_move_meets(&[7u8; 100], "move-short");
    }

    fn resumed_move_meets(foreign: &[u8], tag: &str) {
        let s = state_from(tag, "");
        let src = s.dir.join("race-data");
        let dst = s.dir.join("elsewhere");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("grad"), vec![7u8; 16384]).unwrap();
        let blob = torrent_bytes("grad");
        let root = src.display().to_string();
        let race = s.state.engines.get("race").unwrap();
        let (ih, _) = race.manager.add_torrent_bytes(&blob, &root, true, false).expect("added");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        let params = serde_json::json!({
            "engine": "race", "category": "", "save_path": dst.display().to_string(),
        })
        .to_string();
        let running = {
            let store = s.state.store.lock().unwrap();
            store.insert_torrent(&hash, "race", &blob, &root, "", 0.0, true, "").unwrap();
            store.create_job("move_data", &hash, &params, 16384).unwrap();
            store.claim_next_job().expect("claimed")
        };
        STOP_AFTER_LEAVING_SOURCE.with(|c| c.set(true));
        move_data(&s.state, &running).expect_err("stopped where a crash would");
        let again = {
            let store = s.state.store.lock().unwrap();
            store.requeue_running_jobs();
            store.claim_next_job().expect("claimed again")
        };
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("grad"), foreign).unwrap();

        let err = move_data(&s.state, &again).expect_err("given up");
        assert!(err.contains("already holds another file"), "{err}");
        assert_eq!(std::fs::read(src.join("grad")).unwrap(), vec![7u8; 16384], "the original is kept");
        assert_eq!(std::fs::read(dst.join("grad")).unwrap(), foreign, "and so is the other file");
        let t = race.manager.get(&ih).expect("back in race");
        assert_eq!(*t.save_path.read(), src);
    }

    /// ⭐ The drain graduated onto whatever sat at the destination: rename(2)
    /// replaced another torrent's file of the same name without a word.
    #[test]
    fn a_graduation_onto_a_taken_path_is_refused() {
        let (s, hash, src, dst) = setup("grad-taken");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("grad"), b"somebody else's grad").unwrap();

        let err = graduate(&s.state, &job(&hash, &dst)).expect_err("refused");
        assert!(err.contains("already exist"), "{err}");
        assert_eq!(std::fs::read(dst.join("grad")).unwrap(), b"somebody else's grad");
        assert!(src.join("grad").exists());
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        assert!(s.state.engines.get("race").unwrap().manager.get(&ih).is_some(), "still in race");
    }

    /// A graduation stopped right after its torrent left the source engine,
    /// then the process back up: its job is queued again and claimed.
    fn interrupted(tag: &str) -> (TestState, String, std::path::PathBuf, std::path::PathBuf, crate::store::Job) {
        let (s, hash, src, dst) = setup(tag);
        let params = job(&hash, &dst).params;
        let running = {
            let store = s.state.store.lock().unwrap();
            store.create_job("graduate", &hash, &params, 16384).unwrap();
            store.claim_next_job().expect("claimed")
        };
        STOP_AFTER_LEAVING_SOURCE.with(|c| c.set(true));
        graduate(&s.state, &running).expect_err("stopped where a crash would");
        let again = {
            let store = s.state.store.lock().unwrap();
            assert_eq!(store.requeue_running_jobs(), 1, "the restart queues it again");
            store.claim_next_job().expect("claimed again")
        };
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        assert!(s.state.engines.get("race").unwrap().manager.get(&ih).is_none());
        assert!(s.state.engines.get("hoard").unwrap().manager.get(&ih).is_none(), "in neither engine");
        (s, hash, src, dst, again)
    }

    fn graduated_to_hoard(s: &TestState, hash: &str, dst: &std::path::Path) {
        let ih = typhon_engine::torrent::hex_decode(hash).unwrap();
        let t = s.state.engines.get("hoard").unwrap().manager.get(&ih).expect("in hoard");
        assert_eq!(*t.save_path.read(), dst);
        assert!(t.is_paused.load(std::sync::atomic::Ordering::Relaxed), "a stopped torrent stays stopped");
        let store = s.state.store.lock().unwrap();
        assert_eq!(store.sessions_of(hash), vec!["hoard".to_string()]);
        assert!(store.torrent_blob(hash).unwrap().is_some(), "with its metainfo");
    }

    /// ⭐ Stopped between leaving its engine and reaching the other -- a kill,
    /// an OOM, a power cut in the middle of a copy -- the graduation is
    /// finished when its job runs again, instead of failing with the torrent
    /// in no engine and its record gone.
    #[test]
    fn a_graduation_interrupted_after_leaving_its_engine_is_finished_when_run_again() {
        let (s, hash, src, dst, again) = interrupted("grad-interrupted");
        graduate(&s.state, &again).expect("finished");
        graduated_to_hoard(&s, &hash, &dst);
        assert!(dst.join("grad").exists() && !src.join("grad").exists());
    }

    /// The files that had already moved before the interruption stay where
    /// they went; the run finishes from there.
    #[test]
    fn an_interrupted_graduation_keeps_the_files_it_had_moved() {
        let (s, hash, src, dst, again) = interrupted("grad-half-moved");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::rename(src.join("grad"), dst.join("grad")).unwrap();
        graduate(&s.state, &again).expect("finished");
        graduated_to_hoard(&s, &hash, &dst);
        assert_eq!(std::fs::read(dst.join("grad")).unwrap(), vec![7u8; 16384]);
    }

    /// ⭐ A target that refuses puts everything back: the files return and the
    /// source engine holds the torrent again. 4.3 left it in no engine.
    #[test]
    fn a_refused_graduation_is_rolled_back() {
        let (s, hash, src, dst) = setup("grad-refused");
        let ih = typhon_engine::torrent::hex_decode(&hash).unwrap();
        // The target already holds this torrent, so its adoption is refused.
        let hoard = s.state.engines.get("hoard").unwrap();
        let other = s.dir.join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        hoard.manager.add_torrent_bytes(&torrent_bytes("grad"), &other.display().to_string(), true, true).unwrap();

        let err = graduate(&s.state, &job(&hash, &dst)).expect_err("refused");
        assert!(err.contains("rolled back"), "{err}");
        assert!(src.join("grad").exists(), "the file came back");
        assert!(!dst.join("grad").exists());
        let back = s.state.engines.get("race").unwrap().manager.get(&ih).expect("back in race");
        assert_eq!(*back.save_path.read(), src);
    }

    /// A move interrupted after its torrent left the engine, with the job's
    /// record in its params, picked up again.
    fn interrupted_move(
        tag: &str,
        dest_is_source: bool,
    ) -> (TestState, typhon_engine::torrent::meta::InfoHash, std::path::PathBuf, std::path::PathBuf, crate::store::Job) {
        let s = state_from(tag, "");
        let src = s.dir.join("race-data");
        let dst = if dest_is_source { src.clone() } else { s.dir.join("elsewhere") };
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("grad"), vec![7u8; 16384]).unwrap();
        let blob = torrent_bytes("grad");
        let root = src.display().to_string();
        let race = s.state.engines.get("race").unwrap();
        let (ih, _) = race.manager.add_torrent_bytes(&blob, &root, true, false).expect("added");
        let hash = typhon_engine::torrent::hex_encode(&ih);
        let original = race.manager.export_state(&ih).expect("exported");
        race.manager.remove_torrent(&ih, true).expect("out of its engine");
        let params = serde_json::json!({
            "engine": "race", "category": "", "save_path": dst.display().to_string(),
            "record": serde_json::to_value(&original).unwrap(),
        })
        .to_string();
        let again = {
            let store = s.state.store.lock().unwrap();
            store.insert_torrent(&hash, "race", &blob, &root, "", 0.0, true, "").unwrap();
            store.create_job("move_data", &hash, &params, 16384).unwrap();
            store.claim_next_job().expect("claimed")
        };
        (s, ih, src, dst, again)
    }

    /// A resumed move whose destination is the very path it started from has
    /// nothing to copy and nothing to drop: the file is the torrent's only one.
    #[test]
    fn a_resumed_move_onto_its_own_path_keeps_the_file() {
        let (s, ih, src, dst, again) = interrupted_move("move-same-path", true);
        assert_eq!(src, dst);

        move_data(&s.state, &again).expect("finished");

        assert_eq!(std::fs::read(src.join("grad")).unwrap(), vec![7u8; 16384], "the only copy stays");
        let t = s.state.engines.get("race").unwrap().manager.get(&ih).expect("back in race");
        assert_eq!(*t.save_path.read(), src);
    }
}
