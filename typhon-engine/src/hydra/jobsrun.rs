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
    let t = src.manager.get(&ih).ok_or("the source engine no longer holds it")?;

    // Captured BEFORE the torrent leaves the engine: re-adding builds a fresh
    // state, and the seed counter would restart at zero -- on the very move
    // that a 48-hour obligation is being carried across.
    let now = typhon_engine::torrent::meta::now_secs();
    let seeded = t.seed_time_now(now);
    let old_root = t.save_path.read().clone();
    let multi = t.meta.multi_file;
    let name = t.meta.name.clone();
    let files: Vec<(std::path::PathBuf, std::path::PathBuf)> = t
        .meta
        .files
        .iter()
        .map(|f| {
            let rel: std::path::PathBuf = if multi {
                std::path::Path::new(&name).join(&f.path)
            } else {
                std::path::PathBuf::from(&f.path)
            };
            (old_root.join(&rel), std::path::Path::new(&dest_root).join(&rel))
        })
        .collect();

    // Out of the engine first, KEEPING the data: moving files under a running
    // torrent is how a seed starts serving bytes that are no longer there.
    src.manager
        .remove_torrent(&ih, true)
        .map_err(|e| format!("the source engine refused to release it: {e}"))?;
    src.announce_cache.forget(&hash);

    let mut done: i64 = 0;
    for (from_path, to_path) in &files {
        if !from_path.exists() {
            continue;
        }
        let size = std::fs::metadata(from_path).map(|m| m.len()).unwrap_or(0) as i64;
        crate::jobs::run_move_allowing(from_path, to_path, allow)
            .map_err(|e| format!("moving {}: {e}", from_path.display()))?;
        done += size;
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.job_progress(&job.id, done);
    }

    // The blob keyed by this hash IS this torrent, so there is nothing left to
    // guard against here. The file under uploads/ named after a hash did not
    // necessarily CONTAIN that torrent -- measured on the bench,
    // uploads/cae7a364....torrent held GHOST_D, so the graduation re-added the
    // wrong torrent, pointed it at the data of the right one, and lost the
    // original from every engine. That whole failure mode goes away with the
    // file: the metainfo is fetched by key and handed straight to the engine.
    let metainfo = {
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        store.torrent_blob(&hash).ok().flatten()
    }
    .ok_or("no metainfo in the store; the data moved but nothing can re-add it")?;

    let dst = state.engines.get(&to).ok_or("target engine is gone")?;
    // seed_mode: the payload was verified where it came from and the move
    // copied it byte for byte. A recheck here would read every byte again.
    let (added_ih, _name) = dst
        .manager
        .add_torrent_bytes(&metainfo, &dest_root, false, true)
        .map_err(|e| format!("the target engine refused it, and the data has already moved: {e}"))?;

    // The hash it actually added, against the one asked for. Without this the
    // job adds whatever the file happened to hold, reports success, and leaves
    // the real torrent in no engine at all.
    if added_ih != ih {
        let _ = dst.manager.remove_torrent(&added_ih, true);
        return Err(format!(
            "the metainfo for {hash} describes {} instead; nothing was re-added",
            typhon_engine::torrent::hex_encode(&added_ih)
        ));
    }
    // And that it is really there. `if let Some` with no else is how the last
    // version declared this job a success while the torrent was gone.
    let nt = dst
        .manager
        .get(&ih)
        .ok_or("the target engine accepted it and does not hold it")?;
    nt.seed_secs
        .store(seeded, std::sync::atomic::Ordering::Relaxed);
    nt.fold_seed_time(typhon_engine::torrent::meta::now_secs());
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
    let (hashes, _) = scan_shared(state, t, &plan.old_root, &mine);
    plan.shared_with = hashes;
    plan
}

/// The files of `t` that another torrent -- in any engine -- reads too.
///
/// What a deletion must leave on disk: two torrents pointed at one path (a
/// cross-seed, an upload seeded from the files it was made from) read the same
/// bytes, and deleting them with the first torrent leaves the other one
/// announcing an empty folder, complete by its own account.
pub fn files_read_by_others(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
) -> std::collections::HashSet<std::path::PathBuf> {
    let root = t.save_path.read().clone();
    let mine: std::collections::HashSet<std::path::PathBuf> =
        rel_paths(t).into_iter().map(|p| root.join(p)).collect();
    scan_shared(state, t, &root, &mine).1
}

