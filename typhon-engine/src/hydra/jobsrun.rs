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
    })
    .to_string();
    let store = match state.store.lock() {
        Ok(s) => s,
        Err(e) => e.into_inner(),
    };
    if store.job_pending_for("graduate", hash) {
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

fn run_job(state: &AppState, job: &crate::store::Job) -> Result<(), String> {
    match job.kind.as_str() {
        "graduate" => graduate(state, job),
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
        crate::jobs::run_move(from_path, to_path)
            .map_err(|e| format!("moving {}: {e}", from_path.display()))?;
        done += size;
        let store = match state.store.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        let _ = store.job_progress(&job.id, done);
    }

    let cfg = state.cfg();
    let torrent_file = std::path::Path::new(&cfg.daemon.data_dir)
        .join("uploads")
        .join(format!("{hash}.torrent"));

    // ⚠ The file named after a hash does not necessarily CONTAIN that torrent.
    // The store keeps the metainfo twice -- a blob and a file -- and the file
    // has been seen holding someone else's torrent. Measured on the bench:
    // uploads/cae7a364....torrent held GHOST_D, so the graduation re-added the
    // wrong torrent, pointed it at the data of the right one, and lost the
    // original from every engine. The blob is the authority; the file is
    // rewritten from it here rather than trusted.
    {
        let blob = {
            let store = match state.store.lock() {
                Ok(s) => s,
                Err(e) => e.into_inner(),
            };
            store.torrent_blob(&hash).ok().flatten()
        };
        match blob {
            Some(bytes) => {
                if let Some(dir) = torrent_file.parent() {
                    std::fs::create_dir_all(dir).ok();
                }
                std::fs::write(&torrent_file, &bytes)
                    .map_err(|e| format!("cannot write the metainfo back: {e}"))?;
            }
            None if !torrent_file.exists() => {
                return Err("no metainfo in the store and none on disk; the data moved but nothing can re-add it".into());
            }
            None => {}
        }
    }

    let dst = state.engines.get(&to).ok_or("target engine is gone")?;
    // seed_mode: the payload was verified where it came from and the move
    // copied it byte for byte. A recheck here would read every byte again.
    let (added_ih, _name) = dst
        .manager
        .add_torrent(&torrent_file.to_string_lossy(), &dest_root, false, true)
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
            let _ = store.set_category(&hash, &category);
        }
    }
    tracing::info!(hash = %hash, from = %from, to = %to, moved_bytes = done, seed_secs = seeded,
                   "graduated");
    Ok(())
}