/// Every other torrent that reads one of `mine` (files of `t`, under `root`),
/// and which of those files they read.
fn scan_shared(
    state: &AppState,
    t: &typhon_engine::torrent::meta::TorrentState,
    root: &std::path::Path,
    mine: &std::collections::HashSet<std::path::PathBuf>,
) -> (Vec<String>, std::collections::HashSet<std::path::PathBuf>) {
    let my_top = top_entry(t);
    // The files of `mine` that `other` reads, or None when it reads none.
    let shared_by = |other: &typhon_engine::torrent::meta::TorrentState| -> Option<Vec<std::path::PathBuf>> {
        if other.meta.info_hash == t.meta.info_hash {
            return None;
        }
        // Only a torrent whose root is on the same branch can name the
        // same files: a prefix test first, the file list only then.
        // Read in place: a PathBuf clone per torrent would be an allocation
        // for the 99.99% that fail the test.
        let r = {
            let r = other.save_path.read();
            if !(root.starts_with(&*r) || r.starts_with(root)) {
                return None;
            }
            // Same folder, different top-level entry: no file can be
            // shared, since every path of each starts with its own entry.
            // A whole category can share one folder, and there the folder
            // test lets nearly everything through.
            if *r == *root {
                if let (Some(a), Some(b)) = (my_top, top_entry(other)) {
                    if a != b {
                        return None;
                    }
                }
            }
            r.clone()
        };
        let hit: Vec<std::path::PathBuf> =
            rel_paths(other).into_iter().map(|p| r.join(p)).filter(|p| mine.contains(p)).collect();
        if hit.is_empty() { None } else { Some(hit) }
    };
    let pair = |o: &std::sync::Arc<typhon_engine::torrent::meta::TorrentState>| {
        shared_by(o).map(|files| (typhon_engine::torrent::hex_encode(&o.meta.info_hash), files))
    };
    let mut hashes: Vec<String> = Vec::new();
    let mut files: std::collections::HashSet<std::path::PathBuf> = std::collections::HashSet::new();
    // Over every torrent of every engine, in slices on their own threads: the
    // prefix test alone is a second per move at a million torrents, and a
    // move of a hundred torrents runs it a hundred times.
    for engine in state.engines.engines().iter() {
        let all = engine.manager.all();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 16);
        let found: Vec<Vec<(String, Vec<std::path::PathBuf>)>> = if threads == 1 || all.len() < SHARED_SCAN_PARALLEL_FROM {
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
        for (h, fs) in found.into_iter().flatten() {
            if !hashes.contains(&h) {
                hashes.push(h);
            }
            files.extend(fs);
        }
    }
    (hashes, files)
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
    let files = t
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
    MovePlan { old_root, new_root: new_root.to_path_buf(), files, unsafe_path, shared_with: Vec::new() }
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
    old_root: &std::path::Path,
    paused: bool,
    renamed: &[(std::path::PathBuf, std::path::PathBuf)],
    copied: &[std::path::PathBuf],
) -> String {
    let mut problems = Vec::new();
    for (from, to) in renamed.iter().rev() {
        if let Err(e) = std::fs::rename(to, from) {
            problems.push(format!("could not put {} back: {e}", from.display()));
        }
    }
    for c in copied {
        let _ = std::fs::remove_file(c);
    }
    if let Err(e) = engine.manager.add_torrent_bytes(metainfo, &old_root.to_string_lossy(), paused, true) {
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
///     renamed, and it is re-added at the new root without a recheck;
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
    let t = engine.manager.get(&ih).ok_or("the engine no longer holds it")?;
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

    // 2. Stop, rename, re-add.
    let now = typhon_engine::torrent::meta::now_secs();
    let seeded = t.seed_time_now(now);
    let paused = t.is_paused.load(std::sync::atomic::Ordering::Relaxed);
    drop(t);
    if let Err(e) = engine.manager.remove_torrent(&ih, true) {
        for c in &copied {
            let _ = std::fs::remove_file(c);
        }
        return Err(format!("the engine refused to release it: {e}; nothing moved"));
    }
    engine.announce_cache.forget(&hash);

    let mut renamed: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for f in plan.files.iter().filter(|f| f.present && f.rename) {
        let r = f
            .to
            .parent()
            .map(std::fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|_| std::fs::rename(&f.from, &f.to));
        if let Err(e) = r {
            let undo = roll_back(engine, &metainfo, &plan.old_root, paused, &renamed, &copied);
            return Err(format!("renaming {}: {e}; {undo}", f.from.display()));
        }
        renamed.push((f.from.clone(), f.to.clone()));
        done += f.size as i64;
        progress(done);
    }

    let new_root = plan.new_root.to_string_lossy().to_string();
    match engine.manager.add_torrent_bytes(&metainfo, &new_root, paused, true) {
        Ok((added, _)) if added == ih => {}
        Ok((added, _)) => {
            let _ = engine.manager.remove_torrent(&added, true);
            let undo = roll_back(engine, &metainfo, &plan.old_root, paused, &renamed, &copied);
            return Err(format!(
                "the metainfo for {hash} describes {} instead; {undo}",
                typhon_engine::torrent::hex_encode(&added)
            ));
        }
        Err(e) => {
            let undo = roll_back(engine, &metainfo, &plan.old_root, paused, &renamed, &copied);
            return Err(format!("the engine refused it at {new_root}: {e}; {undo}"));
        }
    }
    if let Some(nt) = engine.manager.get(&ih) {
        nt.seed_secs.store(seeded, std::sync::atomic::Ordering::Relaxed);
        nt.fold_seed_time(typhon_engine::torrent::meta::now_secs());
    }
    set_labels(Some(&new_root));

    // 3. The originals of what was copied. The torrent reads the new ones now.
    for f in plan.files.iter().filter(|f| f.present && !f.rename) {
        if let Err(e) = std::fs::remove_file(&f.from) {
            tracing::warn!(file = %f.from.display(), error = %e, "moved, but the original could not be removed");
        }
    }
    prune_empty_dirs(&plan);
    tracing::info!(hash = %hash, engine = %engine_id, from = %plan.old_root.display(),
                   to = %new_root, copied_bytes = need, "moved");
    Ok(())
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
